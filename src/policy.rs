//! Compiling `senv.toml` into enforced h5i policies.
//!
//! This is senv's core idea in code: **installation and execution are different
//! trust problems, so they get different policies.**
//!
//! - [`Phase::Provision`] downloads a Python interpreter. No third-party code
//!   runs; the only writable path is the interpreter directory.
//! - [`Phase::Install`] runs `uv`, and therefore runs the build backends of
//!   every source distribution in your dependency tree. Egress is limited to
//!   package registries, the project is read-only, writes are confined to the
//!   environment and the wheel cache, and **no secret is ever injected**.
//! - [`Phase::Run`] runs your code. The project is writable, the environment is
//!   read-only, and the network is denied unless you opened it.
//!
//! Every profile is built by *narrowing* h5i's fail-closed `Profile::builtin`
//! rather than by constructing one from scratch. That is deliberate: h5i's own
//! history includes a bug where a hand-written empty list silently widened a
//! profile, and inheriting the base means a field senv forgets stays safe
//! instead of becoming empty.

use std::path::{Path, PathBuf};

use h5i_sandbox::sandbox;
use h5i_sandbox::sandbox_policy::{IsolationClaim, NetMode, Profile, ResolvedPolicy, SecretGrant};

use crate::config::{Config, InstallNet};
use crate::error::{Result, SenvError};
use crate::project::Project;
use crate::util;

/// PyPI, and the CDN it redirects downloads to. Both are needed for any
/// install; `pypi.org` alone gets you a resolution and no wheels.
pub const PYPI_HOSTS: [&str; 2] = ["pypi.org", "files.pythonhosted.org"];

/// Where `uv python install` fetches interpreters from (python-build-standalone
/// releases, served by GitHub). Scoped to the provisioning phase, which runs no
/// third-party code.
pub const PYTHON_DOWNLOAD_HOSTS: [&str; 5] = [
    "github.com",
    "api.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "astral.sh",
];

/// Credential files that must never sit inside a grant. h5i's defaults cover
/// `~/.ssh`, `~/.aws` and `~/.config/gh`; these add the ones a Python
/// toolchain would plausibly be pointed at. They are a *lint*: Landlock cannot
/// subtract a child from a granted parent, so a policy that would expose one is
/// refused outright.
const EXTRA_DENY: [&str; 7] = [
    "~/.netrc",
    "~/.pypirc",
    "~/.git-credentials",
    "~/.docker/config.json",
    "~/.config/gcloud",
    "~/.kube/config",
    // Not a credential — a code-execution path, and the only one of these that
    // is. Apple's system Python is built with `sys.pycache_prefix` preset to
    // this directory, so it caches every module's bytecode *there* instead of in
    // `__pycache__` beside the source. The run phase's read-only environment
    // assumes the opposite (see the note in `run_profile`): make this directory
    // writable and a package gets an authoritative, writable copy of every
    // module in a read-only environment, which is exactly the persistence hole
    // that dropping `PYTHONPYCACHEPREFIX` closed. It is ungranted today, so this
    // is not a live bug — it is a `~/Library/Caches` grant away from being one,
    // and that is an ordinary thing for a macOS user to want.
    "~/Library/Caches/com.apple.python",
];

/// A finite stand-in for `wall = "none"`.
///
/// h5i refuses to express an unbounded wall clock, and it is right to: a
/// confined command that can never be killed is a resource leak with a policy
/// file. senv resolves the dev-server case without weakening that — one year is
/// longer than any `uvicorn` session and still a real, digested kill switch.
const UNBOUNDED_WALL_SECS: u64 = 365 * 24 * 60 * 60;

/// The ceilings a phase runs under when its config sets none.
///
/// Named because [`crate::trust`] has to reason about them: an omitted setting
/// is not "no limit", it is *this* limit, and deleting an explicit `wall = "1m"`
/// therefore raises the ceiling to 30 minutes rather than narrowing anything.
pub const RUN_DEFAULT_MEM: u64 = 4 * 1024 * 1024 * 1024;
pub const RUN_DEFAULT_PROCS: u64 = 256;
pub const RUN_DEFAULT_WALL_SECS: u64 = 30 * 60;
pub const INSTALL_DEFAULT_MEM: u64 = 8 * 1024 * 1024 * 1024;
pub const INSTALL_DEFAULT_PROCS: u64 = 512;
pub const INSTALL_DEFAULT_WALL_SECS: u64 = 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Provision,
    Install,
    Run,
    Shell,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Provision => "provision",
            Phase::Install => "install",
            Phase::Run => "run",
            Phase::Shell => "shell",
        }
    }

    /// `run` and `shell` share one policy; they differ only in what is
    /// executed inside it.
    pub fn policy_name(self) -> &'static str {
        match self {
            Phase::Shell => "run",
            other => other.as_str(),
        }
    }

    pub fn is_run_like(self) -> bool {
        matches!(self, Phase::Run | Phase::Shell)
    }
}

/// Something the user must be told about the policy that is about to be
/// enforced. senv never widens a boundary silently; when it does widen one, the
/// reason appears here, is printed, and is written to the receipt.
#[derive(Debug, Clone)]
pub struct Note {
    pub level: NoteLevel,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteLevel {
    Info,
    Warn,
}

impl Note {
    fn info(text: impl Into<String>) -> Note {
        Note {
            level: NoteLevel::Info,
            text: text.into(),
        }
    }
    fn warn(text: impl Into<String>) -> Note {
        Note {
            level: NoteLevel::Warn,
            text: text.into(),
        }
    }
}

/// A compiled, host-validated policy plus everything needed to execute under
/// it.
#[derive(Debug)]
pub struct Plan {
    pub phase: Phase,
    pub policy: ResolvedPolicy,
    /// True when the working directory is granted read-only.
    pub work_readonly: bool,
    /// The working directory, which h5i grants implicitly.
    pub work: PathBuf,
    /// Environment injected after h5i's `env.pass` allowlist. senv builds the
    /// child's environment here rather than relying on inheritance, so what a
    /// command sees is a decision and not an accident.
    pub env: Vec<(String, String)>,
    pub digest: String,
    pub notes: Vec<Note>,
}

impl Plan {
    pub fn tier(&self) -> IsolationClaim {
        self.policy.claim
    }
}

/// What a per-phase builder produces: the profile to enforce, the working
/// directory, and the environment the child is given.
type PhaseParts = (Profile, PathBuf, Vec<(String, String)>);

/// Inputs that vary per invocation rather than per project.
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// The install phase writes into the project directory (`--in-place`, or a
    /// project whose metadata cannot be resolved from a staging copy).
    pub project_writable: bool,
    /// Run the install against this directory instead of the project root.
    pub work_override: Option<PathBuf>,
    /// Extra egress hosts for this invocation only (`senv run --allow-net`).
    pub extra_hosts: Vec<String>,
    /// Path to the `uv` binary, which must be granted read access.
    pub uv: Option<PathBuf>,
}

/// Does the install phase write to its working directory?
///
/// A staging directory is senv's own scratch, so it is writable by
/// construction; the project is writable only when asked for.
fn project_writable(project: &Project, opts: &PlanOptions) -> bool {
    opts.work_override.is_some() || opts.project_writable || project.config.install.project_writable
}

/// Compile the policy for `phase`.
pub fn plan(project: &Project, phase: Phase, opts: &PlanOptions) -> Result<Plan> {
    let cfg = &project.config;
    let mut notes = Vec::new();

    let (mut profile, work, env) = match phase {
        Phase::Provision => provision_profile(project, opts)?,
        Phase::Install => install_profile(project, opts, &mut notes)?,
        Phase::Run | Phase::Shell => run_profile(project, phase, opts, &mut notes)?,
    };

    profile
        .fs_deny
        .extend(EXTRA_DENY.iter().map(|s| s.to_string()));
    profile.fs_deny.sort();
    profile.fs_deny.dedup();
    dedup_grants(&mut profile);

    // Pick the tier only once the profile knows what it needs to enforce.
    let needs_allowlist = !profile.net_egress.is_empty();
    let tier = select_tier(cfg, phase, needs_allowlist, &mut notes)?;
    profile.isolation = tier;
    if tier.image_backed() {
        profile.image = cfg.env.image.clone();
    }

    // h5i's own lints run before anything is spawned; a policy senv builds
    // wrongly must fail here, not halfway through an install.
    sandbox::validate_profile(&profile)?;
    let caps = sandbox::probe_host_for(tier);
    let policy =
        sandbox::resolve(&profile, &caps).map_err(|e| explain_tier_failure(e, tier, phase))?;
    sandbox::verify_exec(&policy)?;

    let digest = policy.digest()?;

    // h5i grants the working directory read-write *implicitly* — `$WORK` is in
    // the builtin `fs_write`, and `build_confined_command` unions it over any
    // read grant naming the same path. So listing the project under `fs_read`
    // does not make it read-only, and for a while senv believed it did: `senv
    // sync` ran with the project writable while `senv status` reported
    // "read-only", and a build backend could edit your source during an
    // install. `work_readonly` is the switch that actually moves `$WORK` into
    // the read-only set.
    //
    // Runtime-only and serde-skipped upstream, so it does not perturb the
    // digest — it is an enforcement mode for this invocation, not policy.
    let work_readonly = phase == Phase::Install && !project_writable(project, opts);
    let mut policy = policy;
    policy.work_readonly = work_readonly;

    Ok(Plan {
        phase,
        policy,
        work_readonly,
        work,
        env,
        digest,
        notes,
    })
}

// ── per-phase profiles ──────────────────────────────────────────────────────

/// Provisioning: `uv python install`. The narrowest phase in senv — one
/// writable directory, one command, no project involvement, no third-party
/// code.
fn provision_profile(project: &Project, opts: &PlanOptions) -> Result<PhaseParts> {
    let work = project.tmp("provision");
    crate::error::fs::create_dir_all(&work)?;
    // uv opens its cache before it does anything else, including
    // `python install`. Left unset it defaults to `~/.cache/uv`, which this
    // phase does not grant and must not — so provisioning died on
    // "failed to create directory `~/.cache/uv`" before a single byte was
    // downloaded. Every phase senv runs already redirects the cache; this one
    // was simply missed, and nothing noticed because provisioning only runs
    // when the host is short an interpreter.
    let cache = work.join("cache");
    crate::error::fs::create_dir_all(&cache)?;
    let python_dir = project.python_dir();

    let mut p = Profile::builtin("senv-provision", IsolationClaim::Supervised);
    grant_read(&mut p, opts.uv.as_deref());
    grant_write(&mut p, &[&python_dir, &work]);
    allowlist(
        &mut p,
        PYTHON_DOWNLOAD_HOSTS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    );
    p.mem_bytes = Some(2 * 1024 * 1024 * 1024);
    p.wall_secs = 30 * 60;
    p.tools = vec!["uv".to_string()];

    let env = vec![
        (
            "UV_PYTHON_INSTALL_DIR".to_string(),
            python_dir.display().to_string(),
        ),
        ("UV_PYTHON_DOWNLOADS".to_string(), "manual".to_string()),
        // uv also drops `python3.13` shims into `~/.local/bin` by default. senv
        // does not grant that directory and should not: it is on the user's
        // PATH, so a phase that can write there can put a binary in front of
        // every command they run. The boundary refused it correctly and uv
        // reported it as a failure, so a provisioning run that had worked
        // perfectly ended in two warnings and a "Failed to install executable".
        // Turning the shim off means there is nothing to refuse.
        ("UV_PYTHON_INSTALL_BIN".to_string(), "0".to_string()),
        // Inside the phase's own scratch rather than the project's wheel cache:
        // an interpreter download shares nothing with a wheel resolution, and
        // keeping it here leaves the narrowest phase senv has with exactly one
        // writable tree besides the interpreter directory it is filling.
        ("UV_CACHE_DIR".to_string(), cache.display().to_string()),
        ("TMPDIR".to_string(), work.display().to_string()),
        ("PATH".to_string(), system_path()),
    ];
    Ok((p, work, env))
}

/// The install boundary. See the module docs for the reasoning; the shape is:
/// registries-only egress, no secrets, writes confined to the environment and
/// the cache, and the project read-only unless explicitly opened.
fn install_profile(
    project: &Project,
    opts: &PlanOptions,
    notes: &mut Vec<Note>,
) -> Result<PhaseParts> {
    let cfg = &project.config;
    let work = opts
        .work_override
        .clone()
        .unwrap_or_else(|| project.root.clone());
    let staged = opts.work_override.is_some();
    let venv = project.venv();
    let cache = project.cache();
    let tmp = project.tmp("install");
    let python_dir = project.python_dir();

    let mut p = Profile::builtin("senv-install", IsolationClaim::Supervised);

    // A staging copy is already a scratch directory senv owns, so it is
    // writable as `$WORK`. The project itself is read-only unless the user
    // opened it, which is what stops a build backend from editing your source
    // while it installs.
    let project_writable = project_writable(project, opts);
    if !project_writable {
        p.fs_read.push(project.root.display().to_string());
    } else if !staged {
        notes.push(Note::warn(
            "the install phase can write to your project directory — a dependency's build \
             backend runs with that access. Set [install] project-writable = false (the \
             default) once the build no longer needs it."
                .to_string(),
        ));
    }

    grant_read(&mut p, opts.uv.as_deref());
    p.fs_read.push(python_dir.display().to_string());
    for extra in &cfg.install.read {
        p.fs_read.push(util::expand_tilde(extra));
    }
    grant_write(&mut p, &[&venv, &cache, &tmp]);

    match cfg.install.net {
        InstallNet::Registries => {
            let mut hosts: Vec<String> = PYPI_HOSTS.iter().map(|s| s.to_string()).collect();
            hosts.extend(cfg.install.extra_indexes.iter().cloned());
            allowlist(&mut p, hosts);
        }
        InstallNet::Host => {
            p.net_mode = NetMode::Host;
            p.net_egress.clear();
            notes.push(Note::warn(
                "[install] net = \"host\" — installs run with unrestricted network access. \
                 A build backend can reach any host it likes. This is recorded in every \
                 receipt for these installs."
                    .to_string(),
            ));
        }
    }

    // Installs are heavier than runs: a native wheel build is a compiler.
    let r = &cfg.install.resources;
    p.mem_bytes = Some(parse_mem_or(&r.mem, INSTALL_DEFAULT_MEM)?);
    p.max_procs = Some(r.procs.unwrap_or(INSTALL_DEFAULT_PROCS));
    p.wall_secs = parse_wall_or(r, INSTALL_DEFAULT_WALL_SECS)?;
    if let Some(fsize) = &r.fsize {
        p.fsize_bytes = Some(sandbox::parse_mem(fsize)?);
    }
    if let Some(cpu) = &r.cpu {
        p.cpu_secs = Some(sandbox::parse_wall(cpu)?.as_secs());
    }
    p.tools = vec!["uv".to_string()];

    // Secrets are absent here by construction, and that is not an oversight —
    // see `Config::validate`, which refuses to let one be scoped to install.

    let mut env = vec![
        (
            "UV_PROJECT_ENVIRONMENT".to_string(),
            venv.display().to_string(),
        ),
        ("UV_CACHE_DIR".to_string(), cache.display().to_string()),
        (
            "UV_PYTHON_INSTALL_DIR".to_string(),
            python_dir.display().to_string(),
        ),
        // An interpreter download is a different phase with a different egress
        // policy. If uv wants one here, it must fail and say so rather than
        // reach a host this policy did not anticipate.
        ("UV_PYTHON_DOWNLOADS".to_string(), "never".to_string()),
        ("TMPDIR".to_string(), tmp.display().to_string()),
        ("PATH".to_string(), system_path()),
        // uv writes the environment senv points it at; a stale VIRTUAL_ENV
        // inherited from the user's shell would make it warn on every command.
        ("VIRTUAL_ENV".to_string(), venv.display().to_string()),
        // Compile bytecode here, inside the boundary, so the environment ships
        // with a `__pycache__` that the run phase can read and nothing can
        // rewrite. This is what makes dropping the run phase's writable cache
        // free rather than a startup-time regression.
        ("UV_COMPILE_BYTECODE".to_string(), "1".to_string()),
    ];
    if let Some(py) = &cfg.env.python {
        env.push(("UV_PYTHON".to_string(), py.clone()));
    }
    Ok((p, work, env))
}

/// The run boundary: your code, your project, no network, and an environment it
/// cannot modify.
fn run_profile(
    project: &Project,
    phase: Phase,
    opts: &PlanOptions,
    notes: &mut Vec<Note>,
) -> Result<PhaseParts> {
    let cfg = &project.config;
    let venv = project.venv();
    let scratch = project.scratch();
    let tmp = project.tmp("run");
    let python_dir = project.python_dir();

    let mut p = Profile::builtin("senv-run", IsolationClaim::Process);

    // The environment is granted read-only and lives outside the project, so
    // the read-only grant is the whole story: there is no writable parent to
    // reach it through. This is why senv keeps the venv out of tree.
    p.fs_read.push(venv.display().to_string());
    p.fs_read.push(python_dir.display().to_string());
    for extra in &cfg.run.fs.read {
        p.fs_read.push(util::expand_tilde(extra));
    }
    grant_write(&mut p, &[&scratch, &tmp]);
    for extra in &cfg.run.fs.write {
        p.fs_write.push(util::expand_tilde(extra));
    }

    let mut hosts: Vec<String> = cfg.run.net.hosts().to_vec();
    hosts.extend(opts.extra_hosts.iter().cloned());
    if cfg.run.net.is_host() {
        p.net_mode = NetMode::Host;
        p.net_egress.clear();
        notes.push(Note::warn(
            "[run] net = \"host\" — your code has unrestricted network access.".to_string(),
        ));
    } else if hosts.is_empty() || (cfg.run.net.is_deny() && opts.extra_hosts.is_empty()) {
        p.net_mode = NetMode::Deny;
        p.net_egress.clear();
    } else {
        allowlist(&mut p, hosts);
        if !opts.extra_hosts.is_empty() {
            notes.push(Note::info(format!(
                "--allow-net widened this run to {} (not saved; use `senv allow` to keep it)",
                opts.extra_hosts.join(", ")
            )));
        }
    }

    let r = &cfg.run.resources;
    p.mem_bytes = Some(parse_mem_or(&r.mem, RUN_DEFAULT_MEM)?);
    p.max_procs = Some(r.procs.unwrap_or(RUN_DEFAULT_PROCS));
    p.wall_secs = parse_wall_or(r, RUN_DEFAULT_WALL_SECS)?;
    if cfg.run.resources.wall.is_some() && !r.wall_is_unbounded() {
        // Better to say this than to let `status` imply a deadline that never
        // fires: h5i applies the wall clock in the parent that waits for the
        // child, and the interactive path used by `run`/`shell` hands over the
        // terminal and waits without one. The kernel limits below (mem, procs,
        // cpu, fsize) are rlimits and do apply.
        notes.push(Note::info(
            "[run.resources] wall is not enforced for `senv run` or `senv shell` — the \
             interactive path has no deadline. It is enforced for installs. For a kernel-\
             enforced ceiling on a runaway command, set [run.resources] cpu."
                .to_string(),
        ));
    }
    if let Some(fsize) = &r.fsize {
        p.fsize_bytes = Some(sandbox::parse_mem(fsize)?);
    }
    if let Some(cpu) = &r.cpu {
        p.cpu_secs = Some(sandbox::parse_wall(cpu)?.as_secs());
    }
    for key in &cfg.run.env.pass {
        p.env_pass.push(key.clone());
    }
    p.env_pass.sort();
    p.env_pass.dedup();

    p.secret_grants = secret_grants(cfg, phase);
    p.secrets = p.secret_grants.iter().map(|g| g.name.clone()).collect();
    // Never inferred from the presence of a `command:` source. h5i makes this
    // opt-in and puts it in the digest precisely so enabling host-side
    // execution is a deliberate, visible act; deriving it from the secret's own
    // source undid that, and turned a config file the sandbox can write into a
    // host-escape primitive. `Config::validate` refuses a `command:` source
    // unless this is set, and `trust` treats setting it as a widening.
    p.allow_command_extractors = cfg.env.allow_command_secrets;
    if p.allow_command_extractors {
        notes.push(Note::warn(
            "a secret uses a command: source, which runs host-side code outside the sandbox \
             to mint the credential."
                .to_string(),
        ));
    }

    let bin = venv.join("bin");
    let env = vec![
        ("VIRTUAL_ENV".to_string(), venv.display().to_string()),
        (
            "PATH".to_string(),
            format!("{}:{}", bin.display(), system_path()),
        ),
        // Deliberately NO writable bytecode cache.
        //
        // senv used to point `PYTHONPYCACHEPREFIX` at a writable scratch
        // directory so byte compilation kept working against a read-only
        // environment. That handed the run phase a writable and
        // *authoritative* copy of every module's code: CPython trusts a `.pyc`
        // whose header matches the source's mtime and size, and a package can
        // read both. One execution could therefore plant bytecode that runs in
        // place of a read-only module on every later run — persistence through
        // exactly the door the read-only environment exists to close. Verified
        // before the fix: `import idna` afterwards ran the attacker's module
        // body instead of idna's.
        //
        // Without the prefix, CPython looks for `__pycache__` inside the
        // environment, which is read-only at run time and was populated by the
        // install phase (`UV_COMPILE_BYTECODE`). Writes there fail and CPython
        // ignores that, as it always has. Caches for the project's own modules
        // still land beside their sources, exactly as plain Python does, and
        // cost nothing here: that source is writable either way.
        ("TMPDIR".to_string(), tmp.display().to_string()),
        // An inherited PYTHONHOME would point the interpreter somewhere the
        // policy never granted, producing an unreadable startup failure.
        ("PYTHONHOME".to_string(), String::new()),
    ];
    Ok((p, project.root.clone(), env))
}

// ── tier selection ──────────────────────────────────────────────────────────

/// Choose the isolation tier, fail-closed.
///
/// The constraint that shapes this: on Linux the `process` tier's network is
/// all-or-nothing, so **any** domain allowlist needs `supervised` (private
/// netns + nftables pinned to resolved addresses) or an image-backed tier.
/// senv never downgrades silently — when a host cannot enforce what the policy
/// asks for, the command is refused and told what would fix it.
fn select_tier(
    cfg: &Config,
    phase: Phase,
    needs_allowlist: bool,
    notes: &mut Vec<Note>,
) -> Result<IsolationClaim> {
    if let Some(explicit) = &cfg.env.isolation {
        let explicit = explicit.trim();
        if !explicit.eq_ignore_ascii_case("auto") {
            let claim = IsolationClaim::parse(explicit)?;
            if needs_allowlist && claim <= IsolationClaim::Process {
                return Err(SenvError::refused(
                    format!("[env] isolation = \"{explicit}\" cannot enforce a network allowlist"),
                    "the process tier's network is all-or-nothing: it can deny everything or \
                     allow everything, but it cannot restrict egress to named hosts"
                        .to_string(),
                    "use isolation = \"supervised\" (or \"container\" with an image), or drop \
                     the allowlist"
                        .to_string(),
                ));
            }
            return Ok(claim);
        }
    }

    if !needs_allowlist {
        // Deny-all needs nothing exotic: an empty network namespace is enough,
        // and it works on every Linux kernel with Landlock.
        return Ok(IsolationClaim::Process);
    }

    if tier_available(IsolationClaim::Supervised) {
        return Ok(IsolationClaim::Supervised);
    }
    if cfg.env.image.is_some() && tier_available(IsolationClaim::Container) {
        notes.push(Note::info(
            "using the container tier: this host cannot run the supervised tier".to_string(),
        ));
        return Ok(IsolationClaim::Container);
    }

    Err(no_allowlist_tier_error(phase, cfg))
}

fn tier_available(claim: IsolationClaim) -> bool {
    let caps = sandbox::probe_host_for(claim);
    let probe = Profile::builtin("senv-probe", claim);
    sandbox::resolve(&probe, &caps).is_ok()
}

/// The refusal a host with no allowlist-capable tier gets. It is long on
/// purpose: this is the one message standing between a user and giving up on
/// the tool, so it says what is missing and every way forward.
fn no_allowlist_tier_error(phase: Phase, cfg: &Config) -> SenvError {
    let missing = supervisor_missing().join("\n     - ");
    let missing = if missing.is_empty() {
        String::new()
    } else {
        format!("\n   the supervised tier needs:\n     - {missing}")
    };
    let remedy = supervisor_remedy();
    let fix = if phase == Phase::Install && cfg.install.net == InstallNet::Registries {
        format!(
            "{remedy} Or set [install] net = \"host\" in senv.toml to install with unrestricted \
             network access — a warned, recorded downgrade. Run `senv doctor` for the full \
             picture."
        )
    } else {
        format!("{remedy} Or drop the network allowlist. Run `senv doctor` for the full picture.")
    };
    SenvError::refused(
        format!(
            "this host cannot enforce a network allowlist for the {} phase",
            phase.as_str()
        ),
        format!("senv will not pretend to restrict egress it cannot restrict.{missing}"),
        fix,
    )
}

/// What to actually *do* about a missing supervised tier, on this platform.
///
/// The advice used to be "sudo apt install slirp4netns nftables" unconditionally
/// — printed by `senv doctor` and by every allowlist refusal. On macOS that is
/// not merely unhelpful, it is misdirection: there is no slirp4netns to install
/// and no nftables to configure. The Darwin stack is Seatbelt plus a loopback
/// listener, both of which come with the OS, so an unusable tier there means
/// something is actively blocking `sandbox-exec` or loopback — a different
/// afternoon, and one no package manager fixes.
pub fn supervisor_remedy() -> &'static str {
    if cfg!(target_os = "macos") {
        "On macOS the supervised tier needs only macOS's own Seatbelt (`sandbox-exec`) and the \
         ability to bind loopback — nothing to install. An unusable tier here means something \
         on this machine is blocking one of them (an MDM profile, or a security agent); the \
         component list above names which."
    } else {
        "Install the missing pieces (on Debian/Ubuntu: sudo apt install slirp4netns nftables)."
    }
}

/// Serialize h5i's host probes across senv processes.
///
/// h5i's cgroup probe proves delegation by creating a fixed
/// `<base>/h5i.probe/probe-leaf`, writing to it, and removing it. Two senv
/// processes probing at the same moment therefore race: one removes the leaf
/// the other is about to write, that write fails, and the loser concludes the
/// host cannot delegate cgroups — so its install is refused with "this host
/// cannot enforce a network allowlist" on a host that plainly can. Measured at
/// roughly one in eight with eight concurrent `senv sync` runs.
///
/// It is an upstream race (the probe path should carry a pid), but the cost
/// lands on senv users running two commands at once, which is ordinary — a CI
/// matrix, or a dev server alongside a manual sync. So senv takes a
/// cross-process lock and forces the probe once while holding it. h5i caches
/// the result per process, so every later question inside this process reuses
/// the warmed answer and the lock is held only for the probe itself.
///
/// Failing to take the lock is not fatal: the worst case is the pre-existing
/// race, and refusing to run because a lock file could not be opened would be
/// a worse trade.
#[cfg(target_os = "linux")]
pub fn warm_host_probes() {
    use std::os::fd::AsRawFd;

    // The contended resource is the machine's cgroup hierarchy, so the lock has
    // to be machine-global too. An earlier version of this put the lock under
    // senv's state root, which is configurable per invocation — every process
    // locked a different file and the race was untouched.
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let lock_path = dir.join(format!("senv-probe-{uid}.lock"));

    // O_NOFOLLOW and 0600: the fallback directory may be world-writable, and
    // following someone else's symlink to open a file for writing is not a
    // thing to do even when nothing is written through it.
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(false);
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    let Ok(file) = opts.open(&lock_path) else {
        h5i_sandbox::cgroup::probe();
        return;
    };
    // Non-blocking with a short budget, never a bare `LOCK_EX`. The fallback
    // lock directory is world-writable, so any local user could take this lock
    // first and hold it — and every senv command would then hang at startup.
    // Failing to take it costs only the pre-existing probe race, which is worth
    // far less than a hang.
    // SAFETY: flock on a descriptor we own; released explicitly below and by
    // the close on drop regardless.
    let mut locked = false;
    for _ in 0..50 {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            locked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    h5i_sandbox::cgroup::probe();
    if locked {
        // SAFETY: same descriptor, still open.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(not(target_os = "linux"))]
pub fn warm_host_probes() {}

/// What the supervised tier reports as missing on this host, if anything.
pub fn supervisor_missing() -> Vec<String> {
    let probe = h5i_sandbox::supervisor::probe();
    if probe.usable {
        Vec::new()
    } else {
        probe.missing()
    }
}

/// Turn an engine refusal into one that names the senv-level cause.
fn explain_tier_failure(e: h5i_error::H5iError, tier: IsolationClaim, phase: Phase) -> SenvError {
    SenvError::refused(
        format!(
            "the {} phase cannot run at the {} tier on this host",
            phase.as_str(),
            tier.as_str()
        ),
        e.to_string(),
        "run `senv doctor` to see what this host can enforce",
    )
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Restrict egress to `hosts`, and to nothing else.
///
/// The `net_mode` here is **`Deny`, not `Host`**, and that one word is the
/// difference between an enforced allowlist and no allowlist at all on macOS.
///
/// h5i's image-backed tiers read a non-empty `net_egress` as the allowlist and
/// only consult `net_mode` when it is empty, and the Linux supervised tier is
/// the same: it builds the netns and the nftables ruleset from `net_egress`
/// alone. So `net_mode = Host` alongside a list looked harmless and read as
/// "the box has a network, restricted to these hosts".
///
/// Darwin's Seatbelt backend branches the other way round — it matches
/// `net_mode` first, and `Host` emits a bare `(allow network-outbound)`. The
/// allowlist proxy was still started and `HTTPS_PROXY` still pointed at it, but
/// nothing kept traffic on that route: a plain `socket.connect` reached any host
/// it liked. Verified before this change, with `senv run --allow-net
/// example.com` opening a socket to `api.stripe.com` — an install phase that
/// documented "PyPI and nothing else" was enforcing nothing on every Mac.
///
/// `Deny` means the same thing everywhere: no route out except the one this
/// policy names. On Linux the netns and nftables rules are unchanged (both are
/// keyed off `net_egress`); on macOS Seatbelt now permits exactly the proxy's
/// loopback port and denies name resolution, so a client that ignores the proxy
/// variables gets nothing rather than everything.
fn allowlist(p: &mut Profile, hosts: Vec<String>) {
    p.net_mode = NetMode::Deny;
    p.net_egress = hosts;
}

/// Does this profile restrict egress to named hosts?
///
/// Read the host list, never `net_mode`: an allowlist is expressed as
/// `Deny` + a non-empty list (see [`allowlist`]), so a check that asked
/// `net_mode == Deny` first would report an allowlisted phase as "no network".
pub fn has_allowlist(p: &Profile) -> bool {
    !p.net_egress.is_empty()
}

/// Is this profile's network unrestricted?
pub fn is_unrestricted(p: &Profile) -> bool {
    p.net_mode == NetMode::Host && p.net_egress.is_empty()
}

fn grant_read(p: &mut Profile, uv: Option<&Path>) {
    if let Some(uv) = uv {
        p.fs_read.push(uv.display().to_string());
    }
}

fn grant_write(p: &mut Profile, paths: &[&PathBuf]) {
    for path in paths {
        p.fs_write.push(path.display().to_string());
    }
}

/// Landlock unions grants, so duplicates are harmless — but the resolved policy
/// is what a reviewer reads and what the digest is taken over, and a list that
/// repeats itself is neither reviewable nor stable.
fn dedup_grants(p: &mut Profile) {
    for list in [&mut p.fs_read, &mut p.fs_write] {
        let mut seen = std::collections::HashSet::new();
        list.retain(|item| seen.insert(item.clone()));
    }
}

fn parse_mem_or(value: &Option<String>, default: u64) -> Result<u64> {
    match value {
        Some(v) => Ok(sandbox::parse_mem(v)?),
        None => Ok(default),
    }
}

fn parse_wall_or(r: &crate::config::ResourceSection, default: u64) -> Result<u64> {
    if r.wall_is_unbounded() {
        return Ok(UNBOUNDED_WALL_SECS);
    }
    match &r.wall {
        Some(v) => Ok(sandbox::parse_wall(v)?.as_secs()),
        None => Ok(default),
    }
}

/// A deterministic `PATH` for confined commands.
///
/// The host's `PATH` is not reused: it routinely points at directories the
/// policy does not grant (and, on WSL, at the whole Windows filesystem), which
/// turns every `command not found` into a puzzle.
///
/// Every entry must be inside the read grants h5i's built-in profile issues, or
/// the entry is a lie — the child would find the binary and be refused the
/// exec. `/opt/homebrew` qualifies through the builtin `/opt` grant, and it has
/// to be here: on Apple Silicon that is where Homebrew puts everything, so
/// without it `senv run node` (and `make`, `git`, `ruff`, anything not shipped
/// by Apple) was refused on a machine that plainly had it. The Intel-Mac
/// location is `/usr/local`, which is already listed.
pub fn system_path() -> String {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/opt/homebrew/sbin"]);
    }
    dirs.extend([
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
    ]);
    dirs.join(":")
}

/// Secret grants that apply to `phase`. A grant with no `phases` list applies
/// to both run and shell; the install phase can never appear here.
fn secret_grants(cfg: &Config, phase: Phase) -> Vec<SecretGrant> {
    if !phase.is_run_like() {
        return Vec::new();
    }
    let want = if phase == Phase::Shell {
        "shell"
    } else {
        "run"
    };
    cfg.secrets
        .iter()
        .filter(|(_, s)| s.phases.is_empty() || s.phases.iter().any(|p| p == want))
        .map(|(name, s)| SecretGrant {
            name: name.clone(),
            source: s.source.clone(),
            inject: s.inject.clone(),
            ttl: s.ttl.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CacheScope;

    /// The temp directory plus the redirect that points senv's roots at it.
    ///
    /// Both have to outlive the test, and the guard is the reason: it holds the
    /// lock that stops a neighbouring test from reassigning `SENV_STATE_DIR`
    /// mid-fixture. Returning only the `TempDir` dropped the guard at the end of
    /// `fixture`, which is the same as not taking it.
    struct Fixture {
        _tmp: tempfile::TempDir,
        _roots: crate::project::testing::RootsGuard,
    }

    /// A project in a temp dir, with senv's state redirected there too.
    fn fixture(config_text: &str) -> (Fixture, Project) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='x'\nversion='0'\n",
        )
        .unwrap();
        if !config_text.is_empty() {
            std::fs::write(root.join("senv.toml"), config_text).unwrap();
        }
        let roots = crate::project::testing::redirect_roots(tmp.path());
        let project = Project::at(&root).expect("project");
        project.ensure_dirs().expect("dirs");
        (
            Fixture {
                _tmp: tmp,
                _roots: roots,
            },
            project,
        )
    }

    fn profile_for(project: &Project, phase: Phase) -> Profile {
        let opts = PlanOptions {
            uv: Some(PathBuf::from("/usr/bin/true")),
            ..Default::default()
        };
        let (mut p, _, _) = match phase {
            Phase::Provision => provision_profile(project, &opts).unwrap(),
            Phase::Install => install_profile(project, &opts, &mut Vec::new()).unwrap(),
            _ => run_profile(project, phase, &opts, &mut Vec::new()).unwrap(),
        };
        p.fs_deny.extend(EXTRA_DENY.iter().map(|s| s.to_string()));
        dedup_grants(&mut p);
        p
    }

    #[test]
    fn the_run_phase_denies_the_network_by_default() {
        let (_t, project) = fixture("");
        let p = profile_for(&project, Phase::Run);
        assert_eq!(p.net_mode, NetMode::Deny);
        assert!(p.net_egress.is_empty());
    }

    #[test]
    fn the_environment_is_readable_and_never_writable_at_run_time() {
        // The property this protects: code running under `senv run` cannot
        // patch an installed package to persist into the next run.
        let (_t, project) = fixture("");
        let venv = project.venv().display().to_string();
        let p = profile_for(&project, Phase::Run);
        assert!(
            p.fs_read.contains(&venv),
            "the venv must be readable: {:?}",
            p.fs_read
        );
        assert!(
            !p.fs_write.iter().any(|w| w == &venv),
            "the venv must never be writable at run time: {:?}",
            p.fs_write
        );
        // And it is outside the project, so the implicit `$WORK` grant cannot
        // reach it through a writable parent.
        assert!(
            !project.venv().starts_with(&project.root),
            "the venv must live outside the project tree"
        );
    }

    #[test]
    fn the_install_phase_can_write_the_environment_but_not_the_project() {
        // Asserted on the *resolved* policy, not on the profile's strings.
        // The earlier version of this test checked that `fs_write` did not
        // contain the project path and concluded the project was read-only —
        // but h5i grants the working directory read-write implicitly through
        // `$WORK`, which is a different entry entirely. The test passed while
        // `senv sync` ran with the project writable.
        let (_t, project) = fixture("");
        let opts = PlanOptions {
            uv: Some(PathBuf::from("/usr/bin/true")),
            ..Default::default()
        };
        let plan = match plan(&project, Phase::Install, &opts) {
            Ok(plan) => plan,
            // A host that cannot enforce the install phase cannot answer the
            // question this test asks.
            Err(_) => return,
        };
        assert_eq!(
            plan.work, project.root,
            "the project is the working directory"
        );
        assert!(
            plan.policy.work_readonly,
            "the working directory IS the project, so it must be granted read-only"
        );
        assert!(plan.work_readonly);

        let venv = project.venv().display().to_string();
        assert!(plan.policy.profile.fs_write.iter().any(|w| w == &venv));
        assert!(
            plan.policy
                .profile
                .fs_read
                .iter()
                .any(|r| r == &project.root.display().to_string()),
            "and readable, or uv could not see the manifest"
        );
    }

    #[test]
    fn asking_for_a_writable_project_turns_the_read_only_grant_off() {
        let (_t, project) = fixture("[install]\nproject-writable = true\n");
        let opts = PlanOptions {
            uv: Some(PathBuf::from("/usr/bin/true")),
            ..Default::default()
        };
        let Ok(plan) = plan(&project, Phase::Install, &opts) else {
            return;
        };
        assert!(
            !plan.policy.work_readonly,
            "the user asked for write access"
        );
    }

    #[test]
    fn a_staged_install_keeps_its_staging_directory_writable() {
        // The stage is senv's own scratch and uv must write the manifests it
        // resolves there, so `work_readonly` must not leak into that case.
        let (_t, project) = fixture("");
        let opts = PlanOptions {
            uv: Some(PathBuf::from("/usr/bin/true")),
            work_override: Some(project.stage()),
            ..Default::default()
        };
        crate::error::fs::create_dir_all(&project.stage()).unwrap();
        let Ok(plan) = plan(&project, Phase::Install, &opts) else {
            return;
        };
        assert!(!plan.policy.work_readonly, "uv writes the staged manifests");
        assert_eq!(plan.work, project.stage());
    }

    #[test]
    fn the_run_phase_keeps_the_project_writable() {
        let (_t, project) = fixture("");
        let Ok(plan) = plan(&project, Phase::Run, &PlanOptions::default()) else {
            return;
        };
        assert!(!plan.policy.work_readonly, "your code edits your files");
    }

    #[test]
    fn an_allowlist_never_leaves_an_unrestricted_route_beside_it() {
        // The bug: senv paired its host list with `net_mode = host`. Linux and
        // the image-backed tiers read the list and ignored the mode, so it
        // looked correct everywhere it was tested — but Seatbelt reads the mode
        // first, and `host` there is a bare `(allow network-outbound)`. Every
        // Mac ran installs and allowlisted runs with the whole internet open
        // while `senv status` printed the allowlist.
        //
        // Asserted on the mode itself rather than on behaviour, because the
        // behaviour is a property of a backend senv does not own.
        let (_t, project) = fixture("[run]\nnet = [\"api.example.com\"]\n");
        for phase in [Phase::Provision, Phase::Install, Phase::Run] {
            let p = profile_for(&project, phase);
            assert!(!p.net_egress.is_empty(), "{phase:?} should have a list");
            assert_eq!(
                p.net_mode,
                NetMode::Deny,
                "{phase:?} pairs an allowlist with an unrestricted route: {:?}",
                p.net_egress
            );
            assert!(has_allowlist(&p), "{phase:?}");
            assert!(!is_unrestricted(&p), "{phase:?}");
        }
    }

    #[test]
    fn asking_for_an_unrestricted_network_is_still_distinguishable_from_a_deny() {
        // The three states must stay tellable apart, or `status`, the receipt
        // and `report` all mislabel what ran. `deny` and `host` are both an
        // empty host list; only the mode separates them.
        let (_t, denied) = fixture("");
        let p = profile_for(&denied, Phase::Run);
        assert!(!has_allowlist(&p) && !is_unrestricted(&p), "denied");

        let (_t2, open) = fixture("[run]\nnet = \"host\"\n");
        let p = profile_for(&open, Phase::Run);
        assert!(is_unrestricted(&p), "host means unrestricted");
        assert!(!has_allowlist(&p));

        let (_t3, listed) = fixture("[run]\nnet = [\"api.example.com\"]\n");
        let p = profile_for(&listed, Phase::Run);
        assert!(has_allowlist(&p) && !is_unrestricted(&p), "allowlist");
    }

    #[test]
    fn provisioning_writes_its_cache_somewhere_it_is_allowed_to() {
        // uv opens its cache before it does anything else, so an unset
        // `UV_CACHE_DIR` meant `uv python install` died on `~/.cache/uv` —
        // correctly refused by this phase's own grants — before downloading a
        // byte. Every path uv is pointed at must be inside the write set.
        let (_t, project) = fixture("");
        let opts = PlanOptions {
            uv: Some(PathBuf::from("/usr/bin/true")),
            ..Default::default()
        };
        let (p, work, env) = provision_profile(&project, &opts).unwrap();
        let cache = env
            .iter()
            .find(|(k, _)| k == "UV_CACHE_DIR")
            .map(|(_, v)| PathBuf::from(v))
            .expect("provisioning must redirect uv's cache");
        assert!(
            cache.starts_with(&work) || p.fs_write.iter().any(|w| cache.starts_with(w)),
            "the cache at {} is outside every write grant: {:?}",
            cache.display(),
            p.fs_write
        );
        assert!(
            cache.is_dir(),
            "and it must exist before the policy is built"
        );
    }

    #[test]
    fn the_macos_bytecode_cache_can_never_be_granted() {
        // Apple's system Python caches every module's bytecode there rather
        // than in `__pycache__`, so a writable grant would hand a package an
        // authoritative, writable copy of a read-only environment's code — the
        // persistence hole that dropping PYTHONPYCACHEPREFIX closed.
        let (_t, project) = fixture("");
        let p = profile_for(&project, Phase::Run);
        assert!(
            p.fs_deny
                .iter()
                .any(|d| d == "~/Library/Caches/com.apple.python"),
            "{:?}",
            p.fs_deny
        );

        let (_t2, wide) = fixture("[run.fs]\nwrite = [\"~/Library/Caches\"]\n");
        let p = profile_for(&wide, Phase::Run);
        assert!(
            sandbox::validate_profile(&p).is_err(),
            "granting the parent of the bytecode cache must be refused"
        );
    }

    #[test]
    fn the_install_phase_reaches_package_registries_and_nothing_else() {
        let (_t, project) = fixture("");
        let p = profile_for(&project, Phase::Install);
        assert!(p.net_egress.contains(&"pypi.org".to_string()));
        assert!(p.net_egress.contains(&"files.pythonhosted.org".to_string()));
        assert_eq!(p.net_egress.len(), 2, "no other host: {:?}", p.net_egress);

        let (_t2, with_index) = fixture("[install]\nextra-indexes = [\"download.pytorch.org\"]\n");
        let p = profile_for(&with_index, Phase::Install);
        assert!(p.net_egress.contains(&"download.pytorch.org".to_string()));
    }

    #[test]
    fn no_secret_can_reach_the_install_phase() {
        // Belt and braces: `Config::validate` refuses to declare one, and the
        // install profile would drop it even if it got through.
        let (_t, project) = fixture("[secrets.TOKEN]\nsource = \"env:TOKEN\"\n");
        let install = profile_for(&project, Phase::Install);
        assert!(
            install.secret_grants.is_empty(),
            "{:?}",
            install.secret_grants
        );
        let run = profile_for(&project, Phase::Run);
        assert_eq!(run.secret_grants.len(), 1, "but the run phase gets it");
        assert_eq!(run.secret_grants[0].name, "TOKEN");
    }

    #[test]
    fn a_secret_can_be_scoped_to_the_interactive_shell_only() {
        let (_t, project) = fixture("[secrets.TOKEN]\nphases = [\"shell\"]\n");
        assert!(profile_for(&project, Phase::Run).secret_grants.is_empty());
        assert_eq!(profile_for(&project, Phase::Shell).secret_grants.len(), 1);
    }

    #[test]
    fn credentials_are_denied_and_the_lint_would_catch_a_grant_that_exposed_them() {
        let (_t, project) = fixture("");
        let p = profile_for(&project, Phase::Run);
        for expected in ["~/.ssh", "~/.netrc", "~/.pypirc", "~/.git-credentials"] {
            assert!(
                p.fs_deny.iter().any(|d| d == expected),
                "missing deny: {expected}"
            );
        }
        // A user who grants their whole home directory must be refused, not
        // quietly given a policy that reads as though ~/.ssh were protected.
        let (_t2, wide) = fixture("[run.fs]\nread = [\"~\"]\n");
        let p = profile_for(&wide, Phase::Run);
        assert!(
            sandbox::validate_profile(&p).is_err(),
            "granting $HOME must be refused: Landlock cannot subtract ~/.ssh from it"
        );
    }

    #[test]
    fn an_allowlist_is_refused_at_a_tier_that_cannot_enforce_it() {
        let (_t, project) =
            fixture("[env]\nisolation = \"process\"\n[run]\nnet = [\"api.example.com\"]\n");
        let err = select_tier(&project.config, Phase::Run, true, &mut Vec::new())
            .expect_err("must refuse");
        assert!(err.to_string().contains("all-or-nothing"), "{err}");
        assert!(err.fix().unwrap().contains("supervised"), "{err:?}");
    }

    #[test]
    fn denying_the_network_never_needs_an_exotic_tier() {
        // The common case — run untrusted code with no network — must work on
        // any Linux with Landlock, with no slirp4netns or Podman in sight.
        let (_t, project) = fixture("");
        let tier = select_tier(&project.config, Phase::Run, false, &mut Vec::new()).unwrap();
        assert_eq!(tier, IsolationClaim::Process);
    }

    #[test]
    fn wall_none_is_finite() {
        let (_t, project) = fixture("[run.resources]\nwall = \"none\"\n");
        let mut notes = Vec::new();
        let opts = PlanOptions::default();
        let (p, _, _) = run_profile(&project, Phase::Run, &opts, &mut notes).unwrap();
        assert_eq!(p.wall_secs, UNBOUNDED_WALL_SECS);
        assert!(p.wall_secs > 0, "there is always a kill switch");
        // `none` is expressed as a finite year so h5i's "always a kill switch"
        // invariant holds; for run/shell there is no deadline anyway, which the
        // note below the assertion covers separately.
        assert!(p.wall_secs > 0, "there is always a kill switch");
    }

    #[test]
    fn opening_the_network_or_the_project_is_always_announced() {
        let (_t, project) = fixture("[run]\nnet = \"host\"\n");
        let mut notes = Vec::new();
        run_profile(&project, Phase::Run, &PlanOptions::default(), &mut notes).unwrap();
        assert!(
            notes.iter().any(|n| n.level == NoteLevel::Warn),
            "{notes:?}"
        );

        let (_t2, project) = fixture("[install]\nproject-writable = true\n");
        let mut notes = Vec::new();
        install_profile(&project, &PlanOptions::default(), &mut notes).unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.level == NoteLevel::Warn && n.text.contains("build")),
            "{notes:?}"
        );
    }

    #[test]
    fn the_run_environment_is_built_not_inherited() {
        let (_t, project) = fixture("");
        let (_, _, env) = run_profile(
            &project,
            Phase::Run,
            &PlanOptions::default(),
            &mut Vec::new(),
        )
        .unwrap();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(
            get("VIRTUAL_ENV"),
            Some(project.venv().display().to_string())
        );
        assert!(
            get("PATH")
                .unwrap()
                .starts_with(&project.venv().join("bin").display().to_string())
        );
        // /tmp is read-only under the default grants, so a temp dir the policy
        // actually grants is mandatory — without it every tempfile.mkdtemp()
        // lands in the project or fails.
        assert_eq!(
            get("TMPDIR"),
            Some(project.tmp("run").display().to_string())
        );
        // No writable bytecode cache: a `.pyc` there is an authoritative copy
        // of code that is supposed to be read-only, and CPython will prefer it
        // over the source whenever the header matches.
        assert!(
            get("PYTHONPYCACHEPREFIX").is_none(),
            "a writable bytecode cache overrides the read-only environment"
        );
    }

    #[test]
    fn the_cache_is_per_project_unless_the_user_opts_into_sharing() {
        let (_t, project) = fixture("");
        assert_eq!(project.config.install.cache, CacheScope::Project);
        assert!(project.cache().starts_with(&project.state_dir));

        let (_t2, shared) = fixture("[install]\ncache = \"shared\"\n");
        assert!(!shared.cache().starts_with(&shared.state_dir));
    }

    #[test]
    fn provisioning_touches_nothing_but_the_interpreter_directory() {
        let (_t, project) = fixture("");
        let p = profile_for(&project, Phase::Provision);
        let py = project.python_dir().display().to_string();
        assert!(p.fs_write.iter().any(|w| w == &py));
        assert!(
            !p.fs_write
                .iter()
                .any(|w| w == &project.root.display().to_string()),
            "provisioning has no business with the project: {:?}",
            p.fs_write
        );
        assert!(p.net_egress.iter().any(|h| h == "github.com"));
        assert!(
            !p.net_egress.iter().any(|h| h == "pypi.org"),
            "no package access here"
        );
    }

    #[test]
    fn grants_do_not_repeat_themselves_in_the_digested_policy() {
        let (_t, project) = fixture("[run.fs]\nread = [\"/usr\", \"/usr\"]\n");
        let p = profile_for(&project, Phase::Run);
        let usr = p.fs_read.iter().filter(|r| *r == "/usr").count();
        assert_eq!(usr, 1, "{:?}", p.fs_read);
    }
}
