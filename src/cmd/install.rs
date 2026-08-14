//! The install boundary: `init`, `sync`, `add`, `remove`, `lock`, and the raw
//! `uv` passthrough.
//!
//! Everything here runs uv confined, with egress limited to package registries
//! and no secret in the environment. The only interesting decision is *where*
//! uv runs — see [`crate::uv`] for staging — and that decision is always
//! reported rather than assumed.

use serde::Serialize;
use std::path::PathBuf;

use super::Ctx;
use crate::error::{Result, SenvError, fs};
use crate::exec::{self, Execution};
use crate::policy::{self, Phase, Plan, PlanOptions};
use crate::project::{Project, Provenance, State, VenvLink};
use crate::util;
use crate::uv::{self, Staging};

/// What one install-phase command did.
#[derive(Debug, Serialize)]
pub struct InstallOutput {
    pub command: Vec<String>,
    pub tier: String,
    pub policy_digest: String,
    pub exit_code: i32,
    pub wall_ms: u64,
    /// Whether resolution happened in a staging copy rather than the project.
    pub staged: bool,
    /// Project files senv wrote back.
    pub changed: Vec<String>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
    pub blocked: Vec<String>,
}

// ── init ────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct InitOutput {
    pub project: String,
    pub state: String,
    pub venv: String,
    /// `created` or `adopted`.
    pub manifest: &'static str,
    pub python: Option<String>,
    pub venv_link: String,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<InstallOutput>,
}

pub fn init(ctx: &Ctx, args: &crate::cli::InitArgs) -> Result<i32> {
    let root = match &ctx.project_dir {
        Some(dir) => {
            fs::create_dir_all(dir)?;
            dir.clone()
        }
        None => std::env::current_dir().map_err(|e| SenvError::io(PathBuf::from("."), e))?,
    };

    // Check the policy before writing anything. `init` used to create
    // `pyproject.toml` and `.python-version` first, so a project it then
    // refused had already been modified.
    let existing = Project::at(&root).ok();
    if let Some(project) = &existing {
        project.ensure_dirs()?;
        ctx.guard_trust(project)?;
    }

    // Adoption, not scaffolding: an existing manifest is taken as it is.
    let manifest_path = root.join("pyproject.toml");
    let manifest = if manifest_path.is_file() {
        "adopted"
    } else {
        fs::write_no_follow(
            &manifest_path,
            new_manifest(&root, args.python.as_deref()).as_bytes(),
        )?;
        "created"
    };

    if let Some(python) = &args.python {
        // uv's own mechanism, so a teammate without senv gets the same
        // interpreter.
        fs::write_no_follow(
            &root.join(".python-version"),
            format!("{python}\n").as_bytes(),
        )?;
    }

    // Re-read: the manifest may have just been created, which changes what
    // `Project::at` discovers.
    let project = Project::at(&root)?;
    project.ensure_dirs()?;
    // `init` ends by running a sync, so it goes through the same gate as any
    // other executing command. Adopting a project is often the first sighting,
    // which the gate handles by recording a baseline rather than refusing.
    ctx.guard_trust(&project)?;

    let mut warnings = Vec::new();
    if args.replace_venv {
        project.replace_venv_directory()?;
    }
    let link = project.ensure_venv_link()?;
    if let Some(w) = link.warning() {
        warnings.push(w);
    }

    // An adopted .venv holds packages that never passed through the install
    // boundary. Record that honestly; the next sync replaces it.
    let mut state = project.load_state();
    state.version = State::VERSION;
    state.project_root = project.root.display().to_string();
    if state.provenance == Provenance::Absent && matches!(link, VenvLink::Directory) {
        state.provenance = Provenance::HostInstalled;
    }
    project.save_state(&state)?;

    let sync_output = if args.no_sync {
        None
    } else {
        Some(sync_inner(
            ctx,
            &project,
            &crate::cli::SyncArgs {
                frozen: false,
                extra: Vec::new(),
            },
        )?)
    };
    let exit = sync_output.as_ref().map(|s| s.exit_code).unwrap_or(0);

    let out = InitOutput {
        project: project.root.display().to_string(),
        state: project.state_dir.display().to_string(),
        venv: project.venv().display().to_string(),
        manifest,
        python: args.python.clone(),
        venv_link: format!("{:?}", project.venv_link_status()),
        warnings,
        sync: sync_output,
    };

    ctx.emit(&out, |o| {
        println!(
            "{} pyproject.toml",
            if o.manifest == "created" {
                "created"
            } else {
                "adopted"
            }
        );
        println!("  project      {}", o.project);
        println!("  environment  {}", o.venv);
        println!("  state        {}", o.state);
        if let Some(py) = &o.python {
            println!("  python       {py} (written to .python-version)");
        }
        for w in &o.warnings {
            eprintln!("warning: {}", exec::wrap(w, 74, 9));
        }
        match &o.sync {
            None => println!("\nrun `senv sync` to build the environment"),
            Some(sync) if sync.exit_code != 0 => {
                eprintln!(
                    "\nthe environment was not built: uv exited {}",
                    sync.exit_code
                )
            }
            Some(_) => println!("\nenvironment ready"),
        }
    })?;
    Ok(exit)
}

/// A minimal application-style manifest.
///
/// Deliberately without a `[build-system]`: uv then treats the project as an
/// application rather than a distributable package and does not build it during
/// sync. That keeps `senv sync` working with the project read-only, which is
/// the whole point of the install boundary. A user who wants a package adds the
/// build system themselves, and senv reports the read-only build failure with
/// the exact fix if their backend needs to write.
fn new_manifest(root: &std::path::Path, python: Option<&str>) -> String {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "app".to_string());
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let requires = python
        .map(|p| format!(">={p}"))
        .unwrap_or_else(|| ">=3.9".to_string());
    format!(
        "[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nrequires-python = \"{requires}\"\ndependencies = []\n"
    )
}

// ── sync ────────────────────────────────────────────────────────────────────

pub fn sync(ctx: &Ctx, args: &crate::cli::SyncArgs) -> Result<i32> {
    let project = ctx.project()?;
    let out = sync_inner(ctx, &project, args)?;
    let exit = out.exit_code;
    ctx.emit(&out, render_install)?;
    Ok(exit)
}

fn sync_inner(ctx: &Ctx, project: &Project, args: &crate::cli::SyncArgs) -> Result<InstallOutput> {
    let uv_bin = uv::find(project)?;
    let mut changed = Vec::new();
    let mut warnings = Vec::new();

    // A missing lockfile has to be produced before anything can be installed
    // from it, and producing one is a write to the project — so it goes through
    // staging rather than being smuggled into the sync.
    if !project.lock_path().is_file() && !args.frozen {
        if !ctx.json {
            eprintln!("no uv.lock yet — resolving dependencies first");
        }
        let locked = lock_inner(ctx, project, &uv_bin, false, false)?;
        changed.extend(locked.changed.iter().cloned());
        warnings.extend(locked.warnings.iter().cloned());
        if locked.exit_code != 0 {
            return Ok(locked);
        }
    }

    let mode = if args.frozen { "--frozen" } else { "--locked" };
    let mut argv = vec![
        uv_bin.display().to_string(),
        "sync".to_string(),
        mode.to_string(),
    ];
    argv.extend(args.extra.iter().cloned());
    // Pass 1 installs only the dependencies, which is where every third-party
    // build backend in your tree executes — and therefore where the
    // supply-chain risk actually lives. Your source is read-only for all of it.
    // Pass 2 (`argv`) then builds your own project.
    let mut deps_argv = argv.clone();
    deps_argv.insert(3, "--no-install-project".to_string());

    let plan = install_plan(project, &uv_bin, None, false)?;
    exec::print_notes(&plan.notes);
    let mut run = exec::run_captured(project, &plan, &deps_argv, Some("installing dependencies"))?;
    // Which pass failed decides whether the widening retry below is allowed to
    // happen at all.
    let mut deps_ok = run.succeeded();
    if deps_ok {
        run = exec::run_captured(project, &plan, &argv, Some("installing"))?;
    }

    // The install phase cannot download an interpreter: fetching one reaches
    // GitHub, which is not on the registries allowlist, and doing it inside a
    // phase that runs third-party build backends would be the wrong place
    // anyway. Provisioning is its own narrower phase, so senv runs that and
    // retries.
    if !run.succeeded() && needs_interpreter(&run.output) {
        match provision(ctx, project, &uv_bin) {
            Ok(0) => {
                run = exec::run_captured(project, &plan, &deps_argv, Some("installing"))?;
                deps_ok = run.succeeded();
                if deps_ok {
                    run = exec::run_captured(project, &plan, &argv, Some("installing"))?;
                }
            }
            Ok(_) => {}
            Err(e) => warnings.push(format!("could not install a Python interpreter: {e}")),
        }
    }

    // uv refuses to install from a lockfile that no longer matches the
    // manifest. That is the correct behaviour and a normal thing to hit after
    // editing pyproject.toml by hand, so senv re-locks (through staging) and
    // retries rather than making the user run a second command.
    if !run.succeeded() && lock_is_stale(&run.output) && !args.frozen {
        if !ctx.json {
            eprintln!("the lockfile is out of date — re-resolving");
        }
        let locked = lock_inner(ctx, project, &uv_bin, false, false)?;
        changed.extend(locked.changed.iter().cloned());
        warnings.extend(locked.warnings.iter().cloned());
        if locked.exit_code == 0 {
            run = exec::run_captured(project, &plan, &deps_argv, Some("installing"))?;
            deps_ok = run.succeeded();
            if deps_ok {
                run = exec::run_captured(project, &plan, &argv, Some("installing"))?;
            }
        }
    }

    // Your own project's backend is the only one still to run — the
    // dependencies are installed by now — so letting it write your source is a
    // far smaller grant than it looks, and for setuptools it is the only way
    // the project can be installed at all. Most backends (hatchling, flit,
    // pdm) never reach this path.
    //
    // The alternative was telling every setuptools user to set
    // `project-writable = true`, which is strictly worse: that hands write
    // access to all the dependency backends in pass 1 as well, to fix a write
    // only the project's own backend needs.
    let mut plan = plan;
    // `deps_ok` is load-bearing, not a tidiness check. Without it a *dependency*
    // whose build backend fails trying to write your source would match the
    // same signature, and senv would helpfully re-run that backend with the
    // project writable — handing an attacker exactly what this phase exists to
    // deny. The retry is only ever for the project's own backend, which runs
    // after the dependencies are already installed.
    if deps_ok && !run.succeeded() && read_only_project_hint(&run.output).is_some() {
        let writable = install_plan(project, &uv_bin, None, true)?;
        if !ctx.json {
            eprintln!(
                "note: {}",
                exec::wrap(
                    "your project's build backend needs to write into your source tree \
                     (setuptools creates *.egg-info there). Retrying that step with the \
                     project writable — the dependencies were already installed with it \
                     read-only.",
                    74,
                    6
                )
            );
        }
        let retry = exec::run_captured(project, &writable, &argv, Some("building the project"))?;
        if retry.succeeded() {
            warnings.push(
                "your project's own build backend ran with write access to your source; \
                 dependency build backends did not"
                    .to_string(),
            );
            run = retry;
            plan = writable;
        } else if let Some(hint) = read_only_project_hint(&run.output) {
            // Report the original read-only failure, which names the setting,
            // rather than a second one that reads as the same problem.
            warnings.push(hint);
        }
    }

    finish(project, &plan, run, argv, false, changed, warnings, true)
}

/// Did uv refuse because no suitable Python interpreter exists on this host?
fn needs_interpreter(output: &str) -> bool {
    const SIGNS: [&str; 3] = [
        "No interpreter found",
        "No virtual environment found",
        "can be installed with",
    ];
    output.contains("interpreter") && SIGNS.iter().any(|s| output.contains(s))
}

/// Download a Python interpreter, in the narrowest phase senv has.
///
/// Nothing of the project is involved and no third-party code runs: the only
/// writable path is the shared interpreter directory, and egress reaches the
/// python-build-standalone release hosts and nothing else. Every other phase
/// then gets that directory read-only.
fn provision(ctx: &Ctx, project: &Project, uv_bin: &std::path::Path) -> Result<i32> {
    let wanted = wanted_python(project);
    let mut argv = vec![
        uv_bin.display().to_string(),
        "python".to_string(),
        "install".to_string(),
    ];
    if let Some(v) = &wanted {
        argv.push(v.clone());
    }
    if !ctx.json {
        eprintln!(
            "no suitable Python found — installing {} into {}",
            wanted
                .clone()
                .unwrap_or_else(|| "the required version".to_string()),
            project.python_dir().display()
        );
    }
    let plan = policy::plan(
        project,
        Phase::Provision,
        &PlanOptions {
            uv: Some(uv_bin.to_path_buf()),
            ..Default::default()
        },
    )?;
    exec::print_notes(&plan.notes);
    let run = exec::run_captured(project, &plan, &argv, Some("installing python"))?;
    exec::emit_output(&run);
    Ok(run.exit_code)
}

/// The interpreter this project asks for: `senv.toml` first, then uv's own
/// `.python-version`.
fn wanted_python(project: &Project) -> Option<String> {
    // Both sources live in the project, which the run phase can write, and the
    // value becomes an argument to `uv python install`. A value that is not
    // version-shaped is dropped rather than passed on: `--mirror=https://evil`
    // in `.python-version` would otherwise choose where the interpreter senv
    // runs comes from. `[env] python` is validated at config load; this is the
    // same check for uv's own file, which senv does not control the format of.
    if let Some(v) = &project.config.env.python {
        return Some(v.clone());
    }
    let text = std::fs::read_to_string(project.root.join(".python-version")).ok()?;
    let first = text.lines().next()?.trim().to_string();
    if first.is_empty() {
        return None;
    }
    match crate::config::validate_python_request(&first) {
        Ok(()) => Some(first),
        Err(why) => {
            // Not fatal: uv reads this file itself and will report a bad value
            // in its own words. senv simply declines to forward it.
            eprintln!(
                "warning: ignoring .python-version — {}",
                exec::wrap(&why, 74, 9)
            );
            None
        }
    }
}

/// Did uv refuse because the lockfile no longer matches the manifest?
fn lock_is_stale(output: &str) -> bool {
    // Deliberately specific. A bare "does not exist" appears in plenty of
    // unrelated uv and Python messages, and matching it silently re-locked and
    // rewrote uv.lock on failure paths that had nothing to do with staleness.
    const SIGNS: [&str; 4] = [
        "needs to be updated",
        "not up-to-date",
        "Unable to find lockfile",
        "lockfile does not exist",
    ];
    SIGNS.iter().any(|s| output.contains(s))
}

/// A build backend that could not write into the read-only project tree. This
/// is the one predictable false refusal in the install boundary, so it gets a
/// named fix instead of a raw traceback.
fn read_only_project_hint(output: &str) -> Option<String> {
    let denied = output.contains("Read-only file system")
        || (output.contains("Permission denied") && output.contains("egg-info"))
        || output.contains("Permission denied: '");
    if !denied {
        return None;
    }
    Some(
        "the build backend tried to write into your project directory, which the install \
         boundary keeps read-only. If that write is legitimate, set [install] \
         project-writable = true in senv.toml — a dependency's build backend will then run \
         with write access to your source."
            .to_string(),
    )
}

// ── lock ────────────────────────────────────────────────────────────────────

pub fn lock(ctx: &Ctx, args: &crate::cli::LockArgs) -> Result<i32> {
    let project = ctx.project()?;
    let uv_bin = uv::find(&project)?;
    let out = lock_inner(ctx, &project, &uv_bin, args.check, args.in_place)?;
    let exit = out.exit_code;
    ctx.emit(&out, render_install)?;
    Ok(exit)
}

fn lock_inner(
    ctx: &Ctx,
    project: &Project,
    uv_bin: &std::path::Path,
    check: bool,
    in_place: bool,
) -> Result<InstallOutput> {
    let mut argv = vec![uv_bin.display().to_string(), "lock".to_string()];
    if check {
        // A check writes nothing, so it needs no staging and no write grant.
        argv.push("--check".to_string());
        let plan = install_plan(project, uv_bin, None, false)?;
        exec::print_notes(&plan.notes);
        let run = exec::run_captured(project, &plan, &argv, Some("checking the lockfile"))?;
        return finish(
            project,
            &plan,
            run,
            argv,
            false,
            Vec::new(),
            Vec::new(),
            false,
        );
    }
    staged_operation(ctx, project, uv_bin, argv, in_place, false)
}

// ── add / remove ────────────────────────────────────────────────────────────

pub fn add(ctx: &Ctx, args: &crate::cli::AddArgs) -> Result<i32> {
    let project = ctx.project()?;
    let uv_bin = uv::find(&project)?;
    let mut argv = vec![
        uv_bin.display().to_string(),
        "add".to_string(),
        "--no-sync".to_string(),
    ];
    if args.dev {
        argv.push("--dev".to_string());
    }
    if let Some(group) = &args.group {
        argv.push("--group".to_string());
        argv.push(group.clone());
    }
    argv.extend(args.packages.iter().cloned());
    mutate(ctx, &project, &uv_bin, argv, args.in_place)
}

pub fn remove(ctx: &Ctx, args: &crate::cli::RemoveArgs) -> Result<i32> {
    let project = ctx.project()?;
    let uv_bin = uv::find(&project)?;
    let mut argv = vec![
        uv_bin.display().to_string(),
        "remove".to_string(),
        "--no-sync".to_string(),
    ];
    if args.dev {
        argv.push("--dev".to_string());
    }
    if let Some(group) = &args.group {
        argv.push("--group".to_string());
        argv.push(group.clone());
    }
    argv.extend(args.packages.iter().cloned());
    mutate(ctx, &project, &uv_bin, argv, args.in_place)
}

/// Resolve the manifest change, then install it.
fn mutate(
    ctx: &Ctx,
    project: &Project,
    uv_bin: &std::path::Path,
    argv: Vec<String>,
    in_place: bool,
) -> Result<i32> {
    let resolved = staged_operation(ctx, project, uv_bin, argv, in_place, true)?;
    if resolved.exit_code != 0 {
        let exit = resolved.exit_code;
        ctx.emit(&resolved, render_install)?;
        return Ok(exit);
    }

    let mut synced = sync_inner(
        ctx,
        project,
        &crate::cli::SyncArgs {
            frozen: true,
            extra: Vec::new(),
        },
    )?;
    // Present it as one operation: the user asked to add a package, not to run
    // two phases.
    synced.changed = resolved.changed.clone();
    synced.staged = resolved.staged;
    let mut warnings = resolved.warnings.clone();
    warnings.extend(synced.warnings.iter().cloned());
    synced.warnings = warnings;

    let exit = synced.exit_code;
    ctx.emit(&synced, render_install)?;
    Ok(exit)
}

/// Run a uv command that writes manifests, in a staging copy when possible.
fn staged_operation(
    ctx: &Ctx,
    project: &Project,
    uv_bin: &std::path::Path,
    argv: Vec<String>,
    force_in_place: bool,
    expect_manifest_change: bool,
) -> Result<InstallOutput> {
    let manifest = fs::read_to_string_bounded(&project.pyproject_path()).unwrap_or_default();
    let staging = if force_in_place {
        Staging::Impossible("--in-place was requested".to_string())
    } else {
        uv::staging_for(&manifest)
    };

    let before_manifest = util::sha256_file(&project.pyproject_path());
    let before_lock = util::sha256_file(&project.lock_path());
    let mut warnings = Vec::new();
    match staging.reason() {
        None => {
            let stage = uv::prepare_stage(project)?;
            let plan = install_plan(project, uv_bin, Some(stage.dir.clone()), false)?;
            exec::print_notes(&plan.notes);
            let run = exec::run_captured(project, &plan, &argv, Some("resolving"))?;

            let applied = if run.succeeded() {
                uv::apply_stage(project, &stage, expect_manifest_change)?
            } else {
                Default::default()
            };
            warnings.extend(applied.warnings);
            finish(
                project,
                &plan,
                run,
                argv,
                true,
                applied.changed,
                warnings,
                false,
            )
        }
        Some(reason) => {
            if !ctx.json {
                eprintln!(
                    "note: {}",
                    exec::wrap(
                        &format!(
                            "resolving in the project directory rather than a staging copy: \
                             {reason}. Dependency build backends run with write access to your \
                             source for this command."
                        ),
                        74,
                        6
                    )
                );
            }
            warnings.push(format!("resolved in-place ({reason})"));
            let plan = install_plan(project, uv_bin, None, true)?;
            exec::print_notes(&plan.notes);
            let run = exec::run_captured(project, &plan, &argv, Some("resolving"))?;
            // Measured, not assumed. Reporting both files unconditionally
            // told workspace users that senv had rewritten their manifest when
            // it had not touched it.
            let changed = if run.succeeded() {
                [
                    ("pyproject.toml", project.pyproject_path(), before_manifest),
                    ("uv.lock", project.lock_path(), before_lock),
                ]
                .into_iter()
                .filter(|(_, path, before)| util::sha256_file(path) != *before)
                .map(|(name, _, _)| name.to_string())
                .collect()
            } else {
                Vec::new()
            };
            finish(project, &plan, run, argv, false, changed, warnings, false)
        }
    }
}

// ── uv passthrough ──────────────────────────────────────────────────────────

pub fn passthrough(ctx: &Ctx, args: &crate::cli::UvArgs) -> Result<i32> {
    let project = ctx.project()?;
    let uv_bin = uv::find(&project)?;
    let mut argv = vec![uv_bin.display().to_string()];
    argv.extend(args.argv.iter().cloned());

    let plan = install_plan(&project, &uv_bin, None, args.in_place)?;
    exec::print_notes(&plan.notes);
    let run = exec::run_captured(&project, &plan, &argv, Some("running uv"))?;
    let out = finish(
        &project,
        &plan,
        run,
        argv,
        false,
        Vec::new(),
        Vec::new(),
        false,
    )?;
    let exit = out.exit_code;
    ctx.emit(&out, render_install)?;
    Ok(exit)
}

// ── shared ──────────────────────────────────────────────────────────────────

/// Empty a phase's temp directory before the phase runs.
///
/// `TMPDIR` for each phase lives under senv's state and nothing ever cleaned
/// it, so every crashed test run left its temp files there permanently. Doing
/// it at the start rather than the end means a crash still leaves the evidence
/// available until the next run.
fn clear_tmp(project: &Project, phase: &str) {
    let dir = project.tmp(phase);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
}

fn install_plan(
    project: &Project,
    uv_bin: &std::path::Path,
    work_override: Option<PathBuf>,
    project_writable: bool,
) -> Result<Plan> {
    clear_tmp(project, "install");
    policy::plan(
        project,
        Phase::Install,
        &PlanOptions {
            project_writable,
            work_override,
            extra_hosts: Vec::new(),
            uv: Some(uv_bin.to_path_buf()),
        },
    )
}

/// Print output, persist state, and build the command's result.
#[allow(clippy::too_many_arguments)]
fn finish(
    project: &Project,
    plan: &Plan,
    run: Execution,
    argv: Vec<String>,
    staged: bool,
    changed: Vec<String>,
    warnings: Vec<String>,
    record_sync: bool,
) -> Result<InstallOutput> {
    exec::emit_output(&run);
    exec::print_denials(&run.record);

    // Link `.venv` only once there is something behind it. Linking first left a
    // dangling symlink in a project that never had one when the first sync
    // failed, breaking editors and `source .venv/bin/activate`.
    if run.succeeded() && record_sync {
        let _ = project.ensure_venv_link();
    }
    if run.succeeded() && record_sync {
        let mut state = project.load_state();
        state.version = State::VERSION;
        state.project_root = project.root.display().to_string();
        state.provenance = Provenance::Sandboxed;
        state.lock_hash = util::sha256_file(&project.lock_path());
        state.last_sync_ms = Some(util::now_ms());
        state.install_digest = Some(plan.digest.clone());
        state.python = read_venv_python(project);
        project.save_state(&state)?;
    }

    // The resolved policy is written for inspection, never read back as input:
    // senv recompiles it every time and compares digests.
    if let Ok(text) = plan.policy.to_toml() {
        let _ = fs::write(
            &project.policy_path(plan.phase.policy_name()),
            text.as_bytes(),
        );
    }

    Ok(InstallOutput {
        command: argv.iter().skip(1).cloned().collect(),
        tier: plan.tier().as_str().to_string(),
        policy_digest: plan.digest.clone(),
        exit_code: run.exit_code,
        wall_ms: run.wall_ms,
        staged,
        changed,
        warnings,
        notes: plan.notes.iter().map(|n| n.text.clone()).collect(),
        blocked: run
            .record
            .denials
            .iter()
            .filter(|d| d.verdict.is_suggestable())
            .map(|d| d.target.clone())
            .collect(),
    })
}

/// The interpreter version recorded in the environment's `pyvenv.cfg`.
fn read_venv_python(project: &Project) -> Option<String> {
    let text = std::fs::read_to_string(project.venv().join("pyvenv.cfg")).ok()?;
    text.lines()
        .find_map(|l| {
            l.strip_prefix("version_info ")
                .or_else(|| l.strip_prefix("version "))
        })
        .map(|v| v.trim_start_matches('=').trim().to_string())
}

fn render_install(o: &InstallOutput) {
    for w in &o.warnings {
        eprintln!("warning: {}", exec::wrap(w, 74, 9));
    }
    if !o.changed.is_empty() {
        eprintln!("updated {}", o.changed.join(", "));
    }
    // senv's own progress line goes to stderr: stdout belongs to the command,
    // and `senv uv -- export > reqs.txt` must produce requirements.
    if o.exit_code == 0 {
        eprintln!(
            "ok  uv {}  ({} tier, {:.1}s)",
            o.command.first().map(String::as_str).unwrap_or(""),
            o.tier,
            o.wall_ms as f64 / 1000.0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_manifest_is_valid_and_names_the_directory() {
        let manifest = new_manifest(std::path::Path::new("/tmp/my project"), Some("3.13"));
        let parsed: toml::Value = toml::from_str(&manifest).expect("valid TOML");
        assert_eq!(parsed["project"]["name"].as_str(), Some("my-project"));
        assert_eq!(
            parsed["project"]["requires-python"].as_str(),
            Some(">=3.13")
        );
        // No build system: uv then does not build the project during sync, so
        // a read-only project tree is enough for the common case.
        assert!(parsed.get("build-system").is_none());
    }

    #[test]
    fn a_hostile_python_version_file_is_not_forwarded_to_uv() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='x'\nversion='0'\n",
        )
        .unwrap();
        unsafe { std::env::set_var("SENV_STATE_DIR", tmp.path().join("state")) };
        unsafe { std::env::set_var("SENV_CACHE_DIR", tmp.path().join("cache")) };
        let project = Project::at(&root).unwrap();

        std::fs::write(
            root.join(".python-version"),
            "--mirror=https://evil.example\n",
        )
        .unwrap();
        assert_eq!(
            wanted_python(&project),
            None,
            "a flag must never reach uv's argv"
        );

        std::fs::write(root.join(".python-version"), "3.13\n").unwrap();
        assert_eq!(wanted_python(&project), Some("3.13".to_string()));
    }

    #[test]
    fn a_missing_interpreter_is_recognised_so_it_can_be_provisioned() {
        assert!(needs_interpreter(
            "error: No interpreter found for Python 3.13 in managed installations or search path"
        ));
        assert!(!needs_interpreter("Resolved 12 packages in 300ms"));
        assert!(!needs_interpreter("error: Failed to build `demo`"));
    }

    #[test]
    fn uv_staleness_messages_are_recognised() {
        assert!(lock_is_stale(
            "error: The lockfile at `uv.lock` needs to be updated, but `--locked` was provided."
        ));
        assert!(lock_is_stale(
            "error: Unable to find lockfile at `uv.lock`."
        ));
        assert!(!lock_is_stale("Resolved 12 packages in 300ms"));
    }

    #[test]
    fn a_read_only_build_failure_names_the_setting_that_fixes_it() {
        let out = "error: Failed to build `demo`\n  Caused by: [Errno 30] Read-only file \
                   system: '/home/u/proj/demo.egg-info'";
        let hint = read_only_project_hint(out).expect("must recognise this");
        assert!(hint.contains("project-writable"), "{hint}");
        assert!(read_only_project_hint("Installed 12 packages").is_none());
    }
}
