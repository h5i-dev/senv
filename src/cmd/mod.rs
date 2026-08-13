//! Command implementations.

pub mod inspect;
pub mod install;
pub mod run;

use crate::error::{Result, SenvError};
use crate::project::Project;
use std::path::PathBuf;

/// The refusal a widened configuration gets.
///
/// Long on purpose. It has to distinguish "you edited this and forgot" from
/// "something edited this for you", and the reader cannot tell which without
/// seeing exactly what changed.
fn widened_error(widenings: &[String]) -> SenvError {
    let list = widenings
        .iter()
        .map(|w| format!("\n    + {w}"))
        .collect::<Vec<_>>()
        .join("");
    SenvError::refused(
        "senv.toml grants more than senv recorded, so nothing was run",
        format!(
            "the policy on disk is wider than the one you last accepted:{list}\n               senv.toml lives in your project, which your code can write — so a change it did \
             not make is a change worth looking at before running anything."
        ),
        "if you made this change, run `senv trust` to accept it; if you did not, inspect \
         senv.toml and your recent dependencies first",
    )
}

/// Shared invocation context.
pub struct Ctx {
    pub json: bool,
    pub project_dir: Option<PathBuf>,
}

impl Ctx {
    /// Find the project for a command that will **execute** something.
    ///
    /// Refuses when the configuration on disk grants more than the snapshot
    /// senv recorded, because `senv.toml` sits inside a directory the run
    /// phase can write. See [`crate::trust`].
    pub fn project(&self) -> Result<Project> {
        let project = self.project_unchecked()?;
        self.guard_trust(&project)?;
        Ok(project)
    }

    /// Apply the trust verdict for a command that is about to execute
    /// something: refuse a widened policy, adopt anything else as the new
    /// baseline.
    ///
    /// Separate from [`Ctx::project`] because `senv init` builds its project
    /// directly (it creates the marker rather than finding it) and must not be
    /// the one command that skips the check — it ends by running a sync.
    pub fn guard_trust(&self, project: &Project) -> Result<()> {
        match project.trust_verdict() {
            crate::trust::Verdict::Widened(widenings) => Err(widened_error(&widenings)),
            // A first sighting is adopted rather than refused: reaching it
            // means the user chose to work in this project, and a senv.toml
            // that arrived with the repository is trusted exactly as much as
            // the code beside it. It is still worth saying out loud when that
            // config grants more than the defaults.
            crate::trust::Verdict::FirstSight => {
                if !self.json {
                    let wide = crate::trust::PolicySnapshot::of(&project.config)
                        .widenings(&crate::trust::PolicySnapshot::default());
                    if !wide.is_empty() {
                        eprintln!(
                            "note: {} grants more than senv's defaults:",
                            project.config_path.display()
                        );
                        for w in &wide {
                            eprintln!("  + {w}");
                        }
                        eprintln!(
                            "  Accepted as this project's baseline; later widening needs `senv trust`."
                        );
                        eprintln!();
                    }
                }
                project.record_trust()
            }
            // A change that only narrows becomes the new baseline, so the next
            // widening is measured against what is on disk now.
            crate::trust::Verdict::Trusted => project.record_trust(),
        }
    }

    /// Find the project without the trust check, for commands that only look.
    ///
    /// `status`, `report` and `doctor` must keep working on a project whose
    /// policy is under suspicion — refusing to *describe* a tampered
    /// configuration would remove the one tool for understanding it. They warn
    /// instead.
    pub fn project_unchecked(&self) -> Result<Project> {
        let start = match &self.project_dir {
            Some(dir) => dir.clone(),
            None => std::env::current_dir()
                .map_err(|e| crate::error::SenvError::io(PathBuf::from("."), e))?,
        };
        let project = Project::discover(&start)?;
        project.ensure_dirs()?;
        Ok(project)
    }

    /// Print the trust warning for an inspecting command, if there is one.
    pub fn warn_if_untrusted(&self, project: &Project) {
        if self.json {
            return;
        }
        let verdict = project.trust_verdict();
        if !verdict.is_widened() {
            return;
        }
        eprintln!(
            "warning: {} grants more than senv recorded:",
            project.config_path.display()
        );
        for w in verdict.widenings() {
            eprintln!("  + {w}");
        }
        eprintln!("  Commands that execute anything are refused until you run `senv trust`.");
        eprintln!();
    }

    /// Print a value as JSON, or hand it to `render` for prose.
    pub fn emit<T: serde::Serialize>(&self, value: &T, render: impl FnOnce(&T)) -> Result<()> {
        if self.json {
            let text = serde_json::to_string_pretty(value).map_err(|e| {
                crate::error::SenvError::internal(format!("serializing output: {e}"))
            })?;
            println!("{text}");
        } else {
            render(value);
        }
        Ok(())
    }
}
