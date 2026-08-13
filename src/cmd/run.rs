//! The run boundary: `senv run` and `senv shell`.
//!
//! Your code, your project, no network by default, and an environment it cannot
//! modify. Output streams straight to the terminal and the child's exit code
//! passes through unchanged, so `senv run pytest` is a drop-in for `uv run
//! pytest` in a CI script.

use serde::Serialize;

use super::Ctx;
use crate::error::{Result, SenvError};
use crate::exec;
use crate::policy::{self, Phase, PlanOptions};
use crate::project::Project;

#[derive(Debug, Serialize)]
pub struct RunOutput {
    pub argv: Vec<String>,
    pub tier: String,
    pub policy_digest: String,
    pub exit_code: i32,
    pub wall_ms: u64,
}

pub fn run(ctx: &Ctx, args: &crate::cli::RunArgs) -> Result<i32> {
    let project = ctx.project()?;
    exec::require_venv(&project)?;
    warn_if_stale(ctx, &project);

    for host in &args.allow_net {
        crate::config::validate_host_pattern(host).map_err(|e| {
            SenvError::refused("--allow-net", e, "use a hostname like api.example.com")
        })?;
    }

    let plan = policy::plan(
        &project,
        Phase::Run,
        &PlanOptions {
            extra_hosts: args.allow_net.clone(),
            ..Default::default()
        },
    )?;
    exec::print_notes(&plan.notes);

    // Resolve the program inside the environment ourselves: a bare `pytest`
    // that is not installed should produce senv's message naming the
    // environment, not a bare `command not found` from the sandbox.
    let mut argv = args.argv.clone();
    if let Some(resolved) = exec::venv_program(&project, &argv[0]) {
        argv[0] = resolved;
    } else if !argv[0].contains('/') && !is_system_tool(&argv[0]) {
        return Err(SenvError::refused(
            format!("`{}` is not installed in this environment", argv[0]),
            format!("looked in {}", project.venv().join("bin").display()),
            format!("add it with `senv add {}`", argv[0]),
        ));
    }

    let run = exec::run_streaming(&project, &plan, &argv, true)?;
    exec::print_denials(&run.record);
    persist_digest(&project, &plan.digest);

    let out = RunOutput {
        argv: args.argv.clone(),
        tier: plan.tier().as_str().to_string(),
        policy_digest: plan.digest.clone(),
        exit_code: run.exit_code,
        wall_ms: run.wall_ms,
    };
    if ctx.json {
        ctx.emit(&out, |_| {})?;
    }
    Ok(run.exit_code)
}

pub fn shell(ctx: &Ctx, args: &crate::cli::ShellArgs) -> Result<i32> {
    let project = ctx.project()?;
    exec::require_venv(&project)?;
    warn_if_stale(ctx, &project);

    let plan = policy::plan(
        &project,
        Phase::Shell,
        &PlanOptions {
            extra_hosts: args.allow_net.clone(),
            ..Default::default()
        },
    )?;
    exec::print_notes(&plan.notes);

    let shell = pick_shell();
    // `--norc --noprofile`: the host's rc files routinely invoke tools the
    // policy does not grant, and a shell that dies on a prompt hook is a
    // confusing first impression of the sandbox. The prompt below says where
    // you are instead.
    let argv = if shell.ends_with("bash") {
        vec![
            shell.clone(),
            "--norc".into(),
            "--noprofile".into(),
            "-i".into(),
        ]
    } else {
        vec![shell.clone(), "-i".into()]
    };

    if !ctx.json {
        eprintln!(
            "senv shell — {} tier, network {}. Type `exit` to leave.",
            plan.tier().as_str(),
            project.config.run.net.describe()
        );
    }

    let mut plan = plan;
    let name = project
        .root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "senv".into());
    plan.env
        .push(("PS1".to_string(), format!("(senv:{name}) \\w $ ")));
    plan.env.push(("SENV_SHELL".to_string(), "1".to_string()));

    // No stderr tee for an interactive shell: the child owns the terminal.
    let run = exec::run_streaming(&project, &plan, &argv, false)?;
    exec::print_denials(&run.record);
    persist_digest(&project, &plan.digest);
    Ok(run.exit_code)
}

/// Tools that come from the system rather than the environment, so a bare name
/// is legitimate even though it is not in `.venv/bin`.
fn is_system_tool(program: &str) -> bool {
    matches!(
        program,
        "python" | "python3" | "sh" | "bash" | "env" | "ls" | "cat" | "echo" | "make" | "git"
    )
}

fn pick_shell() -> String {
    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh"] {
        if std::path::Path::new(candidate).exists() {
            return candidate.to_string();
        }
    }
    "/bin/sh".to_string()
}

/// Tell the user when the environment no longer matches the lockfile. Not an
/// error: they may be mid-edit, and blocking here would be senv getting in the
/// way of work.
fn warn_if_stale(ctx: &Ctx, project: &Project) {
    if ctx.json {
        return;
    }
    let state = project.load_state();
    if project.lock_is_current(&state) == Some(false) {
        eprintln!("warning: uv.lock has changed since the last sync — run `senv sync`");
    }
    if let Some(w) = project.venv_link_status().warning() {
        eprintln!("warning: {}", exec::wrap(&w, 74, 9));
    }
}

fn persist_digest(project: &Project, digest: &str) {
    let mut state = project.load_state();
    state.run_digest = Some(digest.to_string());
    let _ = project.save_state(&state);
}
