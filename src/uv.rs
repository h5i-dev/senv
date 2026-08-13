//! Driving `uv`, and the staging that keeps dependency resolution away from
//! your source tree.
//!
//! senv never reimplements resolution, locking, or installation — uv does all
//! of it, and a senv project stays a valid uv project. What senv adds is
//! *where* uv is allowed to run.
//!
//! # Staging
//!
//! `senv lock` / `add` / `remove` must write `pyproject.toml` and `uv.lock`,
//! which means the install phase would need write access to the project root —
//! and dependency resolution is exactly when a malicious sdist's build backend
//! gets to execute (PEP 517 metadata preparation). So senv copies the manifest
//! files into a staging directory, resolves there, validates the result, and
//! copies back. A build backend that runs during resolution sees a directory
//! with a `pyproject.toml` in it and nothing else of yours.
//!
//! Staging cannot work for every project: dynamic metadata and path/workspace
//! dependencies need the real tree. senv detects those from the manifest and
//! says so, rather than failing mysteriously or silently widening.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Result, SenvError, fs};
use crate::project::Project;
use crate::util;

/// Files copied into a staging directory. Enough for uv to resolve a project
/// with static metadata; deliberately not the source tree.
const STAGE_FILES: [&str; 3] = ["pyproject.toml", "uv.lock", ".python-version"];

/// Files uv may legitimately read for project metadata (`readme`, `license`).
/// Copied when present so a `readme = "README.md"` does not break the build.
const STAGE_GLOBS: [&str; 6] = [
    "README.md",
    "README.rst",
    "README.txt",
    "LICENSE",
    "LICENSE.txt",
    "LICENSE.md",
];

/// Locate the `uv` binary, resolved to a real path so it can be granted.
///
/// A `[env] uv` path is refused when it resolves inside the project or inside
/// senv's state, because both are writable by the sandbox: a package that
/// writes `evil.sh` into the project and points this key at it would have
/// senv running its binary as senv's own toolchain.
pub fn find(project: &Project) -> Result<PathBuf> {
    let path = find_for(Some(&project.config))?;
    if project.config.env.uv.is_some() {
        for (label, dir) in [
            ("project", &project.root),
            ("senv state", &project.state_dir),
        ] {
            if path.starts_with(dir) {
                return Err(SenvError::refused(
                    format!("[env] uv points inside your {label} directory"),
                    format!(
                        "{} is writable by code running under senv, so senv will not run it as \
                         its own toolchain — that would turn one sandboxed execution into an \
                         unsandboxed one.",
                        path.display()
                    ),
                    "point [env] uv at an installed uv (for example ~/.local/bin/uv), or remove \
                     the key and let senv find it on PATH",
                ));
            }
        }
    }
    Ok(path)
}

/// Is this uv path one the *project* chose, rather than one senv found?
///
/// A configured path is untrusted input: `senv.toml` is inside the sandbox's
/// write grant. senv still runs it — confined, as the install phase's tool —
/// but never unconfined, which rules out the host-side version probe.
pub fn is_project_specified(project: &Project) -> bool {
    project.config.env.uv.is_some()
}

/// [`find`] without a project — `senv doctor` runs outside one.
pub fn find_for(config: Option<&crate::config::Config>) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(configured) = config.and_then(|c| c.env.uv.as_ref()) {
        candidates.push(PathBuf::from(util::expand_tilde(configured)));
    }
    if let Some(env) = std::env::var_os("SENV_UV") {
        candidates.push(PathBuf::from(env));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|p| p.join("uv")));
    }
    for fallback in [
        "~/.local/bin/uv",
        "~/.cargo/bin/uv",
        "/usr/local/bin/uv",
        "/opt/homebrew/bin/uv",
    ] {
        candidates.push(PathBuf::from(util::expand_tilde(fallback)));
    }

    for candidate in candidates {
        if is_executable(&candidate) {
            // Canonicalize: uv is often a symlink, and Landlock grants resolve
            // to the real path. Granting the link would grant nothing.
            return fs::canonicalize(&candidate);
        }
    }
    Err(SenvError::Uv(
        "uv was not found on PATH. senv drives uv for every package operation, so it is a \
         hard requirement."
            .to_string(),
    ))
}

fn is_executable(path: &Path) -> bool {
    let Ok(md) = std::fs::metadata(path) else {
        return false;
    };
    if !md.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

/// `uv --version`, run on the host, **unconfined**.
///
/// Only ever called for a uv that senv discovered itself — on `PATH` or in a
/// standard location — which is host-owned and as trusted as senv's own
/// binary. It must never be called for a path that came from `senv.toml`; see
/// [`is_project_specified`]. That distinction was a real escape: `senv doctor`
/// executed the configured path, so a package that wrote a script into the
/// project and set `[env] uv` got host execution out of a diagnostic command.
pub fn version(uv: &Path) -> Option<String> {
    let out = Command::new(uv).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ── staging ─────────────────────────────────────────────────────────────────

/// Whether a project's manifests can be resolved away from the source tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staging {
    /// Resolution runs in a staging copy; the project stays untouched.
    Possible,
    /// Resolution needs the real project directory, for the stated reason.
    /// senv announces this and records it — it is a deterministic consequence
    /// of declared configuration, not a silent fallback.
    Impossible(String),
}

impl Staging {
    /// Why resolution cannot be staged, when it cannot.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Staging::Impossible(r) => Some(r),
            Staging::Possible => None,
        }
    }
}

/// Decide from `pyproject.toml` whether staging can work.
pub fn staging_for(pyproject: &str) -> Staging {
    let parsed: toml::Value = match toml::from_str(pyproject) {
        Ok(v) => v,
        // An unparseable manifest is uv's problem to report, with the real
        // project in front of it.
        Err(_) => return Staging::Impossible("pyproject.toml could not be parsed".to_string()),
    };

    let project = parsed.get("project");
    if project.is_none() && parsed.get("tool").and_then(|t| t.get("uv")).is_none() {
        return Staging::Impossible(
            "pyproject.toml declares no [project] table, so uv needs the real directory"
                .to_string(),
        );
    }

    if let Some(dynamic) = project
        .and_then(|p| p.get("dynamic"))
        .and_then(|d| d.as_array())
        && !dynamic.is_empty()
    {
        let fields: Vec<String> = dynamic
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect();
        return Staging::Impossible(format!(
            "[project] dynamic = [{}] — the build backend must run against your source to \
                 produce that metadata",
            fields.join(", ")
        ));
    }

    let uv_tool = parsed.get("tool").and_then(|t| t.get("uv"));
    if uv_tool.and_then(|u| u.get("workspace")).is_some() {
        return Staging::Impossible(
            "[tool.uv.workspace] — workspace members live outside the manifest".to_string(),
        );
    }
    if let Some(sources) = uv_tool
        .and_then(|u| u.get("sources"))
        .and_then(|s| s.as_table())
    {
        for (name, spec) in sources {
            let local = spec.get("path").is_some()
                || spec.get("workspace").and_then(|w| w.as_bool()) == Some(true);
            if local {
                return Staging::Impossible(format!(
                    "[tool.uv.sources] '{}' is a local path dependency",
                    crate::util::sanitize(name)
                ));
            }
        }
    }

    Staging::Possible
}

/// A prepared staging directory.
pub struct Stage {
    pub dir: PathBuf,
    /// Hash of `pyproject.toml` as copied in, so a change made *during*
    /// resolution is detectable.
    pyproject_before: Option<String>,
    lock_before: Option<String>,
}

/// Copy the manifests into a clean staging directory.
pub fn prepare_stage(project: &Project) -> Result<Stage> {
    let dir = project.stage();
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;

    for name in STAGE_FILES.iter().chain(STAGE_GLOBS.iter()) {
        let src = project.root.join(name);
        if src.is_file() {
            fs::copy(&src, &dir.join(name))?;
        }
    }
    Ok(Stage {
        pyproject_before: util::sha256_file(&dir.join("pyproject.toml")),
        lock_before: util::sha256_file(&dir.join("uv.lock")),
        dir,
    })
}

/// What copying a staged result back to the project changed.
#[derive(Debug, Default)]
pub struct StageResult {
    pub changed: Vec<String>,
    /// Findings a user must see: a manifest edited when nothing should have
    /// edited it, or keys changed that the requested operation does not touch.
    pub warnings: Vec<String>,
}

/// Copy the staged manifests back into the project.
///
/// `expect_manifest_change` is true for `add`/`remove`, which edit
/// `pyproject.toml` by design, and false for `lock`, which must not. The
/// distinction is the check: something rewrote `pyproject.toml` during a plain
/// lock is a finding, not a diff to apply.
pub fn apply_stage(
    project: &Project,
    stage: &Stage,
    expect_manifest_change: bool,
) -> Result<StageResult> {
    let mut result = StageResult::default();

    let staged_pyproject = stage.dir.join("pyproject.toml");
    let after = util::sha256_file(&staged_pyproject);
    let manifest_changed = after != stage.pyproject_before && after.is_some();

    if manifest_changed {
        if !expect_manifest_change {
            result.warnings.push(format!(
                "pyproject.toml was modified during resolution, which this operation should \
                 not do. senv did NOT copy it back; the staged copy is at {} if you want to \
                 look at it.",
                staged_pyproject.display()
            ));
        } else {
            let before = fs::read_to_string(&project.pyproject_path()).unwrap_or_default();
            let now = fs::read_to_string(&staged_pyproject)?;
            for unexpected in unexpected_manifest_changes(&before, &now) {
                result.warnings.push(format!(
                    "pyproject.toml changed outside the dependency lists: {unexpected}"
                ));
            }
            // Validate before overwriting the user's manifest: a truncated or
            // unparseable copy-back would break the project.
            toml::from_str::<toml::Value>(&now).map_err(|e| {
                SenvError::config(
                    &staged_pyproject,
                    format!("staged manifest is invalid: {e}"),
                )
            })?;
            fs::write(&project.pyproject_path(), now.as_bytes())?;
            result.changed.push("pyproject.toml".to_string());
        }
    }

    let staged_lock = stage.dir.join("uv.lock");
    if staged_lock.is_file() {
        let after = util::sha256_file(&staged_lock);
        if after != stage.lock_before {
            let text = fs::read_to_string(&staged_lock)?;
            toml::from_str::<toml::Value>(&text).map_err(|e| {
                SenvError::config(&staged_lock, format!("staged lockfile is invalid: {e}"))
            })?;
            fs::write(&project.lock_path(), text.as_bytes())?;
            result.changed.push("uv.lock".to_string());
        }
    }

    Ok(result)
}

/// Top-level `pyproject.toml` keys that changed but should not have.
///
/// `add`/`remove` touch dependency lists and uv's source table. Anything else
/// moving is worth a sentence on the user's terminal, because the code that
/// could have moved it is a dependency's build backend.
fn unexpected_manifest_changes(before: &str, after: &str) -> Vec<String> {
    let (Ok(a), Ok(b)) = (
        toml::from_str::<toml::Value>(before),
        toml::from_str::<toml::Value>(after),
    ) else {
        return Vec::new();
    };
    let mut findings = Vec::new();

    let expected: [&[&str]; 4] = [
        &["project", "dependencies"],
        &["project", "optional-dependencies"],
        &["dependency-groups"],
        &["tool", "uv", "sources"],
    ];

    let mut keys: Vec<&String> = Vec::new();
    for table in [a.as_table(), b.as_table()].into_iter().flatten() {
        for key in table.keys() {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }

    for key in keys {
        let av = a.get(key);
        let bv = b.get(key);
        if av == bv {
            continue;
        }
        if key == "project" {
            // Narrow to the sub-keys, so a dependency edit is not reported.
            let sub_a = av.and_then(|v| v.as_table());
            let sub_b = bv.and_then(|v| v.as_table());
            let mut sub_keys: Vec<&String> = Vec::new();
            for t in [sub_a, sub_b].into_iter().flatten() {
                for k in t.keys() {
                    if !sub_keys.contains(&k) {
                        sub_keys.push(k);
                    }
                }
            }
            for sk in sub_keys {
                if sub_a.and_then(|t| t.get(sk)) == sub_b.and_then(|t| t.get(sk)) {
                    continue;
                }
                if !expected.iter().any(|e| e == &["project", sk.as_str()]) {
                    findings.push(format!("[project] {}", crate::util::sanitize(sk)));
                }
            }
            continue;
        }
        if key == "dependency-groups" {
            continue;
        }
        if key == "tool" {
            let sub_a = av.and_then(|v| v.get("uv"));
            let sub_b = bv.and_then(|v| v.get("uv"));
            // Only uv's own source table is expected to move.
            let strip = |v: Option<&toml::Value>| {
                let mut t = v.and_then(|v| v.as_table()).cloned().unwrap_or_default();
                t.remove("sources");
                t
            };
            if strip(sub_a) != strip(sub_b) {
                findings.push("[tool.uv]".to_string());
            }
            let other = |v: Option<&toml::Value>| {
                let mut t = v.and_then(|v| v.as_table()).cloned().unwrap_or_default();
                t.remove("uv");
                t
            };
            if other(av) != other(bv) {
                findings.push("[tool] (a section other than uv)".to_string());
            }
            continue;
        }
        findings.push(format!("[{}]", crate::util::sanitize(key)));
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_static_manifest_can_be_resolved_away_from_the_source() {
        let manifest = r#"
[project]
name = "demo"
version = "0.1.0"
dependencies = ["requests"]
"#;
        assert_eq!(staging_for(manifest), Staging::Possible);
    }

    #[test]
    fn dynamic_metadata_needs_the_real_tree_and_says_so() {
        let manifest = r#"
[project]
name = "demo"
dynamic = ["version"]
"#;
        let s = staging_for(manifest);
        let reason = s.reason().expect("must explain itself");
        assert!(reason.contains("dynamic"), "{reason}");
        assert!(reason.contains("version"), "{reason}");
    }

    #[test]
    fn path_dependencies_and_workspaces_need_the_real_tree() {
        let path_dep = r#"
[project]
name = "demo"
version = "0"
[tool.uv.sources]
mylib = { path = "../mylib" }
"#;
        assert!(staging_for(path_dep).reason().unwrap().contains("mylib"));

        let workspace = r#"
[project]
name = "demo"
version = "0"
[tool.uv.workspace]
members = ["packages/*"]
"#;
        assert!(
            staging_for(workspace)
                .reason()
                .unwrap()
                .contains("workspace")
        );

        // A registry source is fine — nothing local about it.
        let registry = r#"
[project]
name = "demo"
version = "0"
[tool.uv.sources]
torch = { index = "pytorch" }
"#;
        assert_eq!(staging_for(registry), Staging::Possible);
    }

    #[test]
    fn a_dependency_edit_is_expected_and_anything_else_is_reported() {
        let before = r#"
[project]
name = "demo"
version = "0.1.0"
dependencies = ["requests"]
"#;
        let with_dep = r#"
[project]
name = "demo"
version = "0.1.0"
dependencies = ["requests", "idna"]
"#;
        assert!(
            unexpected_manifest_changes(before, with_dep).is_empty(),
            "adding a dependency is the whole point of `senv add`"
        );

        // The case that matters: something rewrote the build backend while a
        // build backend was running.
        let tampered = r#"
[project]
name = "demo"
version = "0.1.0"
dependencies = ["requests"]
[build-system]
requires = ["evil"]
build-backend = "evil"
"#;
        let found = unexpected_manifest_changes(before, tampered);
        assert!(
            found.iter().any(|f| f.contains("build-system")),
            "{found:?}"
        );

        let renamed = r#"
[project]
name = "not-demo"
version = "0.1.0"
dependencies = ["requests"]
"#;
        let found = unexpected_manifest_changes(before, renamed);
        assert!(found.iter().any(|f| f.contains("name")), "{found:?}");
    }

    #[test]
    fn staging_copies_manifests_and_not_the_source_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='x'\nversion='0'\n",
        )
        .unwrap();
        std::fs::write(root.join("README.md"), "# x\n").unwrap();
        std::fs::write(root.join("src/secret_business_logic.py"), "TOKEN=1\n").unwrap();
        unsafe { std::env::set_var("SENV_STATE_DIR", tmp.path().join("state")) };
        unsafe { std::env::set_var("SENV_CACHE_DIR", tmp.path().join("cache")) };

        let project = Project::at(&root).unwrap();
        project.ensure_dirs().unwrap();
        let stage = prepare_stage(&project).unwrap();

        assert!(stage.dir.join("pyproject.toml").is_file());
        assert!(
            stage.dir.join("README.md").is_file(),
            "uv may need the readme for metadata"
        );
        assert!(
            !stage.dir.join("src").exists(),
            "a build backend running during resolution must not see your source"
        );
    }

    #[test]
    fn a_manifest_rewritten_during_a_plain_lock_is_not_copied_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let original = "[project]\nname='x'\nversion='0'\n";
        std::fs::write(root.join("pyproject.toml"), original).unwrap();
        unsafe { std::env::set_var("SENV_STATE_DIR", tmp.path().join("state2")) };
        unsafe { std::env::set_var("SENV_CACHE_DIR", tmp.path().join("cache2")) };

        let project = Project::at(&root).unwrap();
        project.ensure_dirs().unwrap();
        let stage = prepare_stage(&project).unwrap();

        // Simulate a build backend editing the manifest mid-resolution.
        std::fs::write(
            stage.dir.join("pyproject.toml"),
            "[project]\nname='x'\nversion='0'\n[build-system]\nrequires=['evil']\n",
        )
        .unwrap();

        let result = apply_stage(&project, &stage, false).unwrap();
        assert!(
            result.changed.is_empty(),
            "nothing should have been copied back"
        );
        assert!(!result.warnings.is_empty(), "and the user must be told");
        assert_eq!(
            std::fs::read_to_string(project.pyproject_path()).unwrap(),
            original,
            "the project manifest must be untouched"
        );
    }
}
