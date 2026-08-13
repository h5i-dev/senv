//! Command implementations.

pub mod inspect;
pub mod install;
pub mod run;

use crate::error::Result;
use crate::project::Project;
use std::path::PathBuf;

/// Shared invocation context.
pub struct Ctx {
    pub json: bool,
    pub project_dir: Option<PathBuf>,
}

impl Ctx {
    /// Find the project this command applies to.
    pub fn project(&self) -> Result<Project> {
        let start = match &self.project_dir {
            Some(dir) => dir.clone(),
            None => std::env::current_dir()
                .map_err(|e| crate::error::SenvError::io(PathBuf::from("."), e))?,
        };
        let project = Project::discover(&start)?;
        project.ensure_dirs()?;
        Ok(project)
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
