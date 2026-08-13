//! The command-line surface.
//!
//! Shaped after uv on purpose: `sync`, `add`, `lock`, `run` mean what a uv user
//! already expects them to mean, and their flags pass straight through. The
//! migration cost of senv should be typing four different letters, not learning
//! a tool.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "senv",
    version,
    about = "A security boundary for Python environments.",
    long_about = "senv runs uv and your code inside an OS-level sandbox.\n\n\
                  Installs reach package registries and nothing else, and cannot write to your \
                  source tree. Your code runs with no network by default, an environment it \
                  cannot modify, and no access to your credentials.\n\n\
                  A senv project stays a valid uv project: senv adds no files to your tree \
                  beyond an optional senv.toml.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of prose.
    #[arg(long, global = true)]
    pub json: bool,

    /// Operate on this project instead of searching upward from the current
    /// directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub project: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create or adopt a project and build its environment.
    Init(InitArgs),
    /// Add dependencies (resolves in a staging copy, then syncs).
    Add(AddArgs),
    /// Remove dependencies.
    Remove(RemoveArgs),
    /// Install the locked dependencies into the environment.
    Sync(SyncArgs),
    /// Update the lockfile without touching the environment.
    Lock(LockArgs),
    /// Run a command inside the run boundary.
    Run(RunArgs),
    /// Open an interactive shell inside the run boundary.
    Shell(ShellArgs),
    /// Show the environment and the policy that is enforced.
    Status,
    /// Show what ran, what was denied, and what was redacted.
    Report(ReportArgs),
    /// Allow the run phase to reach a host.
    Allow(AllowArgs),
    /// Accept the current senv.toml as the trusted baseline.
    Trust,
    /// Report what this host can enforce.
    Doctor,
    /// Remove state for projects that no longer exist, and prune caches.
    Gc(GcArgs),
    /// Run an arbitrary uv command inside the install boundary.
    #[command(name = "uv")]
    UvPassthrough(UvArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Python version for uv to use, e.g. 3.13. Written to .python-version,
    /// which is uv's own mechanism.
    #[arg(long, value_name = "VERSION")]
    pub python: Option<String>,

    /// Replace an existing .venv directory with a link to senv's environment.
    /// Without this, senv leaves an environment it did not create alone.
    #[arg(long)]
    pub replace_venv: bool,

    /// Set up the project without building the environment.
    #[arg(long)]
    pub no_sync: bool,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Packages to add, in any form uv accepts.
    #[arg(required = true, value_name = "PACKAGE")]
    pub packages: Vec<String>,

    /// Add to a dependency group (uv's --group).
    #[arg(long, value_name = "GROUP")]
    pub group: Option<String>,

    /// Add as a development dependency.
    #[arg(long)]
    pub dev: bool,

    /// Resolve in the project directory instead of a staging copy, giving the
    /// install phase write access to your source. Announced and recorded.
    #[arg(long)]
    pub in_place: bool,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    #[arg(required = true, value_name = "PACKAGE")]
    pub packages: Vec<String>,

    #[arg(long, value_name = "GROUP")]
    pub group: Option<String>,

    #[arg(long)]
    pub dev: bool,

    /// See `senv add --in-place`.
    #[arg(long)]
    pub in_place: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Fail instead of updating a stale lockfile.
    #[arg(long)]
    pub frozen: bool,

    /// Extra arguments for uv sync.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "UV_ARGS"
    )]
    pub extra: Vec<String>,
}

#[derive(Debug, Args)]
pub struct LockArgs {
    /// Check that the lockfile is current without writing it.
    #[arg(long)]
    pub check: bool,

    /// See `senv add --in-place`.
    #[arg(long)]
    pub in_place: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Let this command reach a host, for this invocation only. Repeatable.
    #[arg(long = "allow-net", value_name = "HOST")]
    pub allow_net: Vec<String>,

    /// The command and its arguments.
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    pub argv: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ShellArgs {
    /// See `senv run --allow-net`.
    #[arg(long = "allow-net", value_name = "HOST")]
    pub allow_net: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    /// How many recent commands to show.
    #[arg(long, default_value_t = 20, value_name = "N")]
    pub limit: usize,

    /// Print a senv.toml stanza that would allow everything recorded as
    /// blocked. Nothing is written; applying it stays an explicit edit.
    #[arg(long)]
    pub suggest: bool,
}

#[derive(Debug, Args)]
pub struct AllowArgs {
    /// Host to allow, e.g. api.example.com or *.example.com (optionally with
    /// :port).
    #[arg(required = true, value_name = "HOST")]
    pub hosts: Vec<String>,
}

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Actually delete. Without it, gc only reports what it would remove.
    #[arg(long)]
    pub prune: bool,

    /// Include this project's wheel cache.
    #[arg(long)]
    pub cache: bool,
}

#[derive(Debug, Args)]
pub struct UvArgs {
    /// Arguments passed to uv unchanged.
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    pub argv: Vec<String>,

    /// See `senv add --in-place`.
    #[arg(long)]
    pub in_place: bool,
}
