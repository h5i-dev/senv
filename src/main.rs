//! senv — a security boundary for Python environments.
//!
//! See `DESIGN.md` for the architecture and threat model. In short: `uv` and
//! your code run inside an h5i sandbox, with different policies for installing
//! (registries only, no secrets, your source read-only) and for running (no
//! network, environment read-only, credentials unreachable).

mod cli;
mod cmd;
mod config;
mod error;
mod exec;
mod policy;
mod project;
mod receipt;
mod util;
mod uv;

use clap::Parser;

use cli::{Cli, Command};
use cmd::Ctx;
use error::EXIT_SENV_ERROR;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    // Before anything asks what this host can enforce. See the function's docs:
    // concurrent senv processes can otherwise talk each other out of a tier
    // the host actually supports.
    policy::warm_host_probes();
    let ctx = Ctx {
        json: cli.json,
        project_dir: cli.project.clone(),
    };

    let result = match &cli.command {
        Command::Init(args) => cmd::install::init(&ctx, args),
        Command::Add(args) => cmd::install::add(&ctx, args),
        Command::Remove(args) => cmd::install::remove(&ctx, args),
        Command::Sync(args) => cmd::install::sync(&ctx, args),
        Command::Lock(args) => cmd::install::lock(&ctx, args),
        Command::Run(args) => cmd::run::run(&ctx, args),
        Command::Shell(args) => cmd::run::shell(&ctx, args),
        Command::Status => cmd::inspect::status(&ctx),
        Command::Report(args) => cmd::inspect::report(&ctx, args),
        Command::Allow(args) => cmd::inspect::allow(&ctx, args),
        Command::Doctor => cmd::inspect::doctor(&ctx),
        Command::Gc(args) => cmd::inspect::gc(&ctx, args),
        Command::UvPassthrough(args) => cmd::install::passthrough(&ctx, args),
    };

    match result {
        // A confined command's own exit code passes through untouched, so
        // `senv run pytest` is a drop-in for `uv run pytest` in a CI script.
        Ok(code) => std::process::ExitCode::from(clamp_exit(code)),
        Err(e) => {
            report_error(&e, cli.json);
            std::process::ExitCode::from(EXIT_SENV_ERROR as u8)
        }
    }
}

/// Exit codes are a byte. A child killed by a signal, or one returning
/// something out of range, must still produce a non-zero code rather than
/// wrapping around to 0 and reporting success.
fn clamp_exit(code: i32) -> u8 {
    match u8::try_from(code) {
        Ok(0) => 0,
        Ok(c) => c,
        Err(_) => 1,
    }
}

/// Print a senv error: what happened, why, and the one change that would fix
/// it.
fn report_error(e: &error::SenvError, json: bool) {
    if json {
        let payload = serde_json::json!({
            "error": e.to_string(),
            "fix": e.fix(),
        });
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        return;
    }
    eprintln!("senv: {e}");
    if let Some(fix) = e.fix() {
        eprintln!("  → {}", exec::wrap(fix, 74, 4));
    }
}
