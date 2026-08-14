//! End-to-end tests against the real binary, the real `uv`, and the real
//! kernel sandbox.
//!
//! These assert the properties senv exists to provide, so they deliberately do
//! not mock the boundary: a test that stubs out Landlock would pass on a host
//! where senv protects nothing.
//!
//! They skip — loudly — when the host cannot enforce the policy under test
//! (`senv doctor` explains why on such a host), because a red suite on a
//! Landlock-less CI runner teaches people to ignore the suite.

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_senv");

/// One isolated project, with senv's state and caches redirected into the same
/// temp dir so a test can never touch the developer's real environments.
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new(manifest: Option<&str>) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).expect("mkdir");
        if let Some(text) = manifest {
            std::fs::write(root.join("pyproject.toml"), text).expect("write manifest");
        }
        Fixture {
            state: tmp.path().join("state"),
            cache: tmp.path().join("cache"),
            root,
            _tmp: tmp,
        }
    }

    fn senv(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(&self.root)
            .env("SENV_STATE_DIR", &self.state)
            .env("SENV_CACHE_DIR", &self.cache)
            // Deterministic output regardless of the developer's terminal.
            .env("NO_COLOR", "1")
            .env_remove("VIRTUAL_ENV")
            .output()
            .expect("senv should be runnable")
    }

    /// Run python inside the run boundary and return its combined output.
    fn run_python(&self, script: &str) -> (Output, String) {
        let out = self.senv(&["run", "python", "-c", script]);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out, text)
    }

    /// Build the environment, failing the test with uv's own output if it
    /// does not work — a bare `assert!(status.success())` here would hide the
    /// one message that explains why.
    fn sync(&self) {
        let out = self.senv(&["sync"]);
        assert!(
            out.status.success(),
            "senv sync failed:\n{}",
            combined(&out)
        );
    }

    fn state_dir(&self) -> PathBuf {
        // One project per fixture, so the single entry is ours.
        let projects = self.state.join("projects");
        std::fs::read_dir(&projects)
            .expect("state dir")
            .flatten()
            .next()
            .expect("one project state dir")
            .path()
    }
}

const DEMO: &str = "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
                    requires-python = \">=3.9\"\ndependencies = [\"idna\"]\n";

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Can this host run the run-phase boundary (Landlock/Seatbelt + namespaces)?
fn can_confine() -> bool {
    let caps = h5i_sandbox::sandbox::capabilities_report();
    caps.claims
        .iter()
        .any(|c| c.claim == "process" && c.satisfiable && c.runnable.unwrap_or(false))
}

/// Can this host enforce an egress allowlist, which the install phase needs?
fn can_install() -> bool {
    can_confine() && h5i_sandbox::supervisor::probe().usable
}

fn have_uv() -> bool {
    Command::new("uv")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip with an explanation rather than failing on a host that cannot enforce
/// what the test is about.
macro_rules! require {
    ($cond:expr, $why:expr) => {
        if !$cond {
            eprintln!("SKIP: {}", $why);
            return;
        }
    };
}

#[test]
fn doctor_reports_the_host_without_needing_a_project() {
    let fixture = Fixture::new(None);
    let out = fixture.senv(&["doctor"]);
    let text = combined(&out);
    assert!(text.contains("isolation tiers"), "{text}");
    // It must work outside a project — that is when people run it.
    assert!(!text.contains("no Python project"), "{text}");
}

#[test]
fn a_directory_with_no_project_is_reported_clearly() {
    let fixture = Fixture::new(None);
    let out = fixture.senv(&["status"]);
    let text = combined(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("no Python project"), "{text}");
    assert!(
        text.contains("senv init"),
        "the error must name the way forward: {text}"
    );
}

#[test]
fn the_full_lifecycle_works_and_the_project_stays_a_uv_project() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));

    let out = fixture.senv(&["sync"]);
    assert!(out.status.success(), "sync failed: {}", combined(&out));

    let (out, text) = fixture.run_python("import idna; print('ok', idna.__version__)");
    assert!(out.status.success(), "{text}");
    assert!(text.contains("ok 3."), "{text}");

    // senv adds nothing to the project tree but the .venv link, so a teammate
    // without senv still has an ordinary uv project.
    let mut entries: Vec<String> = std::fs::read_dir(&fixture.root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![".venv", "pyproject.toml", "uv.lock"],
        "{entries:?}"
    );
    assert!(
        std::fs::symlink_metadata(fixture.root.join(".venv"))
            .unwrap()
            .file_type()
            .is_symlink(),
        ".venv must be a link into senv's state, not a directory in the project"
    );
}

#[test]
fn the_environment_cannot_be_modified_by_the_code_that_runs_in_it() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    // The guarantee: a package cannot patch itself to persist into later runs.
    let (out, text) = fixture.run_python(
        "import idna, pathlib\n\
         p = pathlib.Path(idna.__file__)\n\
         try:\n    p.write_text('PWNED'); print('WROTE')\n\
         except OSError as e: print('blocked')",
    );
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("blocked"),
        "the environment was writable at run time: {text}"
    );

    // …and reading it obviously still works, or nothing would import.
    let (_, text) = fixture.run_python("import idna; print(len(idna.__file__) > 0)");
    assert!(text.contains("True"), "{text}");
}

#[test]
fn the_network_is_denied_by_default_and_openable_on_purpose() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let probe = "import socket\n\
                 try:\n    socket.create_connection(('example.com', 443), 8); print('CONNECTED')\n\
                 except OSError as e: print('blocked')";

    let (_, text) = fixture.run_python(probe);
    assert!(
        text.contains("blocked"),
        "the run phase had network access: {text}"
    );

    // Opening one host is an explicit, recorded act.
    let out = fixture.senv(&["allow", "example.com"]);
    assert!(out.status.success(), "{}", combined(&out));
    assert!(
        fixture.root.join("senv.toml").is_file(),
        "`senv allow` must record the decision in the project's config"
    );

    let (_, text) = fixture.run_python(probe);
    assert!(
        text.contains("CONNECTED"),
        "the allowed host was still blocked: {text}"
    );

    // And nothing else came with it.
    let (_, text) = fixture.run_python(
        "import socket\n\
         try:\n    socket.create_connection(('pypi.org', 443), 8); print('CONNECTED')\n\
         except OSError as e: print('blocked')",
    );
    assert!(
        text.contains("blocked"),
        "the allowlist leaked to other hosts: {text}"
    );
}

#[test]
fn credentials_on_the_host_are_unreachable() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let (_, text) = fixture.run_python(
        "import pathlib\n\
         h = pathlib.Path.home()\n\
         for name in ['.ssh', '.aws', '.netrc']:\n\
         \x20   p = h / name\n\
         \x20   try:\n\
         \x20       p.read_bytes() if p.is_file() else list(p.iterdir())\n\
         \x20       print('READ', name)\n\
         \x20   except OSError: print('blocked', name)",
    );
    assert!(
        !text.contains("READ ."),
        "a credential path was readable: {text}"
    );
}

#[test]
fn the_exit_code_of_the_command_passes_through() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let (out, _) = fixture.run_python("import sys; sys.exit(42)");
    assert_eq!(
        out.status.code(),
        Some(42),
        "senv must be transparent in CI"
    );

    let (out, _) = fixture.run_python("print('fine')");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn receipts_are_written_outside_everything_the_sandbox_can_write() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();
    fixture.run_python("print('hello')");

    let receipt = fixture.state_dir().join("receipt.jsonl");
    assert!(
        receipt.is_file(),
        "a receipt must exist at {}",
        receipt.display()
    );
    assert!(
        !receipt.starts_with(&fixture.root),
        "a receipt inside the project would be writable by the code it records"
    );

    let text = std::fs::read_to_string(&receipt).unwrap();
    let records: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is JSON"))
        .collect();
    assert!(records.iter().any(|r| r["phase"] == "install"));
    assert!(records.iter().any(|r| r["phase"] == "run"));
    for r in &records {
        assert!(
            r["policy_digest"].as_str().is_some_and(|d| d.len() == 64),
            "{r}"
        );
    }

    // The code under test can see the state directory's path; it must not be
    // able to rewrite the evidence there.
    let (_, text) = fixture.run_python(&format!(
        "import pathlib\n\
         try:\n    pathlib.Path(r'{}').write_text('tampered'); print('WROTE')\n\
         except OSError: print('blocked')",
        receipt.display()
    ));
    assert!(
        text.contains("blocked"),
        "receipts were writable from inside the sandbox: {text}"
    );
}

#[test]
fn status_and_report_describe_the_boundary_in_json() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let out = fixture.senv(&["--json", "status"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(json["environment"]["provenance"], "sandboxed");
    let phases = json["phases"].as_array().expect("phases");
    let run = phases
        .iter()
        .find(|p| p["phase"] == "run")
        .expect("a run phase");
    assert_eq!(run["network"], "denied");
    let install = phases
        .iter()
        .find(|p| p["phase"] == "install")
        .expect("an install phase");
    assert!(
        install["network"].as_str().unwrap().contains("pypi.org"),
        "installs must be scoped to registries: {install}"
    );

    let out = fixture.senv(&["--json", "report"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert!(json["total_commands"].as_u64().unwrap() >= 1);
}

#[test]
fn an_existing_uv_project_is_adopted_without_being_rewritten() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));

    // Build an environment the way a user would have before finding senv.
    let uv = Command::new("uv")
        .args(["venv", ".venv"])
        .current_dir(&fixture.root)
        .output()
        .expect("uv venv");
    assert!(
        uv.status.success(),
        "{}",
        String::from_utf8_lossy(&uv.stderr)
    );
    let manifest_before = std::fs::read_to_string(fixture.root.join("pyproject.toml")).unwrap();

    let out = fixture.senv(&["init"]);
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("adopted"),
        "an existing manifest must be adopted, not replaced: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("pyproject.toml")).unwrap(),
        manifest_before,
        "senv must not rewrite the user's manifest"
    );

    // An environment senv did not create is never deleted silently…
    assert!(
        std::fs::symlink_metadata(fixture.root.join(".venv"))
            .unwrap()
            .file_type()
            .is_dir(),
        "the pre-existing .venv directory must be left in place"
    );
    assert!(
        text.contains("left it alone"),
        "and the user must be told: {text}"
    );

    // …until asked, and then the provenance is honest about what happened.
    let out = fixture.senv(&["init", "--replace-venv", "--no-sync"]);
    assert!(out.status.success(), "{}", combined(&out));
    assert!(
        std::fs::symlink_metadata(fixture.root.join(".venv"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn a_misspelled_config_key_is_refused_before_anything_runs() {
    let fixture = Fixture::new(Some(DEMO));
    std::fs::write(fixture.root.join("senv.toml"), "[run]\nnett = \"host\"\n").unwrap();

    let out = fixture.senv(&["status"]);
    let text = combined(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("nett"), "the error must name the key: {text}");
}

#[test]
fn an_allowlist_the_tier_cannot_enforce_is_refused_not_ignored() {
    let fixture = Fixture::new(Some(DEMO));
    std::fs::write(
        fixture.root.join("senv.toml"),
        "[env]\nisolation = \"process\"\n[run]\nnet = [\"api.example.com\"]\n",
    )
    .unwrap();

    let out = fixture.senv(&["--json", "status"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let run = json["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["phase"] == "run")
        .expect("a run phase");
    let reason = run["unavailable"]
        .as_str()
        .expect("the phase must report itself unusable");
    assert!(reason.contains("all-or-nothing"), "{reason}");
}

#[test]
fn the_unconfined_tier_cannot_be_configured() {
    let fixture = Fixture::new(Some(DEMO));
    std::fs::write(
        fixture.root.join("senv.toml"),
        "[env]\nisolation = \"workspace\"\n",
    )
    .unwrap();

    let out = fixture.senv(&["status"]);
    let text = combined(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("no unconfined execution path"), "{text}");
}

#[test]
fn a_command_that_is_not_installed_names_the_environment_and_the_fix() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let out = fixture.senv(&["run", "pytest"]);
    let text = combined(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("not available"), "{text}");
    assert!(
        text.contains("senv add pytest"),
        "the fix must be copy-pasteable: {text}"
    );

    // A system tool on the sandbox's own PATH is not senv's business to
    // refuse. A hardcoded ten-name list used to turn `senv run node` into
    // "add it with `senv add node`", which is nonsense for a non-Python tool.
    let out = fixture.senv(&["run", "printenv", "HOME"]);
    assert!(
        out.status.success(),
        "a system tool on PATH must run: {}",
        combined(&out)
    );
}

#[test]
fn adding_a_dependency_resolves_away_from_the_source_tree() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
         requires-python = \">=3.9\"\ndependencies = []\n",
    ));
    // A file that a build backend running during resolution must never see.
    std::fs::create_dir_all(fixture.root.join("src")).unwrap();
    std::fs::write(fixture.root.join("src/secret.py"), "KEY = 'hunter2'\n").unwrap();

    let out = fixture.senv(&["--json", "add", "idna"]);
    assert!(out.status.success(), "{}", combined(&out));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(
        json["staged"], true,
        "a static manifest must resolve in a staging copy"
    );

    let manifest = std::fs::read_to_string(fixture.root.join("pyproject.toml")).unwrap();
    assert!(
        manifest.contains("idna"),
        "the dependency must be written back: {manifest}"
    );
    assert!(fixture.root.join("uv.lock").is_file());

    // The staging copy holds manifests only.
    let stage = fixture.state_dir().join("stage");
    if stage.exists() {
        assert!(
            !stage.join("src").exists(),
            "the source tree must not be staged"
        );
    }
}

#[test]
fn a_project_that_cannot_be_staged_says_so_instead_of_widening_silently() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    // Dynamic metadata forces resolution against the real tree.
    let fixture = Fixture::new(Some(
        "[project]\nname = \"demo\"\ndynamic = [\"version\"]\n\
         requires-python = \">=3.9\"\ndependencies = []\n\
         [build-system]\nrequires = [\"setuptools>=61\"]\n\
         build-backend = \"setuptools.build_meta\"\n",
    ));
    std::fs::write(
        fixture.root.join("setup.py"),
        "from setuptools import setup\nsetup(version='0.1.0')\n",
    )
    .unwrap();

    let out = fixture.senv(&["lock"]);
    let text = combined(&out);
    // Whether the lock itself succeeds depends on the backend; what must always
    // happen is that the wider policy is announced rather than assumed.
    assert!(
        text.contains("dynamic") || text.contains("in-place") || text.contains("staging"),
        "the decision to resolve in-place must be explained: {text}"
    );
}

#[test]
fn gc_reports_before_it_deletes() {
    let fixture = Fixture::new(Some(DEMO));
    // Create state for a project directory that no longer exists.
    let orphan = fixture.state.join("projects").join("gone-000000000000");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(
        orphan.join("state.json"),
        serde_json::json!({
            "version": 1,
            "project_root": "/nonexistent/gone",
        })
        .to_string(),
    )
    .unwrap();

    let out = fixture.senv(&["gc"]);
    let text = combined(&out);
    assert!(text.contains("gone-000000000000"), "{text}");
    assert!(
        text.contains("--prune"),
        "a dry run must say how to act on it: {text}"
    );
    assert!(orphan.exists(), "gc must not delete without --prune");

    let out = fixture.senv(&["gc", "--prune"]);
    assert!(out.status.success(), "{}", combined(&out));
    assert!(!orphan.exists(), "--prune must actually remove it");
}

/// The one test that would notice senv silently losing its boundary: run a
/// command under a policy and confirm the digest is stable and recorded.
#[test]
fn the_enforced_policy_is_pinned_and_recorded() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let digest_of = |f: &Fixture| -> String {
        let out = f.senv(&["--json", "status"]);
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        json["phases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["phase"] == "run")
            .unwrap()["policy_digest"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let before = digest_of(&fixture);
    assert_eq!(
        before,
        digest_of(&fixture),
        "the same policy must digest the same way"
    );

    // Widening the policy must change the digest — that is what makes the
    // receipt's record of it meaningful.
    assert!(fixture.senv(&["allow", "example.com"]).status.success());
    assert_ne!(
        before,
        digest_of(&fixture),
        "a policy change must move the digest"
    );
}

// ── escalation regressions ──────────────────────────────────────────────────
//
// Each test below was a working exploit during review. The threat model is a
// malicious package that gets ONE execution under `senv run`: it can write
// anywhere the run phase grants write, which includes the project directory —
// and `senv.toml` lives there.

/// A package that rewrites the policy cannot make the next run wider.
#[test]
fn a_package_cannot_widen_the_policy_for_the_next_run() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    // One execution is all the attacker gets.
    let (out, _) = fixture.run_python(
        "open('senv.toml','w').write('[run]\\nnet = \"host\"\\n\
         [run.env]\\npass = [\"AWS_SECRET_ACCESS_KEY\"]\\n')",
    );
    assert!(
        out.status.success(),
        "the write itself is allowed — the project is writable"
    );

    // Every later command that would execute anything is refused, and says
    // exactly what changed.
    let out = fixture.senv(&["run", "python", "-c", "print(1)"]);
    let text = combined(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("grants more than senv recorded"), "{text}");
    assert!(text.contains("net"), "the widening must be named: {text}");
    assert!(text.contains("AWS_SECRET_ACCESS_KEY"), "{text}");
    assert!(text.contains("senv trust"), "and the way forward: {text}");

    // Installing is refused too, not just running.
    assert_eq!(fixture.senv(&["sync"]).status.code(), Some(2));

    // The user, having looked, can accept it — that is the point of the gate.
    assert!(fixture.senv(&["trust"]).status.success());
    assert!(
        fixture
            .senv(&["run", "python", "-c", "print(1)"])
            .status
            .success()
    );
}

/// The `command:` secret source runs on the host, outside the sandbox. A
/// package must not be able to introduce one.
#[test]
fn a_package_cannot_obtain_host_execution_through_a_command_secret() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let marker = fixture.root.parent().unwrap().join("host-rce-marker");
    let script = format!(
        "open('senv.toml','w').write('[env]\\nallow-command-secrets = true\\n\
         [secrets.X]\\nsource = \"command:touch {}\"\\n')",
        marker.display()
    );
    fixture.run_python(&script);

    let out = fixture.senv(&["run", "python", "-c", "print(1)"]);
    assert_eq!(out.status.code(), Some(2), "{}", combined(&out));
    assert!(
        !marker.exists(),
        "a command: secret introduced by a package executed on the host"
    );

    // And the gate is independent of the trust check: without the explicit
    // opt-in the source is refused outright, so it cannot be smuggled in as
    // part of an otherwise reasonable-looking config the user accepts.
    std::fs::write(
        fixture.root.join("senv.toml"),
        format!(
            "[secrets.X]\nsource = \"command:touch {}\"\n",
            marker.display()
        ),
    )
    .unwrap();
    // Every command refuses to load it — including `senv trust`, so the gate
    // cannot be accepted away by a user who did not notice what they were
    // accepting. The config has to be fixed by hand.
    for command in [
        vec!["trust"],
        vec!["run", "python", "-c", "print(1)"],
        vec!["status"],
    ] {
        let out = fixture.senv(&command);
        let text = combined(&out);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{command:?} should refuse: {text}"
        );
        assert!(text.contains("OUTSIDE the sandbox"), "{command:?}: {text}");
    }
    assert!(!marker.exists(), "still must not have run");
}

/// The install phase must not be able to edit your source.
///
/// This was false in the first implementation: h5i grants the working
/// directory read-write implicitly, so listing the project under `fs_read` did
/// not make it read-only, and `senv status` reported a guarantee that was not
/// being enforced.
#[test]
fn a_build_backend_cannot_edit_your_source_during_an_install() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    // `senv uv --` runs an arbitrary uv command inside the install boundary,
    // which is the same confinement a build backend gets.
    let out = fixture.senv(&[
        "uv",
        "--",
        "run",
        "--no-project",
        "--python",
        "/usr/bin/python3",
        "python",
        "-c",
        "open('PWNED_BY_INSTALL','w').write('x')",
    ]);
    assert!(
        !fixture.root.join("PWNED_BY_INSTALL").exists(),
        "the install phase wrote to the project: {}",
        combined(&out)
    );

    // And status must not claim otherwise.
    let out = fixture.senv(&["--json", "status"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let install = json["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["phase"] == "install")
        .expect("an install phase");
    let writable = install["writable"].as_array().expect("writable list");
    let root = fixture.root.display().to_string();
    assert!(
        !writable
            .iter()
            .any(|w| w.as_str().is_some_and(|w| w.starts_with(&root))),
        "status lists the project as writable during installs: {install}"
    );
}

const SETUPTOOLS: &str = "[project]\nname = \"mypkg\"\nversion = \"0.1.0\"\n\
     requires-python = \">=3.9\"\ndependencies = []\n\n\
     [build-system]\nrequires = [\"setuptools>=61\"]\n\
     build-backend = \"setuptools.build_meta\"\n";

const HATCHLING: &str = "[project]\nname = \"hp\"\nversion = \"0.1.0\"\n\
     requires-python = \">=3.9\"\ndependencies = []\n\n\
     [build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n";

/// A setuptools project must still install with the source read-only.
///
/// Enforcing the read-only install broke these: setuptools' `build_editable`
/// creates `*.egg-info` inside the source tree. Telling every setuptools user
/// to set `project-writable = true` would have handed write access to all
/// their dependencies' build backends too, so senv splits the install instead —
/// dependencies first with the source read-only, then the project's own
/// backend, widened only if it actually needs it.
#[test]
fn a_setuptools_project_installs_without_opening_the_source_to_dependencies() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(SETUPTOOLS));
    std::fs::create_dir_all(fixture.root.join("src/mypkg")).unwrap();
    std::fs::write(fixture.root.join("src/mypkg/__init__.py"), "VALUE = 42\n").unwrap();

    let out = fixture.senv(&["sync"]);
    let text = combined(&out);
    assert!(
        out.status.success(),
        "a setuptools project must install: {text}"
    );
    assert!(
        text.contains("dependency build backends did not"),
        "the widening must be announced and scoped: {text}"
    );

    let (out, text) = fixture.run_python("import mypkg; print('V', mypkg.VALUE)");
    assert!(out.status.success(), "{text}");
    assert!(text.contains("V 42"), "{text}");
}

/// A failing *dependency* must never trigger the project-backend retry.
///
/// The retry exists so setuptools can write its own egg-info. If it fired on a
/// dependency's write-denied error instead, senv would re-run a hostile build
/// backend with the source writable — the exact thing the install phase denies.
#[test]
fn a_dependency_build_failure_never_widens_the_install() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );

    // A local sdist whose build backend tries to write into the parent project
    // and reports the same "Permission denied" shape the retry looks for.
    let fixture = Fixture::new(None);
    let dep = fixture.root.parent().unwrap().join("hostile");
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(
        dep.join("pyproject.toml"),
        "[project]\nname = \"hostile\"\nversion = \"0.1.0\"\n\
         [build-system]\nrequires = [\"setuptools>=61\"]\n\
         build-backend = \"setuptools.build_meta\"\n",
    )
    .unwrap();
    let marker = fixture.root.join("WRITTEN_BY_DEPENDENCY");
    std::fs::write(
        dep.join("setup.py"),
        format!(
            "from setuptools import setup\n\
             open(r'{}', 'w').write('x')\n\
             setup()\n",
            marker.display()
        ),
    )
    .unwrap();

    std::fs::write(
        fixture.root.join("pyproject.toml"),
        format!(
            "[project]\nname = \"victim\"\nversion = \"0.1.0\"\n\
             requires-python = \">=3.9\"\ndependencies = [\"hostile\"]\n\n\
             [tool.uv.sources]\nhostile = {{ path = \"{}\" }}\n",
            dep.display()
        ),
    )
    .unwrap();
    // A path dependency lives outside the project, so the install phase has to
    // be told it may read it; otherwise the failure is a missing grant rather
    // than the denied write this test is about.
    std::fs::write(
        fixture.root.join("senv.toml"),
        format!("[install]\nread = [\"{}\"]\n", dep.display()),
    )
    .unwrap();
    assert!(fixture.senv(&["trust"]).status.success());

    let out = fixture.senv(&["sync"]);
    assert!(
        !marker.exists(),
        "a dependency's build backend wrote into the project: {}",
        combined(&out)
    );
}

/// A backend that does not need to write the source must never get to.
#[test]
fn a_hatchling_project_installs_with_the_source_read_only_throughout() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(HATCHLING));
    std::fs::create_dir_all(fixture.root.join("src/hp")).unwrap();
    std::fs::write(fixture.root.join("src/hp/__init__.py"), "X = 1\n").unwrap();

    let out = fixture.senv(&["sync"]);
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        !text.contains("write access to your source"),
        "hatchling needs no widening, so senv must not grant one: {text}"
    );

    let (out, text) = fixture.run_python("import hp; print('X', hp.X)");
    assert!(out.status.success(), "{text}");
    assert!(text.contains("X 1"), "{text}");
}

/// senv must not advertise a limit it does not apply.
///
/// h5i enforces the wall clock in the parent that waits for the child. The
/// interactive path used by `run`/`shell` hands over the terminal and waits
/// without a deadline, so the wall clock is real for installs and absent for
/// runs — while `status` used to print "wall 30m" for both.
#[test]
fn status_does_not_claim_a_wall_clock_it_cannot_enforce() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let out = fixture.senv(&["--json", "status"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let phase = |name: &str| -> serde_json::Value {
        json["phases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["phase"] == name)
            .expect("phase")
            .clone()
    };

    let run_wall = phase("run")["resources"]["wall"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        run_wall.contains("not enforced"),
        "the run phase has no deadline, so status must say so: {run_wall}"
    );
    let install_wall = phase("install")["resources"]["wall"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !install_wall.contains("not enforced"),
        "installs do have a deadline: {install_wall}"
    );
}

/// The CPU-time rlimit is the kernel-enforced backstop that the wall clock is
/// not, so it has to actually work.
#[test]
fn a_cpu_limit_stops_a_runaway_command() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();
    std::fs::write(
        fixture.root.join("senv.toml"),
        "[run.resources]\ncpu = \"3s\"\n",
    )
    .unwrap();
    assert!(fixture.senv(&["trust"]).status.success());

    let start = std::time::Instant::now();
    let (out, _) = fixture.run_python("while True: pass");
    let elapsed = start.elapsed();
    assert!(!out.status.success(), "a spinning command must be killed");
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "the CPU limit did not fire: ran for {elapsed:?}"
    );
}

/// A uv workspace is one project: one lockfile, one environment, resolved from
/// the root whichever member you stand in.
///
/// Rooting senv at the member wrote a `uv.lock` inside the member — a file
/// people commit and plain uv never creates — from a resolution that could not
/// even read the workspace root, because the root is outside the member's
/// grants.
#[test]
fn a_workspace_member_uses_the_workspace_root() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );

    let fixture = Fixture::new(Some(
        "[project]\nname = \"mono\"\nversion = \"0.1.0\"\n\
         requires-python = \">=3.9\"\ndependencies = []\n\n\
         [tool.uv.workspace]\nmembers = [\"packages/*\"]\n",
    ));
    let member = fixture.root.join("packages/app");
    std::fs::create_dir_all(&member).unwrap();
    std::fs::write(
        member.join("pyproject.toml"),
        "[project]\nname = \"app\"\nversion = \"0.1.0\"\n\
         requires-python = \">=3.9\"\ndependencies = [\"idna\"]\n",
    )
    .unwrap();

    let out = Command::new(BIN)
        .args(["sync"])
        .current_dir(&member)
        .env("SENV_STATE_DIR", &fixture.state)
        .env("SENV_CACHE_DIR", &fixture.cache)
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("senv runs");
    assert!(out.status.success(), "{}", combined(&out));

    assert!(
        !member.join("uv.lock").exists(),
        "senv wrote a lockfile into a workspace member"
    );
    assert!(
        fixture.root.join("uv.lock").is_file(),
        "the workspace lockfile belongs at the root"
    );
}

/// A package must not reach the environment by swapping the build backend.
///
/// The run phase can write `pyproject.toml`, and the install phase grants the
/// environment read-write — so pointing `build-backend` at a dropped script
/// let a package write into the "read-only" environment on the next ordinary
/// sync. Verified before the fix: it planted a `.pth` that then ran on every
/// later `senv run`.
#[test]
fn a_package_cannot_reach_the_environment_by_swapping_the_build_backend() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    fixture.run_python(
        "import pathlib\n\
         pathlib.Path('evil.py').write_text('def build_wheel(*a, **k): pass\\n')\n\
         p = pathlib.Path('pyproject.toml')\n\
         p.write_text(p.read_text() + '\\n[build-system]\\nrequires = []\\n\
         build-backend = \"evil\"\\nbackend-path = [\".\"]\\n')",
    );

    let out = fixture.senv(&["sync"]);
    let text = combined(&out);
    assert_eq!(
        out.status.code(),
        Some(2),
        "the swap must be refused: {text}"
    );
    assert!(text.contains("build-system"), "and named: {text}");
}

/// Turning on a `[tool.uv]` setting that makes installs build from source is
/// arbitrary code execution during sync, and needs no network.
#[test]
fn adding_a_tool_uv_table_is_caught_even_when_there_was_none() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    fixture.run_python(
        "import pathlib\n\
         p = pathlib.Path('pyproject.toml')\n\
         p.write_text(p.read_text() + '\\n[tool.uv]\\nno-binary = true\\n')",
    );

    let out = fixture.senv(&["sync"]);
    let text = combined(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("tool.uv"), "{text}");
}

/// `senv allow` must bless the document it wrote, not whatever is on disk when
/// it finishes.
#[test]
fn allow_records_what_it_wrote_not_a_re_read() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    // Stand in for a concurrent writer: the file is hostile at the moment
    // `allow` finishes. Re-reading it here was a full escalation — the
    // hostile config became the trusted baseline.
    std::fs::write(fixture.root.join("senv.toml"), "[run]\nnet = \"deny\"\n").unwrap();
    assert!(fixture.senv(&["trust"]).status.success());

    let out = fixture.senv(&["allow", "example.com"]);
    assert!(out.status.success(), "{}", combined(&out));
    std::fs::write(
        fixture.root.join("senv.toml"),
        "[env]\nallow-command-secrets = true\n[run]\nnet = \"host\"\n",
    )
    .unwrap();

    // The hostile edit is not part of the baseline, so it is still refused.
    let out = fixture.senv(&["run", "python", "-c", "print(1)"]);
    let text = combined(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("grants more than senv recorded"), "{text}");
}

/// A package must not be able to persist through the bytecode cache.
///
/// The read-only environment stops a package rewriting a module's *source*.
/// senv used to hand the run phase a writable `PYTHONPYCACHEPREFIX` so byte
/// compilation still worked — which is a writable, authoritative copy of that
/// same code, because CPython prefers a `.pyc` whose header matches the
/// source's mtime and size. One execution was enough to run attacker code on
/// every later import.
#[test]
fn a_package_cannot_persist_by_poisoning_the_bytecode_cache() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
         requires-python = \">=3.9\"\ndependencies = [\"idna\"]\n",
    ));
    fixture.sync();

    // Compile attacker code, then forge the header so CPython accepts it as
    // idna's own cached bytecode.
    std::fs::write(
        fixture.root.join("poison.py"),
        "import importlib.util, os, pathlib, py_compile, struct, idna\n\
         src = pathlib.Path(idna.__file__)\n\
         cached = pathlib.Path(importlib.util.cache_from_source(str(src)))\n\
         cached.parent.mkdir(parents=True, exist_ok=True)\n\
         evil = pathlib.Path(os.environ['TMPDIR']) / 'evil.py'\n\
         evil.write_text('__version__ = \"HIJACKED\"\\n')\n\
         py_compile.compile(str(evil), cfile=str(cached), dfile=str(src), doraise=True)\n\
         st = src.stat()\n\
         data = bytearray(cached.read_bytes())\n\
         data[8:12] = struct.pack('<I', int(st.st_mtime) & 0xFFFFFFFF)\n\
         data[12:16] = struct.pack('<I', st.st_size & 0xFFFFFFFF)\n\
         cached.write_bytes(bytes(data))\n",
    )
    .unwrap();
    fixture.senv(&["run", "python", "poison.py"]);

    let (out, text) = fixture.run_python("import idna; print('V', idna.__version__)");
    assert!(out.status.success(), "{text}");
    assert!(
        !text.contains("HIJACKED"),
        "attacker bytecode ran in place of a read-only module: {text}"
    );
    assert!(
        text.contains("V 3."),
        "the real module must still load: {text}"
    );
}

/// Dropping the writable cache must not cost startup time: the install phase
/// compiles bytecode into the environment, where it is read-only at run time.
#[test]
fn the_environment_ships_precompiled_bytecode() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let venv = fixture.state_dir().join("venv");
    let mut found = false;
    let mut stack = vec![venv];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "pyc") {
                found = true;
            }
        }
    }
    assert!(
        found,
        "the install phase should have compiled bytecode into the environment"
    );
}

/// A package must not be able to reroot senv onto a project it wrote itself.
#[test]
fn a_manufactured_nested_project_cannot_adopt_a_hostile_policy() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let marker = fixture.root.parent().unwrap().join("nested-escape-marker");
    let script = format!(
        "import pathlib\n\
         d = pathlib.Path('tests'); d.mkdir(exist_ok=True)\n\
         (d/'pyproject.toml').write_text('[project]\\nname=\"t\"\\nversion=\"0\"\\n\
         requires-python=\">=3.9\"\\ndependencies=[]\\n')\n\
         (d/'senv.toml').write_text('[env]\\nallow-command-secrets = true\\n\
         [secrets.TOKEN]\\nsource = \"command:touch {}; echo t\"\\nphases = [\"run\"]\\n')",
        marker.display()
    );
    fixture.run_python(&script);

    // The user cd's into the directory the package created.
    let nested = fixture.root.join("tests");
    let out = Command::new(BIN)
        .args(["run", "python", "-c", "print('hello')"])
        .current_dir(&nested)
        .env("SENV_STATE_DIR", &fixture.state)
        .env("SENV_CACHE_DIR", &fixture.cache)
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("senv runs");
    let text = combined(&out);

    assert!(
        !marker.exists(),
        "a manufactured project reached host execution: {text}"
    );
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("has not seen this project before") || text.contains("OUTSIDE the sandbox"),
        "the refusal should explain why: {text}"
    );
}

/// senv must not copy whatever a symlink points at into the install sandbox.
#[test]
fn staging_does_not_carry_host_secrets_across_the_boundary() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let secret = fixture.root.parent().unwrap().join("pretend-private-key");
    std::fs::write(&secret, "SUPER-SECRET-KEY\n").unwrap();

    // README.md is one of the files senv stages for project metadata.
    let script = format!("import os\nos.symlink('{}', 'README.md')", secret.display());
    fixture.run_python(&script);

    let out = fixture.senv(&["lock"]);
    let staged = fixture.state_dir().join("stage").join("README.md");
    let carried = std::fs::read_to_string(&staged).unwrap_or_default();
    assert!(
        !carried.contains("SUPER-SECRET-KEY"),
        "senv carried a host secret into the install sandbox: {}",
        combined(&out)
    );
}

/// senv must not write through a symlink planted in the project.
#[test]
fn senv_never_writes_through_a_symlink_in_the_project() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let outside = fixture.root.parent().unwrap().join("precious.txt");
    std::fs::write(&outside, "irreplaceable\n").unwrap();

    // The temp file behind senv's atomic write used to have a fixed name, so
    // it could be pre-planted as a symlink and `senv allow` would write
    // through it.
    let script = format!(
        "import os\nos.symlink('{}', '.senv.toml.tmp')",
        outside.display()
    );
    fixture.run_python(&script);
    fixture.senv(&["allow", "example.com"]);
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "irreplaceable\n",
        "senv wrote through a planted symlink"
    );
}

/// No command may launder a widening into the baseline on the user's behalf.
#[test]
fn no_command_accepts_a_widening_as_a_side_effect() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    fixture.run_python("open('senv.toml','w').write('[run]\\nnet = \"host\"\\n')");

    // `init` re-adopts a project and then syncs; `allow` deliberately widens
    // and re-records. Both once accepted whatever else was in the file.
    for command in [vec!["init", "--no-sync"], vec!["allow", "example.com"]] {
        let out = fixture.senv(&command);
        let text = combined(&out);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{command:?} must refuse first: {text}"
        );
        assert!(
            text.contains("grants more than senv recorded"),
            "{command:?}: {text}"
        );
    }

    // The widening is still pending, not silently absorbed.
    let out = fixture.senv(&["run", "python", "-c", "print(1)"]);
    assert_eq!(out.status.code(), Some(2), "{}", combined(&out));
}

/// senv must never execute a binary the sandbox could have written.
#[test]
fn a_package_cannot_point_senvs_toolchain_at_its_own_binary() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let marker = fixture.root.parent().unwrap().join("uv-rce-marker");
    let script = format!(
        "import os, stat\n\
         open('evil.sh','w').write('#!/bin/sh\\ntouch {}\\necho \"uv 9.9.9\"\\n')\n\
         os.chmod('evil.sh', 0o755)\n\
         open('senv.toml','w').write('[env]\\nuv = \"' + os.getcwd() + '/evil.sh\"\\n')",
        marker.display()
    );
    fixture.run_python(&script);

    // `doctor` used to run the configured binary to read its version.
    fixture.senv(&["doctor"]);
    assert!(
        !marker.exists(),
        "senv doctor executed an attacker-supplied binary"
    );

    // Even once accepted, a uv inside the project is refused outright: the
    // sandbox can rewrite that file between any two commands.
    assert!(fixture.senv(&["trust"]).status.success());
    fixture.senv(&["doctor"]);
    let out = fixture.senv(&["sync"]);
    let text = combined(&out);
    assert!(!marker.exists(), "an in-project uv was executed: {text}");
    assert!(
        text.contains("writable by code running under senv"),
        "{text}"
    );
}

/// Output from a program must not be able to forge or repaint senv's own
/// messages.
#[test]
fn program_output_cannot_forge_senvs_framing() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    fixture.sync();

    let (_, text) = fixture.run_python(
        "import sys\n\
         sys.stderr.write('\\x1b[2K\\rPermission denied: /home/u/\\x1b[32mSAFE\\x1b[0m\\x07\\n')",
    );
    // The program's own line is passed through untouched, as it would be
    // without senv. What must be clean is senv's quotation of it.
    let framed: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.contains("senv blocked"))
        .collect();
    assert!(
        !framed.is_empty(),
        "senv should have reported a denial:\n{text}"
    );
    for line in framed {
        assert!(
            !line.contains('\u{1b}'),
            "escape survived into senv's output: {line:?}"
        );
        assert!(
            !line.contains('\u{7}'),
            "BEL survived into senv's output: {line:?}"
        );
    }
}

/// Regression: two senv processes probing the host at the same moment used to
/// talk each other out of a tier the host supports.
///
/// h5i's cgroup probe proves delegation through a fixed path, so concurrent
/// probes delete each other's scratch cgroup and the loser concludes the host
/// cannot delegate — surfacing as "this host cannot enforce a network
/// allowlist" on a host that plainly can. Measured at roughly one failure in
/// eight before senv serialized the probe. Running two commands at once is
/// ordinary (a CI matrix, a dev server beside a manual sync), so this is a
/// user-facing property, not a test artefact.
#[test]
fn concurrent_commands_do_not_talk_each_other_out_of_a_tier() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );

    let fixtures: Vec<Fixture> = (0..6).map(|_| Fixture::new(Some(DEMO))).collect();
    let results: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = fixtures
            .iter()
            .map(|f| scope.spawn(move || combined(&f.senv(&["sync"]))))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .collect()
    });

    for text in &results {
        assert!(
            !text.contains("cannot enforce a network allowlist"),
            "a concurrent run lost a tier this host has:\n{text}"
        );
    }
}

/// Regression: h5i skips Landlock grants for paths that do not exist, so every
/// writable directory has to be created before a policy is compiled. Without
/// this, `uv` fails with `Permission denied` creating the environment.
#[test]
fn writable_directories_exist_before_the_policy_is_enforced() {
    require!(have_uv(), "uv is not installed");
    require!(
        can_install(),
        "this host cannot enforce an egress allowlist"
    );
    let fixture = Fixture::new(Some(DEMO));
    let out = fixture.senv(&["sync"]);
    assert!(
        out.status.success(),
        "a first sync into a fresh state dir must work: {}",
        combined(&out)
    );
    assert!(
        fixture
            .state_dir()
            .join("venv")
            .join("pyvenv.cfg")
            .is_file()
    );
}
