//! Making the boundary legible: `status`, `report`, `allow`, `doctor`, `gc`.
//!
//! A security boundary nobody can inspect is a security boundary nobody
//! trusts. These commands answer, in order: what is enforced right now, what
//! has actually happened, how do I open the one thing I need, what can this
//! machine enforce at all, and what is senv keeping on my disk.

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::Ctx;
use crate::config::CONFIG_FILE;
use crate::error::{Result, SenvError, fs};
use crate::exec;
use crate::policy::{self, Phase, PlanOptions};
use crate::project::{self, Project};
use crate::receipt::{DenialKind, Receipts, Verdict};
use crate::util;

// ── status ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct StatusOutput {
    pub project: String,
    /// Stable handle for this project's state directory.
    pub key: String,
    pub config: Option<String>,
    pub state: String,
    pub environment: EnvironmentStatus,
    pub phases: Vec<PhaseStatus>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EnvironmentStatus {
    pub path: String,
    pub exists: bool,
    pub provenance: String,
    pub python: Option<String>,
    pub lock: String,
    pub last_sync: Option<String>,
    pub venv_link: String,
    pub size: String,
}

#[derive(Debug, Serialize)]
pub struct PhaseStatus {
    pub phase: String,
    /// `None` when this host cannot enforce the phase's policy at all.
    pub tier: Option<String>,
    pub network: String,
    pub writable: Vec<String>,
    pub readable: Vec<String>,
    pub resources: BTreeMap<String, String>,
    pub policy_digest: Option<String>,
    pub secrets: Vec<String>,
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

pub fn status(ctx: &Ctx) -> Result<i32> {
    let project = ctx.project_unchecked()?;
    ctx.warn_if_untrusted(&project);
    let mut state = project.load_state();
    // `state.json` is senv's own file, outside every grant — but the interpreter
    // string in it was copied out of the environment's `pyvenv.cfg`, which the
    // install phase grants a build backend read-write. It is sanitized on the
    // way in now; sanitizing on the way out as well is what protects a state
    // file that was already poisoned before this senv was installed, since
    // nothing rewrites it until the next successful sync.
    state.python = state.python.as_deref().map(util::sanitize);
    let uv_bin = crate::uv::find(&project).ok();

    let lock = match project.lock_is_current(&state) {
        Some(true) => "current".to_string(),
        Some(false) => "STALE — run `senv sync`".to_string(),
        None if project.lock_path().is_file() => "not yet synced".to_string(),
        None => "absent".to_string(),
    };

    let mut warnings = Vec::new();
    if let Some(w) = project.venv_link_status().warning() {
        warnings.push(w);
    }
    if state.provenance == project::Provenance::HostInstalled {
        warnings.push(
            "this environment was adopted from an existing .venv, so its packages never passed \
             through the install boundary. `senv sync` rebuilds it inside the sandbox."
                .to_string(),
        );
    }
    if state.provenance == project::Provenance::Sandboxed && !project.venv_has_bytecode() {
        warnings.push(format!(
            "this environment contains no compiled bytecode, so every import recompiles from \
             source on each run. senv compiles it during the install precisely so the run phase \
             does not need a writable cache — but this interpreter ({}) has sys.pycache_prefix \
             preset and cached it outside the environment instead, where the boundary cannot \
             use it. Apple's system Python does this. Build the environment on a managed \
             interpreter to get it back: `senv init --python 3.13`, or [env] python in \
             senv.toml.",
            state.python.as_deref().unwrap_or("unknown version")
        ));
    }

    let phases = [Phase::Install, Phase::Run]
        .iter()
        .map(|phase| phase_status(&project, *phase, uv_bin.as_deref()))
        .collect();

    let out = StatusOutput {
        project: project.root.display().to_string(),
        key: project.key.clone(),
        config: project
            .config_path
            .is_file()
            .then(|| project.config_path.display().to_string()),
        state: project.state_dir.display().to_string(),
        environment: EnvironmentStatus {
            path: project.venv().display().to_string(),
            exists: project.venv_exists(),
            provenance: state.provenance.as_str().to_string(),
            python: state.python.clone(),
            lock,
            last_sync: state.last_sync_ms.map(util::format_utc),
            venv_link: match project.venv_link_status() {
                project::VenvLink::Ours => "linked".to_string(),
                project::VenvLink::Dangling => "linked, but nothing built yet".to_string(),
                project::VenvLink::Absent => "absent".to_string(),
                project::VenvLink::Directory => "a real directory (not senv's)".to_string(),
                // The link target is whatever the run phase pointed `.venv` at,
                // so it is attacker-chosen text on senv's own status line.
                project::VenvLink::OtherLink(p) => {
                    format!(
                        "links elsewhere: {}",
                        util::sanitize(&p.display().to_string())
                    )
                }
                project::VenvLink::Foreign => "unreadable".to_string(),
            },
            size: util::human_bytes(util::dir_size(&project.venv())),
        },
        phases,
        warnings,
    };

    ctx.emit(&out, render_status)?;
    Ok(0)
}

fn phase_status(project: &Project, phase: Phase, uv_bin: Option<&std::path::Path>) -> PhaseStatus {
    let opts = PlanOptions {
        uv: uv_bin.map(|p| p.to_path_buf()),
        ..Default::default()
    };
    match policy::plan(project, phase, &opts) {
        Ok(plan) => {
            let p = &plan.policy.profile;
            let mut resources = BTreeMap::new();
            // Which of these the *host* actually applies is a separate question
            // from what the profile asks for, and h5i answers it per tier.
            // Darwin has no cgroups, does not enforce RLIMIT_AS against the
            // mmap'd heap every Python runtime uses, and scopes RLIMIT_NPROC to
            // the whole uid rather than to one command — so at the kernel tiers
            // it applies neither, and senv printing a bare "mem 4.0 GiB" there
            // stated a ceiling that does not exist.
            let limits = h5i_sandbox::sandbox::limit_support(plan.tier());
            if let Some(mem) = p.mem_bytes {
                resources.insert(
                    "mem".to_string(),
                    annotate(util::human_bytes(mem), limits.mem),
                );
            }
            if let Some(procs) = p.max_procs {
                resources.insert(
                    "procs".to_string(),
                    annotate(procs.to_string(), limits.procs),
                );
            }
            // The wall clock is applied by the parent that waits for the
            // child, and the interactive path hands the terminal over and
            // simply waits — so for `run` and `shell` it is not enforced.
            // Everything else here is a kernel rlimit and applies to every
            // phase. Printing "wall 30m" for a phase that has no deadline is
            // exactly the kind of untrue status line this tool cannot afford.
            resources.insert(
                "wall".to_string(),
                if phase.is_run_like() {
                    format!(
                        "{} (not enforced for run/shell — use cpu)",
                        format_duration(p.wall_secs)
                    )
                } else {
                    format_duration(p.wall_secs)
                },
            );
            PhaseStatus {
                phase: phase.as_str().to_string(),
                tier: Some(plan.tier().as_str().to_string()),
                network: describe_net(p),
                writable: writable_summary(project, p, plan.work_readonly),
                readable: readable_summary(project, p),
                resources,
                policy_digest: Some(plan.digest.clone()),
                secrets: p.secrets.clone(),
                notes: plan.notes.iter().map(|n| n.text.clone()).collect(),
                unavailable: None,
            }
        }
        Err(e) => PhaseStatus {
            phase: phase.as_str().to_string(),
            tier: None,
            network: "—".to_string(),
            writable: Vec::new(),
            readable: Vec::new(),
            resources: BTreeMap::new(),
            policy_digest: None,
            secrets: Vec::new(),
            notes: Vec::new(),
            // An engine refusal quotes the grant it refused, and that grant
            // came from senv.toml. Multiline because these are rendered as a
            // block, not spliced into one of senv's sentences.
            unavailable: Some(util::sanitize_multiline(&e.to_string())),
        },
    }
}

/// The network line for a phase.
///
/// The allowlist is checked before `net_mode`, because an allowlist *is*
/// expressed as `Deny` plus a host list — see [`policy::has_allowlist`]. Asking
/// `net_mode` first would print "denied" for the install phase, which reaches
/// PyPI on every sync.
fn describe_net(p: &h5i_sandbox::sandbox_policy::Profile) -> String {
    if policy::has_allowlist(p) {
        let hosts: Vec<String> = p.net_egress.iter().map(|h| util::sanitize(h)).collect();
        format!("{}{}", hosts.join(", "), egress_caveat())
    } else if policy::is_unrestricted(p) {
        "UNRESTRICTED".to_string()
    } else {
        "denied".to_string()
    }
}

/// How an allowlist is enforced, when that is not obvious.
///
/// On Linux it is nftables rules pinned to resolved addresses inside the box's
/// own network namespace, so the allowlist holds for any client. macOS has no
/// namespace: h5i leaves the box on the host's stack and Seatbelt permits one
/// destination, the loopback port of h5i's DNS-pinned allowlist proxy. The
/// allowlist itself is enforced by the kernel either way — what differs is that
/// a Mac box has no DNS and no direct socket, so a client that ignores
/// `HTTPS_PROXY` reaches nothing at all rather than reaching its host directly.
/// Saying so is the difference between a confusing failure and an expected one.
fn egress_caveat() -> &'static str {
    if cfg!(target_os = "macos") {
        " (reached through senv's allowlist proxy — a client that ignores \
         HTTPS_PROXY gets no network at all)"
    } else {
        ""
    }
}

/// Writable paths, with `$WORK` spelled out.
///
/// `$WORK` is granted implicitly by h5i and never appears in `fs_write`, so it
/// has to be added here from the enforcement mode rather than inferred from the
/// grant lists. Inferring it was how `status` came to report the project as
/// read-only during installs while it was in fact writable.
///
/// Every grant string here came out of `senv.toml`, which the run phase can
/// write — see [`sanitized_grants`].
fn writable_summary(
    project: &Project,
    p: &h5i_sandbox::sandbox_policy::Profile,
    work_readonly: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if !work_readonly {
        out.push(format!("{} (the project)", project.root.display()));
    }
    out.extend(sanitized_grants(p.fs_write.iter().filter(|w| {
        !matches!(w.as_str(), "$WORK" | "/dev/null" | "/dev/zero")
    })));
    out
}

/// Strip terminal control sequences from grant paths before they are displayed.
///
/// `[run.fs] read`, `[run.fs] write` and `[run] net` are copied verbatim out of
/// `senv.toml` into the compiled policy, and `senv.toml` lives in the project —
/// which the run phase grants read-write. So these strings are chosen by the
/// code the boundary contains, and `senv status` prints them.
///
/// That made `status` forgeable by the one thing it is supposed to expose. A
/// package writing `read = ["/data[2K\rnetwork    denied"]` had its own
/// text erase and repaint senv's lines; verified before this change, with the
/// raw `ESC[2K` and `\r` reaching the terminal. `status` is what a user runs
/// *after* senv warns that a policy was widened behind their back, so it is
/// precisely the output that must not be writable by the suspect.
///
/// The same reasoning `crate::trust` already applies to the widenings it
/// reports — that code sanitizes, and this did not.
fn sanitized_grants<'a>(items: impl Iterator<Item = &'a String>) -> Vec<String> {
    items.map(|s| util::sanitize(s)).collect()
}

fn readable_summary(project: &Project, p: &h5i_sandbox::sandbox_policy::Profile) -> Vec<String> {
    const BASELINE: [&str; 13] = [
        "/usr",
        "/lib",
        "/lib64",
        "/bin",
        "/sbin",
        "/etc",
        "/nix",
        "/opt",
        "/tmp",
        "/dev/null",
        "/dev/zero",
        "/dev/urandom",
        "/proc",
    ];
    let venv = project.venv().display().to_string();
    let root = project.root.display().to_string();
    // Compared before sanitizing, rendered after: the comparison must be
    // against the grant the policy actually carries, and only the rendering is
    // a place a control sequence could do harm. See `sanitized_grants`.
    p.fs_read
        .iter()
        .filter(|r| !BASELINE.contains(&r.as_str()))
        .map(|r| {
            let shown = util::sanitize(r);
            if *r == venv {
                format!("{shown} (the environment, read-only)")
            } else if *r == root {
                format!("{shown} (the project, read-only)")
            } else {
                shown
            }
        })
        .collect()
}

/// Mark a configured limit that this host does not actually apply.
///
/// The value is still shown: it is what the policy asks for, it is in the
/// digest, and it becomes real the moment the same project runs on a host that
/// can enforce it. What must not happen is showing it as though it were a
/// ceiling here.
fn annotate(value: String, enforced: bool) -> String {
    if enforced {
        value
    } else {
        format!("{value} (NOT enforced on this host)")
    }
}

fn format_duration(secs: u64) -> String {
    if secs >= 86_400 * 300 {
        return "unbounded (wall = \"none\")".to_string();
    }
    if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn render_status(o: &StatusOutput) {
    println!("project      {}", o.project);
    println!(
        "config       {}",
        o.config
            .clone()
            .unwrap_or_else(|| format!("(none — defaults; write {CONFIG_FILE} to change)"))
    );
    println!("state        {}", o.state);
    println!();
    let e = &o.environment;
    println!("environment  {}", e.path);
    println!(
        "  status     {}",
        if e.exists {
            "installed"
        } else {
            "not created yet"
        }
    );
    println!("  provenance {}", e.provenance);
    if let Some(py) = &e.python {
        println!("  python     {py}");
    }
    println!("  lockfile   {}", e.lock);
    if let Some(sync) = &e.last_sync {
        println!("  last sync  {sync}");
    }
    println!("  .venv      {}", e.venv_link);
    if e.exists {
        println!("  size       {}", e.size);
    }

    for phase in &o.phases {
        println!();
        match (&phase.tier, &phase.unavailable) {
            (_, Some(err)) => {
                println!("{} phase  UNAVAILABLE", phase.phase);
                println!("  {}", exec::wrap(err, 74, 2));
                continue;
            }
            (Some(tier), _) => println!("{} phase  ({tier} tier)", phase.phase),
            _ => {}
        }
        println!("  network    {}", phase.network);
        for (i, w) in phase.writable.iter().enumerate() {
            println!("  {} {w}", if i == 0 { "writable  " } else { "          " });
        }
        for (i, r) in phase.readable.iter().enumerate() {
            println!("  {} {r}", if i == 0 { "readable  " } else { "          " });
        }
        let limits: Vec<String> = phase
            .resources
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        if !limits.is_empty() {
            println!("  limits     {}", limits.join(", "));
        }
        if !phase.secrets.is_empty() {
            println!("  secrets    {}", phase.secrets.join(", "));
        }
        if let Some(d) = &phase.policy_digest {
            println!("  digest     {}", &d[..16.min(d.len())]);
        }
        for note in &phase.notes {
            println!("  note       {}", exec::wrap(note, 62, 13));
        }
    }

    for w in &o.warnings {
        eprintln!("\nwarning: {}", exec::wrap(w, 74, 9));
    }
}

// ── report ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ReportOutput {
    pub receipt: String,
    pub total_commands: usize,
    pub recent: Vec<RecentEntry>,
    pub blocked_hosts: Vec<String>,
    pub blocked_paths: Vec<String>,
    pub blocked_by_design: Vec<String>,
    /// Refusals whose destination the error never named, so there is nothing to
    /// suggest — reported as a count rather than dropped.
    pub blocked_unnamed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RecentEntry {
    pub ts: String,
    pub phase: String,
    pub tier: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub denials: usize,
}

pub fn report(ctx: &Ctx, args: &crate::cli::ReportArgs) -> Result<i32> {
    let project = ctx.project_unchecked()?;
    ctx.warn_if_untrusted(&project);
    let receipts = Receipts::new(project.receipt_path(), vec![project.venv()]);
    let records = receipts.read();

    let mut hosts: Vec<String> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    let mut by_design: Vec<String> = Vec::new();
    let mut unnamed = 0usize;
    for record in &records {
        for d in &record.denials {
            match (&d.verdict, d.kind) {
                (Verdict::Suggestable, DenialKind::Network) => push_once(&mut hosts, &d.target),
                (Verdict::Suggestable, DenialKind::Filesystem) => push_once(&mut paths, &d.target),
                (Verdict::Unnamed, _) => unnamed += 1,
                (Verdict::ByDesign(_), _) => push_once(&mut by_design, &d.target),
            }
        }
    }

    let recent = records
        .iter()
        .rev()
        .take(args.limit)
        .map(|r| RecentEntry {
            ts: r.ts.clone(),
            phase: r.phase.clone(),
            tier: r.tier.clone(),
            command: shorten_command(&r.argv),
            exit_code: r.exit_code,
            denials: r.denials.len(),
        })
        .collect();

    let suggestion = args.suggest.then(|| suggest_stanza(&hosts, &paths));

    let out = ReportOutput {
        receipt: project.receipt_path().display().to_string(),
        total_commands: records.len(),
        recent,
        blocked_hosts: hosts,
        blocked_paths: paths,
        blocked_by_design: by_design,
        blocked_unnamed: unnamed,
        suggestion,
    };
    ctx.emit(&out, render_report)?;
    Ok(0)
}

fn push_once(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|v| v == value) {
        list.push(value.to_string());
    }
}

/// One line per command in the report, however the command was written.
///
/// Absolute paths collapse to their basename (the full path to uv is noise),
/// and embedded newlines collapse to spaces — a `python -c` with a multi-line
/// script would otherwise turn a table into a wall of text.
fn shorten_command(argv: &[String]) -> String {
    const MAX: usize = 60;
    let joined = argv
        .iter()
        .map(|a| {
            a.rsplit('/')
                .next()
                .filter(|_| a.starts_with('/'))
                .unwrap_or(a)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" ");
    let joined = util::sanitize(&joined);
    if joined.chars().count() <= MAX {
        return joined;
    }
    let cut: String = joined.chars().take(MAX - 1).collect();
    format!("{cut}…")
}

/// Turn recorded denials into a `senv.toml` stanza.
///
/// Printed, never written. The whole point of the boundary is that widening it
/// is a decision someone makes and commits, so senv produces the text and stops
/// there.
fn suggest_stanza(hosts: &[String], paths: &[String]) -> String {
    if hosts.is_empty() && paths.is_empty() {
        return "# nothing was blocked that senv would suggest allowing\n".to_string();
    }
    let mut out = String::from(
        "# Reviewed suggestions from recorded denials. Nothing is applied until you\n\
         # paste it into senv.toml yourself.\n",
    );
    if !hosts.is_empty() {
        out.push_str("\n[run]\nnet = [\n");
        for h in hosts {
            out.push_str(&format!("  {},\n", toml_string(h)));
        }
        out.push_str("]\n");
    }
    if !paths.is_empty() {
        out.push_str("\n[run.fs]\nread = [\n");
        for p in paths {
            out.push_str(&format!("  {},\n", toml_string(p)));
        }
        out.push_str("]\n");
    }
    out
}

/// Quote a value as a TOML basic string.
///
/// These come from a program's output, and the result is text a user pastes
/// into their policy. Today the extractor cannot produce a `"` — it tokenizes
/// on quotes — so nothing can break out of the string. That is a property of
/// one function elsewhere in this file, which is a thin thing to rest on when
/// the consequence is a policy that grants more than it appears to.
fn toml_string(value: &str) -> String {
    let escaped: String = value
        .chars()
        .flat_map(|c| match c {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect(),
            c => vec![c],
        })
        .collect();
    format!("\"{escaped}\"")
}

fn render_report(o: &ReportOutput) {
    println!("receipt  {}", o.receipt);
    println!("commands {}", o.total_commands);
    if !o.recent.is_empty() {
        println!();
        for e in &o.recent {
            let status = match e.exit_code {
                Some(0) => "ok  ".to_string(),
                Some(c) => format!("exit{c:<2}"),
                None => "kill".to_string(),
            };
            let denials = if e.denials > 0 {
                format!("  {} blocked", e.denials)
            } else {
                String::new()
            };
            println!("{}  {status}  {:<9} {}{denials}", e.ts, e.phase, e.command);
        }
    }
    if !o.blocked_hosts.is_empty() {
        println!("\nblocked hosts:  {}", o.blocked_hosts.join(", "));
    }
    if !o.blocked_paths.is_empty() {
        println!("blocked paths:  {}", o.blocked_paths.join(", "));
    }
    if !o.blocked_by_design.is_empty() {
        println!(
            "blocked by design (not suggestable):  {}",
            o.blocked_by_design.join(", ")
        );
    }
    if o.blocked_unnamed > 0 {
        println!(
            "blocked connections whose host the error did not name:  {}",
            o.blocked_unnamed
        );
    }
    match &o.suggestion {
        Some(s) => println!("\n{s}"),
        None if !o.blocked_hosts.is_empty() || !o.blocked_paths.is_empty() => {
            println!("\nrun `senv report --suggest` for a policy stanza covering these");
        }
        None => {}
    }
}

// ── allow ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AllowOutput {
    pub config: String,
    pub added: Vec<String>,
    pub already_present: Vec<String>,
    pub net: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn allow(ctx: &Ctx, args: &crate::cli::AllowArgs) -> Result<i32> {
    // This command widens the policy on purpose and then re-baselines, so it
    // must start from a policy the user has already accepted. Otherwise
    // `senv allow example.com` would quietly adopt whatever else had been
    // added to the file since — laundering an attacker's edit through a
    // legitimate one.
    let project = ctx.project_unchecked()?;
    ctx.guard_trust(&project)?;
    for host in &args.hosts {
        crate::config::validate_host_pattern(host).map_err(|e| {
            SenvError::refused(
                "that is not a host senv can allow",
                e,
                "use a hostname like api.example.com",
            )
        })?;
    }

    let path = project.config_path.clone();
    // The same guarded read `Config::load` uses, not a bare one. This is the
    // second time senv opens this file in one command and it is about to write
    // it back, so it must not be the weaker of the two reads: `senv.toml` is in
    // the project, and between the load at startup and here it can have become
    // a symlink, a fifo, or a gigabyte.
    let text = if std::fs::symlink_metadata(&path).is_ok() {
        fs::read_to_string_no_follow(&path)?
    } else {
        String::new()
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| SenvError::config(&path, e.to_string()))?;

    let mut warnings = Vec::new();
    let run = doc
        .entry("run")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let run = run
        .as_table_mut()
        .ok_or_else(|| SenvError::config(&path, "[run] exists but is not a table"))?;

    // An existing `net = "host"` is already wider than any allowlist; turning
    // it into one here would silently *narrow* the policy, which is a change
    // the user did not ask for.
    if run.get("net").and_then(|n| n.as_str()) == Some("host") {
        warnings.push(
            "[run] net = \"host\" is already unrestricted, so nothing was added. Remove it \
             first if you want an allowlist."
                .to_string(),
        );
        let out = AllowOutput {
            config: path.display().to_string(),
            added: Vec::new(),
            already_present: args.hosts.clone(),
            net: vec!["host".to_string()],
            warnings,
        };
        ctx.emit(&out, render_allow)?;
        return Ok(0);
    }

    let mut existing: Vec<String> = run
        .get("net")
        .and_then(|n| n.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut added = Vec::new();
    let mut already = Vec::new();
    for host in &args.hosts {
        if existing.iter().any(|h| h == host) {
            already.push(host.clone());
        } else {
            existing.push(host.clone());
            added.push(host.clone());
        }
    }

    let mut array = toml_edit::Array::new();
    for host in &existing {
        array.push(host.as_str());
    }
    array.set_trailing_comma(false);
    run["net"] = toml_edit::value(array);

    let rendered = doc.to_string();
    project::write_atomic(&path, rendered.as_bytes())?;

    // Baseline the document senv just produced, **not** a re-read of the file.
    //
    // The user asked for exactly this widening, so the result has to be
    // recorded as trusted or the next command would refuse over senv's own
    // edit. Re-reading `senv.toml` to do that was a working escalation:
    // between `guard_trust` at the top of this function and the record here
    // lie a config read, a parse and two fsync'd writes, and anything that
    // edited the file inside that window was silently blessed. Code under a
    // concurrent `senv run` — a dev server, a watcher, a test run, all of which
    // this design calls ordinary — can flip the file between empty and hostile
    // in a loop and win on the first attempt, turning the one command every
    // denial message recommends into "trust my policy". The laundered config
    // carried `allow-command-secrets` and a `command:` secret, which is
    // unconfined host execution, after which `senv trust` reported nothing to
    // accept, forever.
    let written: crate::config::Config = toml::from_str(&rendered).map_err(|e| {
        SenvError::config(&path, format!("senv wrote a config it cannot parse: {e}"))
    })?;
    written.validate(&path)?;
    let mut baselined = project.clone();
    baselined.config = written;
    baselined.record_trust()?;

    let out = AllowOutput {
        config: path.display().to_string(),
        added,
        already_present: already,
        net: existing,
        warnings,
    };
    ctx.emit(&out, render_allow)?;
    Ok(0)
}

fn render_allow(o: &AllowOutput) {
    for w in &o.warnings {
        eprintln!("warning: {}", exec::wrap(w, 74, 9));
    }
    if !o.added.is_empty() {
        println!("allowed {} in {}", o.added.join(", "), o.config);
    }
    if !o.already_present.is_empty() && o.added.is_empty() && o.warnings.is_empty() {
        println!("already allowed: {}", o.already_present.join(", "));
    }
    if !o.net.is_empty() {
        println!("the run phase may now reach: {}", o.net.join(", "));
    }
}

// ── trust ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TrustOutput {
    pub config: String,
    pub accepted: Vec<String>,
}

/// Accept the configuration on disk as the baseline.
pub fn trust(ctx: &Ctx) -> Result<i32> {
    let project = ctx.project_unchecked()?;
    let accepted = project.trust_verdict().widenings().to_vec();
    project.record_trust()?;
    let out = TrustOutput {
        config: project.config_path.display().to_string(),
        accepted,
    };
    ctx.emit(&out, |o| {
        if o.accepted.is_empty() {
            println!(
                "nothing to accept — {} already matches what senv recorded",
                o.config
            );
            return;
        }
        println!("accepted {} widening(s) in {}:", o.accepted.len(), o.config);
        for w in &o.accepted {
            println!("  + {w}");
        }
    })?;
    Ok(0)
}

// ── doctor ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DoctorOutput {
    pub os: String,
    pub mechanism: String,
    pub strongest_tier: String,
    pub syscall_filter: bool,
    pub memory_limit: bool,
    pub process_limit: bool,
    pub egress_allowlist: bool,
    pub tiers: Vec<TierStatus>,
    pub supervised_missing: Vec<String>,
    pub uv: Option<String>,
    pub verdicts: Vec<PhaseVerdict>,
}

#[derive(Debug, Serialize)]
pub struct TierStatus {
    pub tier: String,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PhaseVerdict {
    pub phase: String,
    pub ok: bool,
    pub detail: String,
}

pub fn doctor(ctx: &Ctx) -> Result<i32> {
    let caps = h5i_sandbox::sandbox::capabilities_report();
    let missing = policy::supervisor_missing();

    let tiers = caps
        .claims
        .iter()
        .filter(|c| c.claim != "workspace")
        .map(|c| TierStatus {
            tier: c.claim.to_string(),
            available: c.satisfiable && c.runnable.unwrap_or(true),
            note: c.note.map(|n| n.to_string()),
        })
        .collect();

    // Report on the real project when there is one; otherwise on the defaults,
    // so `senv doctor` works before `senv init`.
    let project = ctx.project_unchecked().ok();
    if let Some(p) = &project {
        ctx.warn_if_untrusted(p);
    }
    // A project-specified uv is deliberately not executed here: running it to
    // read its version would be running an attacker-chosen binary unconfined.
    let project_specified = project
        .as_ref()
        .is_some_and(crate::uv::is_project_specified);
    let uv_version = if project_specified {
        match project.as_ref().map(crate::uv::find) {
            // Reported, not executed: running a configured path to read its
            // version is running an attacker-chosen binary unconfined. And
            // since it is attacker-chosen, it is sanitized before it is
            // printed — `[env] uv` comes from senv.toml.
            Some(Ok(path)) => Some(format!(
                "{} (configured; not executed here)",
                util::sanitize(&path.display().to_string())
            )),
            Some(Err(e)) => Some(format!("REFUSED — {}", util::sanitize(&e.to_string()))),
            None => None,
        }
    } else {
        crate::uv::find_for(project.as_ref().map(|p| &p.config))
            .ok()
            .and_then(|p| crate::uv::version(&p))
    };

    let mut verdicts = Vec::new();
    if let Some(project) = &project {
        let uv_bin = crate::uv::find(project).ok();
        for phase in [Phase::Install, Phase::Run] {
            let opts = PlanOptions {
                uv: uv_bin.clone(),
                ..Default::default()
            };
            let (ok, detail) = match policy::plan(project, phase, &opts) {
                Ok(plan) => (
                    true,
                    format!(
                        "{} tier, network {}",
                        plan.tier().as_str(),
                        describe_net(&plan.policy.profile)
                    ),
                ),
                Err(e) => (false, e.to_string()),
            };
            verdicts.push(PhaseVerdict {
                phase: phase.as_str().to_string(),
                ok,
                detail,
            });
        }
    }

    // Against the tiers senv actually runs, not against the host's best.
    //
    // h5i's `memory_limit` answers "can anything on this machine cap memory",
    // and it is true as soon as a container or microVM runtime is installed.
    // senv never uses those tiers — its phases are `process` and `supervised` —
    // so on a Mac with `msb` on PATH, `senv doctor` printed "memory limits yes"
    // directly above a tier line reading "no memory cap". Ask the same question
    // senv's own phases will be answered by.
    let limits = h5i_sandbox::sandbox::limit_support(
        h5i_sandbox::sandbox_policy::IsolationClaim::Supervised,
    );

    let out = DoctorOutput {
        os: caps.os.clone(),
        mechanism: caps.mechanism.to_string(),
        strongest_tier: caps.strongest_tier.to_string(),
        syscall_filter: caps.syscall_filter,
        memory_limit: limits.mem,
        process_limit: limits.procs,
        egress_allowlist: missing.is_empty() || caps.egress_enforced,
        tiers,
        supervised_missing: missing,
        uv: uv_version,
        verdicts,
    };
    let ok = out.verdicts.iter().all(|v| v.ok);
    ctx.emit(&out, render_doctor)?;
    Ok(if ok { 0 } else { 1 })
}

fn render_doctor(o: &DoctorOutput) {
    println!("host         {} ({})", o.os, o.mechanism);
    println!(
        "uv           {}",
        o.uv.clone().unwrap_or_else(|| "NOT FOUND".to_string())
    );
    println!();
    println!("enforcement");
    println!("  syscall filter    {}", yes_no(o.syscall_filter));
    println!("  memory limits     {}", yes_no(o.memory_limit));
    println!("  process limits    {}", yes_no(o.process_limit));
    println!("  egress allowlist  {}", yes_no(o.egress_allowlist));
    println!();
    println!("isolation tiers");
    for tier in &o.tiers {
        let note = tier
            .note
            .clone()
            .map(|n| format!("  ({n})"))
            .unwrap_or_default();
        println!("  {:<20} {}{note}", tier.tier, yes_no(tier.available));
    }
    if !o.supervised_missing.is_empty() {
        println!("\nthe supervised tier is unavailable because:");
        for m in &o.supervised_missing {
            println!("  - {m}");
        }
        println!(
            "  {}",
            exec::wrap(
                &format!(
                    "Installs need it to restrict egress to package registries. {} Without it, \
                     set [install] net = \"host\" to install with unrestricted network access — \
                     warned and recorded.",
                    policy::supervisor_remedy()
                ),
                74,
                2
            )
        );
    }
    if !o.verdicts.is_empty() {
        println!("\nthis project");
        for v in &o.verdicts {
            println!("  {:<9} {} {}", v.phase, yes_no(v.ok), v.detail);
        }
    }
}

fn yes_no(v: bool) -> &'static str {
    if v { "yes" } else { "no " }
}

// ── gc ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct GcOutput {
    pub scanned: usize,
    pub removable: Vec<GcEntry>,
    pub removed: Vec<String>,
    pub freed: String,
    pub pruned: bool,
}

#[derive(Debug, Serialize)]
pub struct GcEntry {
    /// The state directory's name. Shown only in `--json` output, where it is
    /// the stable handle for a project's state.
    #[allow(dead_code)]
    pub key: String,
    pub path: String,
    pub project: String,
    pub size: String,
    pub reason: String,
}

pub fn gc(ctx: &Ctx, args: &crate::cli::GcArgs) -> Result<i32> {
    let root = project::state_root()?.join("projects");
    let mut removable = Vec::new();
    let mut scanned = 0;
    let mut freed_bytes = 0u64;
    let mut removed = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            scanned += 1;
            // Three cases, and conflating them cost a live project both its
            // environment and its receipts: an unreadable `state.json` is not
            // evidence that a project is gone, it is evidence that senv cannot
            // tell — and the safe answer to "cannot tell" is to leave it alone.
            // An environment on disk settles it either way, and receipts are
            // the one thing senv promises nothing can destroy.
            let raw = std::fs::read_to_string(dir.join("state.json"));
            let has_env = dir.join("venv").join("pyvenv.cfg").is_file();
            let state: Option<project::State> =
                raw.as_ref().ok().and_then(|t| serde_json::from_str(t).ok());
            let project_root = state
                .as_ref()
                .map(|s| s.project_root.clone())
                .unwrap_or_default();
            let reason = match state {
                // A readable, parseable record: trust what it says.
                Some(_) if !project_root.is_empty() => {
                    if PathBuf::from(&project_root).exists() {
                        None
                    } else {
                        Some("the project directory no longer exists")
                    }
                }
                // Anything else is unidentifiable. Remove it only when there is
                // no environment here to lose.
                _ if has_env => None,
                _ => Some("no usable state.json and no environment — nothing identifies a project"),
            };
            let Some(reason) = reason else { continue };
            let size = util::dir_size(&dir);
            removable.push(GcEntry {
                key: entry.file_name().to_string_lossy().to_string(),
                path: dir.display().to_string(),
                project: project_root,
                size: util::human_bytes(size),
                reason: reason.to_string(),
            });
            if args.prune {
                fs::remove_dir_all(&dir)?;
                removed.push(dir.display().to_string());
                freed_bytes += size;
            }
        }
    }

    if args.cache
        && let Ok(project) = ctx.project_unchecked()
    {
        let cache = project.cache();
        let size = util::dir_size(&cache);
        if args.prune {
            if cache.exists() {
                fs::remove_dir_all(&cache)?;
            }
            removed.push(cache.display().to_string());
            freed_bytes += size;
        } else {
            removable.push(GcEntry {
                key: "cache".to_string(),
                path: cache.display().to_string(),
                project: project.root.display().to_string(),
                size: util::human_bytes(size),
                reason: "this project's wheel cache (--cache)".to_string(),
            });
        }
    }

    let out = GcOutput {
        scanned,
        removable,
        removed,
        freed: util::human_bytes(freed_bytes),
        pruned: args.prune,
    };
    ctx.emit(&out, |o| {
        println!(
            "scanned {} project state director{}",
            o.scanned,
            if o.scanned == 1 { "y" } else { "ies" }
        );
        if o.removable.is_empty() {
            println!("nothing to remove");
            return;
        }
        for e in &o.removable {
            println!("  {}  {}  — {}", e.size, e.path, e.reason);
        }
        if o.pruned {
            println!("removed {} item(s), freed {}", o.removed.len(), o.freed);
        } else {
            println!("\nnothing was deleted. Re-run with --prune to remove them.");
        }
    })?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggestion_is_valid_toml_that_matches_the_real_schema() {
        // The failure this prevents: senv printing a stanza that the user
        // pastes in, and senv then refusing to load its own suggestion.
        let stanza = suggest_stanza(
            &[
                "api.example.com".to_string(),
                "*.s3.amazonaws.com".to_string(),
            ],
            &["/home/u/datasets".to_string()],
        );
        let cfg: crate::config::Config =
            toml::from_str(&stanza).expect("the suggestion must parse against senv's own schema");
        assert_eq!(cfg.run.net.hosts().len(), 2);
        assert_eq!(cfg.run.fs.read, vec!["/home/u/datasets".to_string()]);
        cfg.validate(std::path::Path::new("senv.toml"))
            .expect("and must validate");
    }

    #[test]
    fn nothing_blocked_produces_no_stanza_to_paste() {
        let stanza = suggest_stanza(&[], &[]);
        assert!(stanza.contains("nothing was blocked"));
        let cfg: crate::config::Config = toml::from_str(&stanza).expect("still valid TOML");
        assert!(cfg.run.net.is_deny());
    }

    #[test]
    fn durations_read_as_the_user_wrote_them() {
        assert_eq!(format_duration(1800), "30m");
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(45), "45s");
        assert!(format_duration(365 * 24 * 3600).contains("unbounded"));
    }

    #[test]
    fn a_uv_invocation_reads_as_a_command_not_a_path() {
        let argv = vec![
            "/home/u/.local/bin/uv".to_string(),
            "sync".to_string(),
            "--locked".to_string(),
        ];
        assert_eq!(shorten_command(&argv), "uv sync --locked");
    }
}
