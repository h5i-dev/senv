//! Command implementations.

pub mod inspect;
pub mod install;
pub mod run;

use crate::error::{Result, SenvError};
use crate::project::Project;
use std::path::PathBuf;

/// The refusal a project senv has never seen gets, when its policy already
/// grants more than the defaults.
fn first_sight_error(project: &Project, wide: &[String], format_changed: bool) -> SenvError {
    let list = wide
        .iter()
        .map(|w| format!("\n    + {w}"))
        .collect::<Vec<_>>()
        .join("");
    // After an upgrade the user *has* seen this project before, and being told
    // otherwise would read as senv having lost track of its own state.
    let (what, why) = if format_changed {
        (
            "senv reads policies in a new format and cannot compare the one it recorded here"
                .to_string(),
            format!(
                "the policy asks for:{list}\n  \
                 Re-confirming it once after an upgrade is the honest thing to do: senv will \
                 not carry forward an approval it can no longer check."
            ),
        )
    } else {
        (
            format!(
                "{} grants more than senv's defaults, and senv has not seen this project before",
                project.config_path.display()
            ),
            format!(
                "the policy asks for:{list}\n  \
                 senv has no earlier version of this project's policy to compare against, so \
                 it cannot tell a configuration you wrote from one that was written for you."
            ),
        )
    };
    SenvError::refused(
        what,
        why,
        "read the settings above; if they are what you want, run `senv trust` to accept them",
    )
}

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
        "this project's policy grants more than senv recorded, so nothing was run",
        format!(
            "the policy on disk is wider than the one you last accepted:{list}\n  \
             senv.toml and pyproject.toml both live in your project, which your code can \
             write — so a change neither you nor your tools made is worth looking at before \
             running anything."
        ),
        "if you made this change, run `senv trust` to accept it; if you did not, inspect \
         those files and your recent dependencies first",
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
        self.guard_trust_snapshot(project).map(|_| ())
    }

    /// [`Ctx::guard_trust`], returning the snapshot it checked and recorded.
    ///
    /// One read of `pyproject.toml`, compared and then stored. Reading it twice
    /// — once to compare, once to record — let anything editing the file in
    /// between have its `[build-system]` blessed without comparison. See
    /// [`Project::record_snapshot`].
    pub fn guard_trust_snapshot(&self, project: &Project) -> Result<crate::trust::PolicySnapshot> {
        let current = project.policy_snapshot();
        let verdict = project.verdict_against(&current);
        self.apply_verdict(project, &current, verdict)?;
        Ok(current)
    }

    fn apply_verdict(
        &self,
        project: &Project,
        current: &crate::trust::PolicySnapshot,
        verdict: crate::trust::Verdict,
    ) -> Result<()> {
        match verdict {
            crate::trust::Verdict::Widened(widenings) => Err(widened_error(&widenings)),
            // A first sighting has no baseline to compare against, so a policy
            // that grants more than senv's defaults has to be looked at rather
            // than adopted.
            //
            // This is what closes the nested-project bypass: the run phase can
            // write anywhere in the project, so a package can manufacture a
            // whole new project in a subdirectory the user plausibly cd's into
            // — `tests/`, say — with its own hostile `senv.toml`. That project
            // has no recorded state, so a permissive first sighting would adopt
            // it silently, and a `command:` secret there is unconfined host
            // execution.
            //
            // A snapshot from another senv version lands here too, with its own
            // sentence: after an upgrade the user *has* seen this project
            // before, and saying otherwise would read as senv losing track of
            // its own state.
            crate::trust::Verdict::FormatChanged | crate::trust::Verdict::FirstSight => {
                // `current`, not a fresh read: the snapshot that gets recorded
                // below must be the same one judged here.
                let wide = current.widenings(&crate::trust::PolicySnapshot::defaults());
                if wide.is_empty() {
                    return project.record_snapshot(current);
                }
                Err(first_sight_error(
                    project,
                    &wide,
                    matches!(verdict, crate::trust::Verdict::FormatChanged),
                ))
            }
            // A change that only narrows becomes the new baseline, so the next
            // widening is measured against what is on disk now.
            crate::trust::Verdict::Trusted => project.record_snapshot(current),
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
        if !self.json && self.project_dir.is_none() && !start.starts_with(&project.root) {
            eprintln!(
                "note: using {} — this directory belongs to that uv workspace.",
                project.root.display()
            );
        } else if !self.json && self.project_dir.is_none() && start != project.root {
            // Standing in a subdirectory is ordinary; standing in a *member* of
            // a workspace is worth one line, because the environment and the
            // lockfile belong to the root.
            if project.root.join("pyproject.toml").is_file()
                && start.join("pyproject.toml").is_file()
            {
                eprintln!(
                    "note: using the workspace at {} — its lockfile and environment cover \
                     every member.",
                    project.root.display()
                );
            }
        }
        if !self.json
            && self.project_dir.is_none()
            && let Some(outer) = project.enclosing_project()
        {
            eprintln!(
                "note: this project sits inside {}, which senv also tracks. Each has its own \
                 policy and environment — check you are in the one you meant.",
                outer.display()
            );
        }
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
