//! senv's error type.
//!
//! Two things distinguish it from a plain `anyhow`-style string error, and both
//! matter for a tool whose job is refusing to do things:
//!
//! - [`SenvError::Refused`] carries a *fix*. Every fail-closed refusal in senv
//!   names the one config change or command that would let the user proceed,
//!   because a boundary that only says "no" gets replaced by the boundary-free
//!   tool next door.
//! - The exit code is part of the type. senv passes a confined command's own
//!   exit code through untouched, so its failures must be distinguishable from
//!   senv's own.

use std::fmt;
use std::path::{Path, PathBuf};

/// Exit code for senv's own failures (a confined command's code passes through
/// unchanged, so this must not collide with a common one). 2 is the
/// conventional "usage/tool error" code and is what `uv` itself uses.
pub const EXIT_SENV_ERROR: i32 = 2;

#[derive(Debug)]
pub enum SenvError {
    /// No Python project (and no `senv.toml`) at or above the starting dir.
    NoProject { start: PathBuf },
    /// I/O, with the path that failed — never a bare "No such file".
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A malformed or invalid `senv.toml`.
    Config { path: PathBuf, message: String },
    /// The sandbox engine refused, or could not be applied.
    Sandbox(h5i_error::H5iError),
    /// A fail-closed refusal, with the change that would allow it.
    Refused {
        what: String,
        why: String,
        fix: String,
    },
    /// `uv` is missing or unusable.
    Uv(String),
    /// Anything else senv itself got wrong.
    Internal(String),
}

impl SenvError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        SenvError::Io {
            path: path.into(),
            source,
        }
    }

    pub fn config(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        SenvError::Config {
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn refused(
        what: impl Into<String>,
        why: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        SenvError::Refused {
            what: what.into(),
            why: why.into(),
            fix: fix.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        SenvError::Internal(message.into())
    }

    /// The remediation line, when there is one. Printed indented under the
    /// error so a user can copy it.
    pub fn fix(&self) -> Option<&str> {
        match self {
            SenvError::Refused { fix, .. } => Some(fix),
            SenvError::NoProject { .. } => {
                Some("run `senv init` here, or cd into a project with a pyproject.toml")
            }
            SenvError::Uv(_) => Some("install uv: curl -LsSf https://astral.sh/uv/install.sh | sh"),
            _ => None,
        }
    }
}

impl fmt::Display for SenvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SenvError::NoProject { start } => write!(
                f,
                "no Python project found at or above {}\n  senv looks for senv.toml, pyproject.toml, or uv.lock",
                start.display()
            ),
            SenvError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            SenvError::Config { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
            SenvError::Sandbox(e) => write!(f, "{e}"),
            SenvError::Refused { what, why, .. } => write!(f, "{what}\n  {why}"),
            SenvError::Uv(m) => write!(f, "{m}"),
            SenvError::Internal(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for SenvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SenvError::Io { source, .. } => Some(source),
            SenvError::Sandbox(e) => Some(e),
            _ => None,
        }
    }
}

impl From<h5i_error::H5iError> for SenvError {
    fn from(e: h5i_error::H5iError) -> Self {
        SenvError::Sandbox(e)
    }
}

pub type Result<T> = std::result::Result<T, SenvError>;

/// `std::fs` wrappers that attach the path. Used everywhere in senv instead of
/// bare `std::fs`, so no error ever reaches the user without saying which file
/// it was about.
pub mod fs {
    use super::{Result, SenvError};
    use std::path::Path;

    // NOTE: there is deliberately no unguarded `read_to_string` here. Every
    // file senv parses lives somewhere the sandbox can write, so each read goes
    // through one of the bounded, non-blocking, regular-file-only helpers
    // below. An unguarded wrapper sitting beside them is an invitation to reach
    // for the wrong one.

    /// Largest file senv will parse from a location the sandbox can write.
    ///
    /// Generous next to any real manifest — the biggest `uv.lock` in the wild
    /// is a few megabytes — and small enough that a package cannot turn the
    /// user's own tooling into an out-of-memory kill. A 200 MB `senv.toml`
    /// cost 4.7 s and 216 MB of RSS on *every* senv command before this;
    /// scaling that up is a one-line attack on the security tool itself.
    pub const MAX_PARSED_BYTES: u64 = 16 * 1024 * 1024;

    /// Read a file senv is going to parse, refusing an implausible one.
    ///
    /// Follows symlinks — some callers read manifests in ancestor directories
    /// senv does not govern — but is otherwise as defensive as
    /// [`read_to_string_no_follow`], and for the same reason: `pyproject.toml`
    /// is in the project, so its contents *and its file type* are chosen by the
    /// code the boundary contains. `os.mkfifo('pyproject.toml')` used to wedge
    /// every senv command forever on the open, since `read_to_string` blocks
    /// waiting for a writer that never comes.
    pub fn read_to_string_bounded(path: &Path) -> Result<String> {
        read_bounded(open_regular(path, 0)?, path)
    }

    fn too_large(path: &Path, len: u64) -> SenvError {
        SenvError::refused(
            format!(
                "{} is {} MiB, which is too large to be a real one",
                path.display(),
                len / 1024 / 1024
            ),
            "senv parses this file on every command, and it lives somewhere code running \
             under senv can write — so an implausible size is refused rather than loaded."
                .to_string(),
            format!(
                "inspect {} and replace it with the file you meant",
                path.display()
            ),
        )
    }

    /// Read a file senv parses **as policy**, refusing to follow a symlink.
    ///
    /// The counterpart of [`write_no_follow`], and it closes the same door from
    /// the other side. senv already refuses to *write* `senv.toml` through a
    /// link because the project directory is writable by the code the policy
    /// governs. Reading it through one was worse, and was a working
    /// arbitrary-file-read:
    ///
    /// A package replaces `senv.toml` with a symlink to `~/.netrc`. senv — the
    /// unconfined process — follows it, fails to parse it as TOML, and toml's
    /// error quotes the offending source line back to the terminal:
    ///
    /// ```text
    /// senv: …/senv.toml: TOML parse error at line 1, column 9
    ///   |
    /// 1 | machine api.example.com login deploy password S3cr3tT0ken
    /// ```
    ///
    /// Verified before this change. The credential never crossed the sandbox
    /// boundary — senv carried it out on the attacker's behalf, which is the
    /// same sentence `crate::uv`'s staging code already uses about the file
    /// it declines to copy. `~/.aws/credentials`, `~/.git-credentials` and a
    /// `.env` all read the same way, and one line is the whole secret.
    ///
    /// `O_NOFOLLOW` turns that into an `ELOOP` that [`symlink_aware`] explains.
    pub fn read_to_string_no_follow(path: &Path) -> Result<String> {
        read_bounded(open_no_follow(path)?, path)
    }

    /// Read an already-open file, bounded.
    ///
    /// The bound is applied against the descriptor, not a separate `stat` of
    /// the path: the two can disagree when the author of the file is hostile,
    /// and only the descriptor is the thing actually being read.
    fn read_bounded(file: std::fs::File, path: &Path) -> Result<String> {
        use std::io::Read;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len > MAX_PARSED_BYTES {
            return Err(too_large(path, len));
        }
        let mut text = String::new();
        file.take(MAX_PARSED_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|e| SenvError::io(path, e))?;
        if text.len() as u64 > MAX_PARSED_BYTES {
            return Err(too_large(path, text.len() as u64));
        }
        Ok(text)
    }

    /// Open a **regular** file for reading, following no symlink and blocking
    /// on nothing.
    ///
    /// Three flags, and every one of them is load-bearing because the caller is
    /// always opening a path the sandbox can create:
    ///
    /// - `O_NOFOLLOW` is the point: senv reads these unconfined, so a link would
    ///   redirect the read anywhere on the disk.
    /// - `O_NONBLOCK` stops the open itself from hanging. `open(2)` on a FIFO
    ///   for reading blocks until a writer arrives, so replacing `senv.toml`
    ///   with `os.mkfifo('senv.toml')` wedged every senv command **forever** —
    ///   verified, and a worse denial of service than the oversized-file one
    ///   these readers already guard against. It is the trap in replacing a
    ///   `metadata()` check with an open, since the check never touched the
    ///   file.
    /// - the `is_file` check afterwards is what makes the open equivalent to
    ///   the check it replaced: a device or a directory is not something senv
    ///   should read as policy, and on a regular file `O_NONBLOCK` means
    ///   nothing at all.
    pub fn open_no_follow(path: &Path) -> Result<std::fs::File> {
        #[cfg(unix)]
        return open_regular(path, libc::O_NOFOLLOW);
        #[cfg(not(unix))]
        return open_regular(path, 0);
    }

    /// Open a regular file for reading, blocking on nothing, with `extra`
    /// added to the open flags.
    #[cfg_attr(not(unix), allow(unused_variables))]
    fn open_regular(path: &Path, extra: i32) -> Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(extra | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)
                .map_err(|e| symlink_aware_read(path, e))?;
            if !file.metadata().map(|m| m.is_file()).unwrap_or(false) {
                return Err(SenvError::refused(
                    format!("{} is not a regular file", path.display()),
                    "senv reads this file unconfined, and code running under senv can create \
                     anything here — a fifo that never delivers, a device, a directory. Only a \
                     regular file is read."
                        .to_string(),
                    format!("replace {} with a regular file", path.display()),
                ));
            }
            Ok(file)
        }
        #[cfg(not(unix))]
        {
            std::fs::File::open(path).map_err(|e| SenvError::io(path, e))
        }
    }

    /// Copy a file without following a symlink at the source.
    ///
    /// The `symlink_metadata`-then-`copy` this replaces was a time-of-check to
    /// time-of-use gap: the check and the copy are two syscalls, and between
    /// them anything still running under a previous `senv run` — a dev server,
    /// a watcher, both of which this design calls ordinary — could swap the
    /// checked regular file for a link to `~/.ssh/id_ed25519`. `fs::copy`
    /// follows links, so senv would have carried the key into the staging
    /// directory, which is the install phase's own writable working directory
    /// and readable by every build backend that runs there.
    ///
    /// Opening with `O_NOFOLLOW` and copying from the descriptor makes the
    /// check and the read the same operation, so there is no window to win.
    pub fn copy_no_follow(src: &Path, dest: &Path) -> Result<()> {
        let mut input = open_no_follow(src)?;
        let mut output = create_new_no_follow(dest)?;
        std::io::copy(&mut input, &mut output).map_err(|e| SenvError::io(dest, e))?;
        Ok(())
    }

    /// The read-side counterpart of [`symlink_aware`].
    fn symlink_aware_read(path: &Path, e: std::io::Error) -> SenvError {
        #[cfg(unix)]
        if e.raw_os_error() == Some(libc::ELOOP) {
            return SenvError::refused(
                format!("{} is a symbolic link", path.display()),
                "senv will not read a file it parses as policy through a symlink: code running \
                 under senv can create one, and senv reads this file unconfined — so the link \
                 would make senv open something elsewhere on your disk and quote it back in an \
                 error message."
                    .to_string(),
                format!("replace {} with a regular file", path.display()),
            );
        }
        SenvError::io(path, e)
    }

    pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
        std::fs::write(path, contents).map_err(|e| SenvError::io(path, e))
    }

    /// Write a file, refusing to follow a symlink at the final component.
    ///
    /// Every file senv writes into a project — `pyproject.toml`, `uv.lock`,
    /// `.python-version`, `senv.toml` — sits in a directory the run phase
    /// grants read-write. A package that replaces one of them with a symlink
    /// turns senv's own next write into an arbitrary-file-write **outside** the
    /// sandbox, performed by senv, unconfined. That was a working exploit:
    /// planting `.senv.toml.tmp` as a symlink made `senv allow` overwrite a
    /// file elsewhere on the disk.
    ///
    /// `O_NOFOLLOW` fails with `ELOOP` on a symlink instead, which senv reports
    /// rather than papering over: a manifest that is a symlink is either a
    /// setup senv should not silently rewrite, or an attack.
    pub fn write_no_follow(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
        use std::io::Write;
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
                .map_err(|e| symlink_aware(path, e))?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::File::create(path).map_err(|e| SenvError::io(path, e))?;

        file.write_all(contents.as_ref())
            .map_err(|e| SenvError::io(path, e))
    }

    /// Create a new file that must not already exist and must not be a
    /// symlink. Used for the temp file behind an atomic write.
    pub fn create_new_no_follow(path: &Path) -> Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600)
                .open(path)
                .map_err(|e| symlink_aware(path, e))
        }
        #[cfg(not(unix))]
        {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| SenvError::io(path, e))
        }
    }

    /// Turn the kernel's `ELOOP` into an explanation, since "Too many levels of
    /// symbolic links" does not tell anyone what senv refused or why.
    fn symlink_aware(path: &Path, e: std::io::Error) -> SenvError {
        #[cfg(unix)]
        if e.raw_os_error() == Some(libc::ELOOP) {
            return SenvError::refused(
                format!("{} is a symbolic link", path.display()),
                "senv will not write through a symlink in your project: code running under \
                 senv can create one, which would turn this write into a write somewhere else \
                 on your disk."
                    .to_string(),
                format!("replace {} with a regular file", path.display()),
            );
        }
        SenvError::io(path, e)
    }

    pub fn create_dir_all(path: &Path) -> Result<()> {
        std::fs::create_dir_all(path).map_err(|e| SenvError::io(path, e))
    }

    pub fn remove_dir_all(path: &Path) -> Result<()> {
        std::fs::remove_dir_all(path).map_err(|e| SenvError::io(path, e))
    }

    pub fn canonicalize(path: &Path) -> Result<std::path::PathBuf> {
        std::fs::canonicalize(path).map_err(|e| SenvError::io(path, e))
    }
}

/// Attach a path to an `io::Error` at a call site that isn't one of the
/// wrappers above.
pub trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|e| SenvError::io(path, e))
    }
}
