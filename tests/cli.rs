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
    assert!(text.contains("not installed"), "{text}");
    assert!(
        text.contains("senv add pytest"),
        "the fix must be copy-pasteable: {text}"
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
