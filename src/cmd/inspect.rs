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
    let project = ctx.project()?;
    let state = project.load_state();
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
                project::VenvLink::Absent => "absent".to_string(),
                project::VenvLink::Directory => "a real directory (not senv's)".to_string(),
                project::VenvLink::OtherLink(p) => format!("links elsewhere: {}", p.display()),
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
            if let Some(mem) = p.mem_bytes {
                resources.insert("mem".to_string(), util::human_bytes(mem));
            }
            if let Some(procs) = p.max_procs {
                resources.insert("procs".to_string(), procs.to_string());
            }
            resources.insert("wall".to_string(), format_duration(p.wall_secs));
            PhaseStatus {
                phase: phase.as_str().to_string(),
                tier: Some(plan.tier().as_str().to_string()),
                network: describe_net(p),
                writable: writable_summary(project, p, phase),
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
            unavailable: Some(e.to_string()),
        },
    }
}

fn describe_net(p: &h5i_sandbox::sandbox_policy::Profile) -> String {
    if p.net_mode == h5i_sandbox::sandbox_policy::NetMode::Deny {
        "denied".to_string()
    } else if p.net_egress.is_empty() {
        "UNRESTRICTED".to_string()
    } else {
        p.net_egress.join(", ")
    }
}

/// Writable paths, with `$WORK` spelled out — it is implicit in the profile and
/// invisible to a reader who does not know that.
fn writable_summary(
    project: &Project,
    p: &h5i_sandbox::sandbox_policy::Profile,
    phase: Phase,
) -> Vec<String> {
    let mut out = Vec::new();
    if phase.is_run_like()
        || p.fs_read
            .iter()
            .all(|r| r != &project.root.display().to_string())
    {
        out.push(format!("{} (the project)", project.root.display()));
    }
    out.extend(
        p.fs_write
            .iter()
            .filter(|w| !matches!(w.as_str(), "$WORK" | "/dev/null" | "/dev/zero"))
            .cloned(),
    );
    out
}

/// The read-only grants worth showing.
///
/// h5i's baseline system paths are filtered out — they are the same on every
/// project and drown the two lines that matter. Matched exactly rather than by
/// prefix: a project or environment that happens to live under `/tmp` is a
/// real grant a reader needs to see, and prefix-matching silently hid it.
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
    p.fs_read
        .iter()
        .filter(|r| !BASELINE.contains(&r.as_str()))
        .map(|r| {
            if *r == venv {
                format!("{r} (the environment, read-only)")
            } else if *r == root {
                format!("{r} (the project, read-only)")
            } else {
                r.clone()
            }
        })
        .collect()
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
    let project = ctx.project()?;
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
            out.push_str(&format!("  \"{h}\",\n"));
        }
        out.push_str("]\n");
    }
    if !paths.is_empty() {
        out.push_str("\n[run.fs]\nread = [\n");
        for p in paths {
            out.push_str(&format!("  \"{p}\",\n"));
        }
        out.push_str("]\n");
    }
    out
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
    let project = ctx.project()?;
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
    let text = if path.is_file() {
        fs::read_to_string(&path)?
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

    project::write_atomic(&path, doc.to_string().as_bytes())?;

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

// ── doctor ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DoctorOutput {
    pub os: String,
    pub mechanism: String,
    pub strongest_tier: String,
    pub syscall_filter: bool,
    pub memory_limit: bool,
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
    let project = ctx.project().ok();
    let uv_version = crate::uv::find_for(project.as_ref().map(|p| &p.config))
        .ok()
        .and_then(|p| crate::uv::version(&p));

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

    let out = DoctorOutput {
        os: caps.os.clone(),
        mechanism: caps.mechanism.to_string(),
        strongest_tier: caps.strongest_tier.to_string(),
        syscall_filter: caps.syscall_filter,
        memory_limit: caps.memory_limit,
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
                "Installs need it to restrict egress to package registries. On Debian/Ubuntu: \
                 sudo apt install slirp4netns nftables. Without it, set [install] net = \
                 \"host\" to install with unrestricted network access — warned and recorded.",
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
            let state: Option<project::State> = std::fs::read_to_string(dir.join("state.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok());
            let project_root = state
                .as_ref()
                .map(|s| s.project_root.clone())
                .unwrap_or_default();
            let reason = if project_root.is_empty() {
                Some("no state.json — senv cannot tell which project this belongs to")
            } else if !PathBuf::from(&project_root).exists() {
                Some("the project directory no longer exists")
            } else {
                None
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
        && let Ok(project) = ctx.project()
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
