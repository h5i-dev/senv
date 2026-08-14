//! Project discovery and senv's on-disk layout.
//!
//! # Why senv's state lives outside the project
//!
//! Everything senv writes — the environment, the resolved policies, the
//! receipts — lives under a per-project state directory outside the project
//! tree, and the project keeps only a `.venv` symlink pointing into it.
//!
//! This is a security property, not tidiness. The run phase grants the project
//! directory read-write (that is the point: your code edits your files), so
//! anything stored *inside* the project is writable by the very code the
//! boundary exists to contain. Receipts that a compromised package can rewrite
//! are not evidence, and an environment it can patch is not read-only. h5i
//! keeps its own receipts outside every box grant for exactly this reason, and
//! senv follows it.
//!
//! The side effect is a nice one for adoption: senv adds nothing to the project
//! tree that was not already there. A senv project is a uv project with an
//! optional `senv.toml`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::{self, CacheScope, Config};
use crate::error::{IoContext, Result, SenvError, fs};
use crate::util;

/// Files that mark a directory as the root of a Python project, in the order
/// senv prefers them.
const MARKERS: [&str; 3] = [config::CONFIG_FILE, "pyproject.toml", "uv.lock"];

/// How the environment on disk came to be — the answer to "did these bytes pass
/// through the install boundary?".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provenance {
    /// No environment yet.
    #[default]
    Absent,
    /// Every package in it was installed by senv, inside the install boundary.
    Sandboxed,
    /// Adopted from a `.venv` that already existed. Its contents were installed
    /// on the host, outside any boundary, so senv makes no claim about them
    /// until the next sandboxed sync.
    HostInstalled,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Absent => "absent",
            Provenance::Sandboxed => "sandboxed",
            Provenance::HostInstalled => "host-installed (unverified)",
        }
    }
}

/// `state.json` — what senv remembers about this project between commands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    pub project_root: String,
    pub provenance: Provenance,
    /// Interpreter the environment was built with, as uv reported it.
    pub python: Option<String>,
    /// sha256 of `uv.lock` at the last successful sync — how `senv status`
    /// knows the environment is stale.
    pub lock_hash: Option<String>,
    pub last_sync_ms: Option<u64>,
    /// Digest of the policy the last install ran under.
    pub install_digest: Option<String>,
    /// Digest of the policy the last run ran under.
    pub run_digest: Option<String>,
    /// The configuration senv was last told to trust.
    ///
    /// Kept here, in senv's state directory, precisely because `senv.toml`
    /// itself is inside the project — which the run phase grants read-write.
    /// See [`crate::trust`].
    pub trusted: Option<crate::trust::PolicySnapshot>,
}

impl State {
    pub const VERSION: u32 = 1;
}

/// A discovered project and every path senv derives from it.
#[derive(Debug, Clone)]
pub struct Project {
    /// Canonicalized project root — the directory holding `pyproject.toml`.
    pub root: PathBuf,
    /// Stable identifier: a readable name plus a hash of the absolute path, so
    /// two projects with the same directory name never share state.
    pub key: String,
    /// Per-project state directory, outside the project tree.
    pub state_dir: PathBuf,
    pub config: Config,
    /// Where the config was loaded from — it may not exist yet.
    pub config_path: PathBuf,
    /// Root of the shared caches, resolved once at construction.
    ///
    /// Held rather than re-read: the roots come from the environment, and an
    /// accessor that consulted it on every call would let a `Project` return
    /// two different answers for the same question during its lifetime.
    cache_root: PathBuf,
}

impl Project {
    /// Find the project containing `start` by walking up to the filesystem
    /// root.
    pub fn discover(start: &Path) -> Result<Project> {
        let start = fs::canonicalize(start)?;
        let mut dir = start.as_path();
        loop {
            if MARKERS.iter().any(|m| dir.join(m).is_file()) {
                // A uv workspace is one project with one lockfile and one
                // environment, and uv resolves it from the root no matter which
                // member you stand in. Rooting senv at the member instead
                // produced a `uv.lock` inside the member — a file people commit
                // and plain uv never creates — from a resolution that could not
                // even read the workspace root, because the root is outside the
                // member's grants.
                if let Some(root) = workspace_root_above(dir) {
                    return Project::at(&root);
                }
                return Project::at(dir);
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => return Err(SenvError::NoProject { start }),
            }
        }
    }

    /// Build a project rooted at exactly `root`, without searching upward.
    /// Used by `senv init`, which creates the marker rather than finding it.
    pub fn at(root: &Path) -> Result<Project> {
        let root = fs::canonicalize(root)?;
        let key = project_key(&root);
        let state_dir = state_root()?.join("projects").join(&key);
        let cache_root = cache_root()?;
        let config_path = config::config_path(&root);
        let config = Config::load(&config_path)?;
        Ok(Project {
            root,
            key,
            state_dir,
            config,
            config_path,
            cache_root,
        })
    }

    // ── paths ───────────────────────────────────────────────────────────────

    /// The environment itself. Not inside the project: see the module docs.
    pub fn venv(&self) -> PathBuf {
        self.state_dir.join("venv")
    }

    /// The `.venv` symlink inside the project, for editors and language
    /// servers that expect it.
    pub fn venv_link(&self) -> PathBuf {
        self.root.join(".venv")
    }

    /// Writable scratch for the run phase, including the bytecode cache.
    pub fn scratch(&self) -> PathBuf {
        self.state_dir.join("scratch")
    }

    /// `TMPDIR` for a phase. Each phase gets its own, so a temp file written
    /// during an install is not sitting in the run phase's writable set.
    pub fn tmp(&self, phase: &str) -> PathBuf {
        self.state_dir.join("tmp").join(phase)
    }

    /// Where manifests are copied for a staged resolution.
    pub fn stage(&self) -> PathBuf {
        self.state_dir.join("stage")
    }

    /// The wheel cache this project uses, honouring `[install] cache`.
    pub fn cache(&self) -> PathBuf {
        match self.config.install.cache {
            CacheScope::Project => self.state_dir.join("cache"),
            CacheScope::Shared => self.cache_root.join("uv"),
        }
    }

    /// Interpreters installed by uv. Shared across projects — they are large,
    /// and they are written only by the provisioning phase, which runs no
    /// third-party code. Every other phase gets them read-only.
    pub fn python_dir(&self) -> PathBuf {
        self.cache_root.join("python")
    }

    /// Append-only execution record. Outside every grant, by construction.
    pub fn receipt_path(&self) -> PathBuf {
        self.state_dir.join("receipt.jsonl")
    }

    pub fn state_path(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    /// Where a phase's resolved policy is written for inspection. The file is
    /// evidence, not input: senv recompiles the policy every run and compares
    /// digests rather than trusting what is on disk.
    pub fn policy_path(&self, phase: &str) -> PathBuf {
        self.state_dir.join(format!("policy.{phase}.toml"))
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("uv.lock")
    }

    pub fn pyproject_path(&self) -> PathBuf {
        self.root.join("pyproject.toml")
    }

    // ── lifecycle ───────────────────────────────────────────────────────────

    /// Create the state directories this project needs. Idempotent.
    ///
    /// Every writable path must exist *before* a policy is compiled. h5i skips
    /// Landlock grants for paths that are not there — narrowing the sandbox,
    /// which is the fail-closed direction and exactly right in general, but it
    /// means a grant for a directory uv has not created yet silently does
    /// nothing and uv then fails with `Permission denied`. Creating them here
    /// is what makes the write grants real.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            self.state_dir.clone(),
            self.venv(),
            self.scratch(),
            self.tmp("install"),
            self.tmp("run"),
            self.cache(),
            self.python_dir(),
        ] {
            fs::create_dir_all(&dir)?;
        }
        // The state directory can hold brokered secret files and the receipt
        // log; on a multi-user machine neither is anyone else's business.
        restrict_to_owner(&self.state_dir)?;
        Ok(())
    }

    pub fn load_state(&self) -> State {
        let path = self.state_path();
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<State>(&t).ok())
            .unwrap_or_else(|| State {
                version: State::VERSION,
                project_root: self.root.display().to_string(),
                ..State::default()
            })
    }

    /// Read, modify and write `state.json` under a lock.
    ///
    /// `load_state` → mutate → `save_state` is a read-modify-write, and two
    /// senv commands overlapping (a `senv run` during a `senv sync`, which the
    /// design calls ordinary) could drop the other's `lock_hash` — after which
    /// `status` reports "not yet synced" and every run warns about a stale
    /// lockfile. `write_atomic` makes each write safe; it does not make the
    /// pair atomic.
    pub fn update_state(&self, edit: impl FnOnce(&mut State)) -> Result<()> {
        use std::os::fd::AsRawFd;

        let lock_path = self.state_dir.join(".state.lock");
        let guard = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .ok();
        if let Some(file) = &guard {
            // Best-effort: a lock we cannot take costs the pre-existing race,
            // which is worth less than refusing to record state at all.
            // SAFETY: flock on a descriptor we own, released on drop.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        }
        let mut state = self.load_state();
        edit(&mut state);
        let result = self.save_state(&state);
        if let Some(file) = &guard {
            // SAFETY: same descriptor, still open.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
        result
    }

    pub fn save_state(&self, state: &State) -> Result<()> {
        let path = self.state_path();
        let text = serde_json::to_string_pretty(state)
            .map_err(|e| SenvError::internal(format!("serializing state: {e}")))?;
        write_atomic(&path, text.as_bytes())
    }

    /// The nearest ancestor directory that senv already tracks as a project,
    /// if any.
    ///
    /// A project nested inside another one is legitimate in a monorepo and is
    /// also the signature of an attack: the run phase can write anywhere in the
    /// project, so a package can create `tests/pyproject.toml` plus a hostile
    /// `tests/senv.toml`, and a user who later runs senv from that directory
    /// gets a different project with no recorded baseline. senv reports it
    /// rather than refusing, because breaking monorepos to catch this would be
    /// the wrong trade — the refusal that actually stops it is the first-sight
    /// check on a wide policy.
    pub fn enclosing_project(&self) -> Option<PathBuf> {
        let projects = state_root().ok()?.join("projects");
        let mut dir = self.root.parent();
        while let Some(candidate) = dir {
            if projects
                .join(project_key(candidate))
                .join("state.json")
                .is_file()
            {
                return Some(candidate.to_path_buf());
            }
            dir = candidate.parent();
        }
        None
    }

    /// The policy this project presents right now: `senv.toml` plus the parts
    /// of `pyproject.toml` that decide what code an install runs.
    pub fn policy_snapshot(&self) -> crate::trust::PolicySnapshot {
        let manifest =
            crate::error::fs::read_to_string_bounded(&self.pyproject_path()).unwrap_or_default();
        crate::trust::PolicySnapshot::of_project(&self.config, &manifest)
    }

    /// Compare the configuration on disk with the snapshot senv recorded.
    pub fn trust_verdict(&self) -> crate::trust::Verdict {
        let current = self.policy_snapshot();
        match self.load_state().trusted {
            None => crate::trust::Verdict::FirstSight,
            // A snapshot written by a different senv cannot be compared field
            // by field; treating it as a difference would accuse every user of
            // tampering the first time they upgrade.
            Some(previous) if previous.version != crate::trust::SNAPSHOT_VERSION => {
                crate::trust::Verdict::FormatChanged
            }
            Some(previous) => {
                let widenings = current.widenings(&previous);
                if widenings.is_empty() {
                    crate::trust::Verdict::Trusted
                } else {
                    crate::trust::Verdict::Widened(widenings)
                }
            }
        }
    }

    /// Record the configuration on disk as the trusted baseline.
    pub fn record_trust(&self) -> Result<()> {
        let snapshot = self.policy_snapshot();
        let root = self.root.display().to_string();
        self.update_state(|state| {
            state.version = State::VERSION;
            state.project_root = root;
            state.trusted = Some(snapshot);
        })
    }

    /// Current on-disk truth about the environment, independent of `state.json`
    /// — used to notice a venv deleted behind senv's back.
    pub fn venv_exists(&self) -> bool {
        self.venv().join("pyvenv.cfg").is_file()
    }

    /// Is the recorded lock hash still the lockfile's hash? `None` when there
    /// is no lockfile or nothing recorded.
    pub fn lock_is_current(&self, state: &State) -> Option<bool> {
        let on_disk = util::sha256_file(&self.lock_path())?;
        state.lock_hash.as_ref().map(|h| *h == on_disk)
    }

    /// What `.venv` in the project currently is.
    pub fn venv_link_status(&self) -> VenvLink {
        let link = self.venv_link();
        let Ok(meta) = std::fs::symlink_metadata(&link) else {
            return VenvLink::Absent;
        };
        if meta.file_type().is_symlink() {
            return match std::fs::read_link(&link) {
                // Points at senv's environment, but that environment may not
                // exist yet — reporting "linked" for a dangling link sent
                // people looking in the wrong place.
                Ok(target) if target == self.venv() && !self.venv_exists() => VenvLink::Dangling,
                Ok(target) if target == self.venv() => VenvLink::Ours,
                Ok(target) => VenvLink::OtherLink(target),
                Err(_) => VenvLink::Foreign,
            };
        }
        VenvLink::Directory
    }

    /// Point `.venv` at senv's environment, unless a real directory is sitting
    /// there — senv never deletes a directory it did not create.
    pub fn ensure_venv_link(&self) -> Result<VenvLink> {
        let status = self.venv_link_status();
        let link = self.venv_link();
        match status {
            VenvLink::Ours | VenvLink::Dangling => Ok(status),
            VenvLink::Absent => {
                symlink(&self.venv(), &link)?;
                Ok(VenvLink::Ours)
            }
            // A stale link (a previous state dir, or a moved project) is ours to
            // fix: replacing a symlink destroys nothing.
            VenvLink::OtherLink(_) | VenvLink::Foreign => {
                std::fs::remove_file(&link).at(&link)?;
                symlink(&self.venv(), &link)?;
                Ok(VenvLink::Ours)
            }
            VenvLink::Directory => Ok(status),
        }
    }

    /// Replace a real `.venv` directory with a link to senv's environment.
    /// Only ever called from an explicit `--replace-venv`.
    pub fn replace_venv_directory(&self) -> Result<()> {
        let link = self.venv_link();
        if matches!(self.venv_link_status(), VenvLink::Directory) {
            fs::remove_dir_all(&link)?;
        }
        self.ensure_venv_link().map(|_| ())
    }
}

/// What the project's `.venv` entry is right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VenvLink {
    /// A symlink to senv's environment.
    Ours,
    /// A symlink to senv's environment, which does not exist yet.
    Dangling,
    /// Nothing there.
    Absent,
    /// A real directory — an environment installed outside senv.
    Directory,
    /// A symlink somewhere else.
    OtherLink(PathBuf),
    /// A symlink we could not read.
    Foreign,
}

impl VenvLink {
    /// The warning to print, if this state deserves one.
    pub fn warning(&self) -> Option<String> {
        match self {
            VenvLink::Dangling => Some(
                ".venv points at senv's environment, which has not been built yet — run \
                 `senv sync`."
                    .to_string(),
            ),
            VenvLink::Directory => Some(
                "a real .venv directory exists here, so senv left it alone. Your editor and \
                 `source .venv/bin/activate` will use that environment, not senv's — run \
                 `senv init --replace-venv` to delete it and link to senv's instead."
                    .to_string(),
            ),
            _ => None,
        }
    }
}

// ── roots ───────────────────────────────────────────────────────────────────

/// The nearest ancestor of `dir` whose `pyproject.toml` declares a uv
/// workspace.
///
/// Membership globs are deliberately not evaluated. Getting `members =
/// ["packages/*", "!packages/legacy"]` subtly wrong would mean rooting some
/// projects in the wrong place, and the failure mode of being slightly
/// over-eager here is a note telling the user which project senv picked —
/// while the failure mode of missing a workspace is a corrupt lockfile
/// committed to their repository.
fn workspace_root_above(dir: &Path) -> Option<PathBuf> {
    let mut candidate = dir.parent();
    while let Some(current) = candidate {
        let manifest = current.join("pyproject.toml");
        if manifest.is_file()
            && let Ok(text) = crate::error::fs::read_to_string_bounded(&manifest)
            && let Ok(value) = toml::from_str::<toml::Value>(&text)
            && value
                .get("tool")
                .and_then(|t| t.get("uv"))
                .and_then(|u| u.get("workspace"))
                .is_some()
        {
            return Some(current.to_path_buf());
        }
        candidate = current.parent();
    }
    None
}

/// Root of senv's state, honouring `SENV_STATE_DIR` and then XDG.
pub fn state_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SENV_STATE_DIR") {
        return absolute_root("SENV_STATE_DIR", dir);
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Ok(dir.join("senv"));
        }
    }
    Ok(home()?.join(".local").join("state").join("senv"))
}

/// Root of senv's shared caches, honouring `SENV_CACHE_DIR` and then XDG.
pub fn cache_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SENV_CACHE_DIR") {
        return absolute_root("SENV_CACHE_DIR", dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Ok(dir.join("senv"));
        }
    }
    Ok(home()?.join(".cache").join("senv"))
}

/// senv's roots must be absolute.
///
/// A relative value is resolved against the working directory, which changes
/// per invocation — and one that resolved *into* a project would put the venv
/// and the receipts inside the run phase's write grant, quietly voiding the
/// invariant the whole state layout exists for. The XDG variables are already
/// filtered this way; these two were not.
fn absolute_root(var: &str, value: std::ffi::OsString) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(SenvError::refused(
            format!("{var} must be an absolute path"),
            format!(
                "{} is relative, so it would move with the working directory — and if it \
                 resolved inside a project, senv's state would land inside the sandbox's own \
                 write grant.",
                path.display()
            ),
            format!("set {var} to an absolute path, or unset it to use the default"),
        ));
    }
    Ok(path)
}

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        SenvError::internal("$HOME is not set, so senv cannot locate its state directory")
    })
}

/// `<readable-name>-<12 hex of the absolute path>`.
///
/// The hash is what makes it unique; the name is what makes `ls
/// ~/.local/state/senv/projects` legible.
pub fn project_key(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let sanitized = sanitized.trim_matches('-').to_string();
    let sanitized = if sanitized.is_empty() {
        "project".to_string()
    } else {
        sanitized
    };
    let hash = util::sha256_hex(root.as_os_str().as_encoded_bytes());
    format!("{sanitized}-{}", &hash[..12])
}

// ── filesystem helpers ──────────────────────────────────────────────────────

fn symlink(target: &Path, link: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).at(link)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        Err(SenvError::internal(
            "symlinks are not supported on this platform",
        ))
    }
}

/// Write via a temp file and rename, so a crash mid-write cannot leave a
/// truncated `state.json` that the next command fails to parse.
///
/// The temp file is created with `O_EXCL | O_NOFOLLOW` under an unpredictable
/// name. Both matter when the destination is inside a project: a fixed name was
/// pre-plantable as a symlink, and `senv allow` then wrote its output to
/// wherever that symlink pointed — outside the sandbox, as the user. `O_EXCL`
/// is what actually closes it; the random suffix keeps a stale temp from a
/// crashed run out of the way.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| SenvError::internal(format!("{} has no parent", path.display())))?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "senv".to_string());

    let mut last_err = None;
    for _ in 0..8 {
        let tmp = parent.join(format!(".{name}.{}.tmp", temp_suffix()));
        match fs::create_new_no_follow(&tmp) {
            Ok(mut file) => {
                let written = file
                    .write_all(bytes)
                    .and_then(|_| file.sync_all())
                    .map_err(|e| SenvError::io(&tmp, e));
                if let Err(e) = written {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                drop(file);
                // `rename` replaces the destination itself rather than
                // following it, so a symlink at `path` is destroyed instead of
                // written through.
                return match std::fs::rename(&tmp, path) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp);
                        Err(SenvError::io(path, e))
                    }
                };
            }
            // Occupied or a symlink: try another name rather than touching it.
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        SenvError::internal(format!(
            "could not create a temp file next to {}",
            path.display()
        ))
    }))
}

/// An unpredictable-enough suffix for a temp file name.
///
/// Not a security control — `O_EXCL` is. This only avoids collisions between
/// concurrent writers and stale files from crashed runs, so the clock and the
/// pid are sufficient and keep senv free of another dependency.
fn temp_suffix() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{:x}{:x}{:x}",
        std::process::id(),
        nanos,
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// `chmod 0700`, best-effort on platforms without Unix permissions.
fn restrict_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, perms).at(path)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_separates_projects_that_share_a_directory_name() {
        let a = project_key(Path::new("/home/u/work/api"));
        let b = project_key(Path::new("/home/u/personal/api"));
        assert_ne!(a, b, "same basename, different path — must not share state");
        assert!(a.starts_with("api-"), "the key should stay readable: {a}");
        assert_eq!(
            a,
            project_key(Path::new("/home/u/work/api")),
            "must be stable"
        );
    }

    #[test]
    fn a_key_is_always_a_usable_directory_name() {
        for weird in ["/", "/a/../b", "/home/u/my project (v2)", "/home/u/.."] {
            let key = project_key(Path::new(weird));
            assert!(!key.is_empty());
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{weird} produced an unusable key: {key}"
            );
        }
    }

    #[test]
    fn discovery_walks_up_and_prefers_the_nearest_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("proj");
        let nested = root.join("src").join("pkg");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\nname='x'\n").unwrap();

        let found = Project::discover(&nested).expect("discovers from a nested dir");
        assert_eq!(found.root, std::fs::canonicalize(&root).unwrap());
    }

    #[test]
    fn discovery_reports_where_it_looked_when_there_is_no_project() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = Project::discover(tmp.path()).expect_err("no markers here");
        assert!(matches!(err, SenvError::NoProject { .. }));
        assert!(err.to_string().contains("pyproject.toml"), "{err}");
    }

    #[test]
    fn state_survives_a_round_trip_and_a_corrupt_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        unsafe { std::env::set_var("SENV_STATE_DIR", tmp.path().join("state")) };

        let project = Project::at(&root).expect("project");
        project.ensure_dirs().expect("dirs");

        let mut state = project.load_state();
        assert_eq!(state.provenance, Provenance::Absent);
        state.provenance = Provenance::Sandboxed;
        state.lock_hash = Some("abc".into());
        project.save_state(&state).expect("save");
        assert_eq!(project.load_state().provenance, Provenance::Sandboxed);

        // A truncated state file must degrade to defaults, not abort every
        // later command.
        std::fs::write(project.state_path(), "{not json").unwrap();
        assert_eq!(project.load_state().provenance, Provenance::Absent);
    }

    #[test]
    fn the_venv_link_is_created_but_a_real_directory_is_never_deleted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        unsafe { std::env::set_var("SENV_STATE_DIR", tmp.path().join("state2")) };
        let project = Project::at(&root).expect("project");

        assert_eq!(project.venv_link_status(), VenvLink::Absent);
        assert_eq!(project.ensure_venv_link().expect("link"), VenvLink::Ours);
        // Nothing has been built yet, so the link is real but dangling — and
        // saying "linked" for that sent people looking in the wrong place.
        assert_eq!(project.venv_link_status(), VenvLink::Dangling);
        assert!(project.venv_link_status().warning().is_some());

        // Once the environment exists, it is simply ours.
        std::fs::create_dir_all(project.venv()).unwrap();
        std::fs::write(project.venv().join("pyvenv.cfg"), "home = /usr\n").unwrap();
        assert_eq!(project.venv_link_status(), VenvLink::Ours);
        assert!(project.venv_link_status().warning().is_none());

        // Now the adoption case: a real directory the user built with plain uv.
        std::fs::remove_file(project.venv_link()).unwrap();
        std::fs::create_dir_all(project.venv_link().join("bin")).unwrap();
        assert_eq!(project.venv_link_status(), VenvLink::Directory);
        assert_eq!(
            project.ensure_venv_link().expect("link"),
            VenvLink::Directory
        );
        assert!(
            project.venv_link().join("bin").is_dir(),
            "senv must never delete an environment it did not create"
        );
        assert!(project.venv_link_status().warning().is_some());

        // …until asked explicitly.
        project.replace_venv_directory().expect("replace");
        assert_eq!(project.venv_link_status(), VenvLink::Ours);
    }
}
