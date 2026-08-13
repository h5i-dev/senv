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

    pub fn read_to_string(path: &Path) -> Result<String> {
        std::fs::read_to_string(path).map_err(|e| SenvError::io(path, e))
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

    pub fn copy(from: &Path, to: &Path) -> Result<u64> {
        std::fs::copy(from, to).map_err(|e| SenvError::io(from, e))
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
