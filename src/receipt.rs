//! The execution record: what ran, under which policy, and what the boundary
//! refused.
//!
//! Receipts are append-only JSONL in senv's state directory — **outside every
//! grant senv issues**. Nothing running under a senv policy can write here, by
//! construction, which is the only reason the file is worth reading after an
//! incident.
//!
//! Two things are done before any byte is written: secret values collected by
//! the broker are removed verbatim, and h5i's pattern scanner takes a second
//! pass for credentials nobody declared (API keys, JWTs, PEM blocks).
//!
//! The other job of this module is turning a denied operation into a sentence a
//! user can act on. A boundary that only says "Permission denied" trains people
//! to disable it; one that says "your code tried to read ~/datasets — add it
//! here if that was intentional" gets kept.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{IoContext, Result};
use crate::policy::Plan;
use crate::util;

/// Stand-in target for a refusal whose destination the error never named.
pub const UNNAMED_HOST: &str = "(host not named in the error)";

/// How much of a command's output a receipt keeps. Enough to diagnose a
/// failure, bounded so a runaway log cannot fill the disk.
const OUTPUT_TAIL_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Human-readable UTC, because people read these.
    pub ts: String,
    pub ts_ms: u64,
    pub phase: String,
    pub tier: String,
    pub policy_digest: String,
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    pub wall_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rss_kb: Option<i64>,
    pub net: NetRecord,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denials: Vec<Denial>,
    /// Policy facts announced to the user at plan time — downgrades, wide
    /// grants. Recorded so "was this run wide open?" is answerable later.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Secret grants delivered, by name and fingerprint. Never values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Redacted tail of the command's output, kept only when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetRecord {
    /// `deny`, `host`, or `allowlist`.
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<String>,
    /// Populated only by tiers that observe every request (container).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied: Option<u64>,
}

/// One thing the boundary refused, recovered from the command's own output.
///
/// This is inference, not instrumentation: at the kernel tiers there is no
/// egress log to read, so senv reads what the program said when it was refused.
/// Treated accordingly — a denial here is a lead for `senv report --suggest`,
/// never an authority on what happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Denial {
    pub kind: DenialKind,
    /// The path or host the command was refused.
    pub target: String,
    /// Whether widening the policy is a sensible response.
    pub verdict: Verdict,
    /// The line it was inferred from, redacted and trimmed.
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DenialKind {
    Filesystem,
    Network,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// The policy could reasonably be widened to allow this.
    Suggestable,
    /// Something was blocked, but the error did not say what — so senv reports
    /// it and has nothing to suggest.
    Unnamed,
    /// senv refuses to suggest widening. Writing to the environment at run
    /// time is the case that matters: it is denied *on purpose*, and a tool
    /// that offered to turn that off would be undoing its own reason to exist.
    ByDesign(String),
}

impl Verdict {
    pub fn is_suggestable(&self) -> bool {
        matches!(self, Verdict::Suggestable)
    }

    fn by_design(reason: &str) -> Verdict {
        Verdict::ByDesign(reason.to_string())
    }

    /// Why this will not be suggested, for `senv report`.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Verdict::ByDesign(r) => Some(r),
            Verdict::Unnamed => Some(
                "the error did not name the host. Check the traceback, then allow it with \
                 `senv allow <host>`.",
            ),
            Verdict::Suggestable => None,
        }
    }
}

/// Everything the receipt needs about how a command finished.
pub struct Completion<'a> {
    pub argv: &'a [String],
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub wall_ms: u64,
    pub cpu_ms: Option<u64>,
    pub max_rss_kb: Option<i64>,
    pub output: &'a str,
    pub secrets: Vec<String>,
    /// Exact secret values to strip before anything is written.
    pub redactions: &'a [String],
}

/// The append-only log for one project.
pub struct Receipts {
    path: PathBuf,
    /// Paths that are denied deliberately, so their denials are never
    /// suggested as things to allow.
    protected: Vec<PathBuf>,
}

impl Receipts {
    pub fn new(path: PathBuf, protected: Vec<PathBuf>) -> Receipts {
        Receipts { path, protected }
    }

    /// Build and append the record for a finished command.
    pub fn record(&self, plan: &Plan, done: Completion<'_>) -> Result<Record> {
        let clean = redact(done.output, done.redactions);
        let denials = self.analyze(&clean);
        let failed = done.exit_code != Some(0) || done.timed_out;
        let ts_ms = util::now_ms();

        let profile = &plan.policy.profile;
        let mode = if profile.net_mode == h5i_sandbox::sandbox_policy::NetMode::Deny {
            "deny"
        } else if profile.net_egress.is_empty() {
            "host"
        } else {
            "allowlist"
        };

        let record = Record {
            ts: util::format_utc(ts_ms),
            ts_ms,
            phase: plan.phase.as_str().to_string(),
            tier: plan.tier().as_str().to_string(),
            policy_digest: plan.digest.clone(),
            argv: done.argv.to_vec(),
            exit_code: done.exit_code,
            timed_out: done.timed_out,
            wall_ms: done.wall_ms,
            cpu_ms: done.cpu_ms,
            max_rss_kb: done.max_rss_kb,
            net: NetRecord {
                mode: mode.to_string(),
                egress: profile.net_egress.clone(),
                allowed: None,
                denied: None,
            },
            denials,
            notes: plan.notes.iter().map(|n| n.text.clone()).collect(),
            secrets: done.secrets,
            // A successful command's output is the user's business, not the
            // log's. Keeping it would put arbitrary program output — and
            // whatever it printed — into a file senv promises to keep clean.
            output_tail: failed.then(|| util::tail(clean.trim_end(), OUTPUT_TAIL_BYTES)),
        };
        self.append(&record)?;
        Ok(record)
    }

    pub fn append(&self, record: &Record) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            crate::error::fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_string(record)
            .map_err(|e| crate::error::SenvError::internal(format!("serializing receipt: {e}")))?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .at(&self.path)?;
        file.write_all(line.as_bytes()).at(&self.path)?;
        Ok(())
    }

    /// Every record, oldest first. A malformed line is skipped rather than
    /// failing the read: a truncated tail must not make the whole history
    /// unreadable.
    pub fn read(&self) -> Vec<Record> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Record>(l).ok())
            .collect()
    }

    /// Infer denials from a command's output.
    pub fn analyze(&self, output: &str) -> Vec<Denial> {
        let lines: Vec<&str> = output.lines().map(str::trim).collect();
        let mut found: Vec<Denial> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            if is_network_refusal(line) {
                // A Python traceback names the host on the *source* line and
                // reports the failure several lines later, so the destination
                // is usually just above the error rather than in it.
                let target = host_from_line(line)
                    .or_else(|| host_from_context(&lines, i))
                    .unwrap_or_else(|| UNNAMED_HOST.to_string());
                let verdict = if target == UNNAMED_HOST {
                    Verdict::Unnamed
                } else {
                    Verdict::Suggestable
                };
                push_unique(
                    &mut found,
                    Denial {
                        kind: DenialKind::Network,
                        target,
                        verdict,
                        evidence: util::tail(line, 200),
                    },
                );
            } else if let Some(target) = filesystem_denial(line) {
                let verdict = self.classify_path(&target);
                push_unique(
                    &mut found,
                    Denial {
                        kind: DenialKind::Filesystem,
                        target,
                        verdict,
                        evidence: util::tail(line, 200),
                    },
                );
            }
        }
        // A traceback that names its host on one line and fails on another
        // produces both an anonymous and a named denial for the same event.
        // The named one is strictly more useful.
        if found
            .iter()
            .any(|d| d.kind == DenialKind::Network && d.target != UNNAMED_HOST)
        {
            found.retain(|d| !(d.kind == DenialKind::Network && d.target == UNNAMED_HOST));
        }
        found
    }

    /// Is widening the policy to allow this path a reasonable suggestion?
    fn classify_path(&self, path: &str) -> Verdict {
        let p = Path::new(path);
        for protected in &self.protected {
            if p == protected || p.starts_with(protected) {
                return Verdict::by_design(
                    "the environment is read-only while your code runs, so packages cannot be \
                     patched between runs. Change it with `senv add`/`senv sync`, not by \
                     widening the run policy.",
                );
            }
        }
        // Granting a credential directory is the request senv should never
        // help with, even when a program genuinely wants it.
        const NEVER: [&str; 6] = [
            ".ssh",
            ".aws",
            ".gnupg",
            ".netrc",
            ".git-credentials",
            ".pypirc",
        ];
        if NEVER.iter().any(|c| path.contains(c)) {
            return Verdict::by_design(
                "this is a credential path. senv will not suggest granting it; if a tool truly \
                 needs one, pass it as a declared secret instead.",
            );
        }
        Verdict::Suggestable
    }
}

fn push_unique(list: &mut Vec<Denial>, d: Denial) {
    if !list
        .iter()
        .any(|e| e.kind == d.kind && e.target == d.target)
    {
        list.push(d);
    }
}

/// Strip declared secret values, then run h5i's credential scanner over what
/// remains.
pub fn redact(text: &str, values: &[String]) -> String {
    let mut out = text.to_string();
    for v in values {
        if v.len() >= 4 {
            out = out.replace(v.as_str(), "«redacted»");
        }
    }
    h5i_sandbox::secrets::redact_text(&out)
}

// ── denial inference ────────────────────────────────────────────────────────

/// Does this line report a refused network operation, whether or not it names
/// the destination?
fn is_network_refusal(line: &str) -> bool {
    NETWORK_MARKERS.iter().any(|m| line.contains(m))
}

const NETWORK_MARKERS: [&str; 8] = [
    "Network is unreachable",
    "Temporary failure in name resolution",
    "Name or service not known",
    "nodename nor servname provided",
    "Could not resolve host",
    "failed to lookup address information",
    "No route to host",
    "getaddrinfo",
];

/// Pull a hostname out of a line, conservatively.
///
/// Conservative on purpose. A false positive here becomes a suggestion to
/// allowlist a host the user never contacted, which is worse than staying
/// quiet: a missed denial costs a suggestion, a wrong one costs trust in every
/// suggestion. So senv only accepts a host it can see in a URL, in quotes, or
/// immediately after a phrase that names one — never a bare dotted token, which
/// in Python output is far more often a module path (`socket.gaierror`) than a
/// hostname.
fn host_from_line(line: &str) -> Option<String> {
    if let Some(host) = url_host(line) {
        return Some(host);
    }
    if let Some(host) = quoted_host(line) {
        return Some(host);
    }
    for marker in [
        "Could not resolve host:",
        "resolve host",
        "hostname",
        "connect to",
    ] {
        if let Some(idx) = line.find(marker) {
            let rest = line[idx + marker.len()..].trim_start_matches([':', ' ', '\'', '"']);
            let token: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .collect();
            if looks_like_host(&token) {
                return Some(token);
            }
        }
    }
    None
}

/// Does this token have the shape of a hostname a user could allowlist?
///
/// Uppercase is the useful signal: `NameResolutionError` and `Client.Timeout`
/// are dotted identifiers, and real hostnames in error output are lowercase.
fn looks_like_host(token: &str) -> bool {
    let token = token.trim();
    if token.len() < 4 || token.len() > 253 || !token.contains('.') {
        return false;
    }
    if token.starts_with('.') || token.ends_with('.') || token.starts_with('/') {
        return false;
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
    {
        return false;
    }
    // Python module paths are the dominant false positive; none of these is a
    // registrable domain.
    const MODULES: [&str; 12] = [
        "socket", "urllib", "urllib3", "requests", "ssl", "http", "httpx", "aiohttp", "botocore",
        "asyncio", "self", "os",
    ];
    let mut labels = token.split('.');
    let first = labels.next().unwrap_or_default();
    if MODULES.contains(&first) {
        return false;
    }
    let tld = token.split('.').next_back().unwrap_or_default();
    // A trailing file extension is the other common shape that parses as a
    // two-label hostname (`setup.py`, `pyproject.toml`).
    const EXTENSIONS: [&str; 30] = [
        "py", "pyc", "pyi", "txt", "toml", "json", "lock", "cfg", "ini", "md", "yaml", "yml", "sh",
        "rs", "so", "whl", "tar", "gz", "zip", "log", "xml", "html", "csv", "db", "sqlite", "egg",
        "bak", "tmp", "conf", "pth",
    ];
    if EXTENSIONS.contains(&tld) {
        return false;
    }
    tld.len() >= 2 && tld.len() <= 24 && tld.bytes().all(|b| b.is_ascii_lowercase())
}

/// Look back a few lines for the host a refusal was about.
///
/// Bounded to the lines just above the error: a traceback frame is right there,
/// and searching further would start attributing a failure to a hostname
/// printed by something unrelated.
fn host_from_context(lines: &[&str], error_index: usize) -> Option<String> {
    const WINDOW: usize = 10;
    let start = error_index.saturating_sub(WINDOW);
    lines[start..error_index]
        .iter()
        .rev()
        .find_map(|line| url_host(line).or_else(|| quoted_host(line)))
}

/// A host inside quotes — `('api.example.com', 443)` in a traceback frame.
fn quoted_host(line: &str) -> Option<String> {
    for quote in ['\'', '"'] {
        let mut parts = line.split(quote);
        parts.next();
        while let Some(inner) = parts.next() {
            if looks_like_host(inner) {
                return Some(inner.to_string());
            }
            parts.next();
        }
    }
    None
}

fn url_host(line: &str) -> Option<String> {
    for scheme in ["https://", "http://"] {
        if let Some(idx) = line.find(scheme) {
            let rest = &line[idx + scheme.len()..];
            let host: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .collect();
            if host.contains('.') {
                return Some(host);
            }
        }
    }
    None
}

/// Absolute path a line was refused, if it reads like a blocked file
/// operation.
fn filesystem_denial(line: &str) -> Option<String> {
    const MARKERS: [&str; 4] = [
        "Permission denied",
        "Read-only file system",
        "Operation not permitted",
        "EACCES",
    ];
    if !MARKERS.iter().any(|m| line.contains(m)) {
        return None;
    }
    // The longest absolute path on the line is nearly always the subject.
    line.split(|c: char| c.is_whitespace() || c == '\'' || c == '"' || c == '(' || c == ')')
        .filter(|t| t.starts_with('/') && t.len() > 1)
        .map(|t| t.trim_end_matches([':', ',', '.', ';']).to_string())
        .max_by_key(|t| t.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipts() -> Receipts {
        Receipts::new(
            PathBuf::from("/dev/null"),
            vec![PathBuf::from("/state/venv")],
        )
    }

    #[test]
    fn a_blocked_connection_names_the_host() {
        let r = receipts();
        let out = "urllib3.exceptions.NameResolutionError: Failed to resolve 'api.example.com' \
                   ([Errno -3] Temporary failure in name resolution)";
        let d = r.analyze(out);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].kind, DenialKind::Network);
        assert_eq!(d[0].target, "api.example.com");
        assert!(d[0].verdict.is_suggestable());
    }

    #[test]
    fn a_blocked_url_fetch_names_the_host() {
        let r = receipts();
        let d = r.analyze(
            "error sending request for url (https://download.pytorch.org/whl/torch.whl): \
             Network is unreachable",
        );
        assert_eq!(d[0].target, "download.pytorch.org");
    }

    #[test]
    fn a_blocked_read_names_the_path() {
        let r = receipts();
        let d =
            r.analyze("PermissionError: [Errno 13] Permission denied: '/home/u/datasets/x.csv'");
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].kind, DenialKind::Filesystem);
        assert_eq!(d[0].target, "/home/u/datasets/x.csv");
        assert!(d[0].verdict.is_suggestable());
    }

    #[test]
    fn writing_to_the_environment_is_never_suggested_as_something_to_allow() {
        // The failure this prevents: senv helpfully offering to make the venv
        // writable, which would undo the guarantee it exists to provide.
        let r = receipts();
        let d = r.analyze(
            "OSError: [Errno 30] Read-only file system: '/state/venv/lib/python3.13/x.py'",
        );
        assert_eq!(d.len(), 1);
        assert!(!d[0].verdict.is_suggestable(), "{:?}", d[0].verdict);
        let reason = d[0].verdict.reason().expect("a by-design verdict");
        assert!(reason.contains("read-only"), "{reason}");
    }

    #[test]
    fn credential_paths_are_never_suggested_either() {
        let r = receipts();
        let d = r.analyze("cat: /home/u/.ssh/id_ed25519: Permission denied");
        assert!(!d[0].verdict.is_suggestable());
        let reason = d[0].verdict.reason().expect("a by-design verdict");
        assert!(reason.contains("credential"), "{reason}");
    }

    #[test]
    fn a_traceback_that_names_the_host_above_the_error_is_understood() {
        // The real shape of a blocked connection in Python: the destination is
        // on the source line, the failure several frames later.
        let r = receipts();
        let traceback = "Traceback (most recent call last):\n  \
                         File \"/proj/probe.py\", line 2, in <module>\n    \
                         socket.create_connection(('api.example.com', 443), 5)\n  \
                         File \"/usr/lib/python3.12/socket.py\", line 828, in create_connection\n    \
                         for res in getaddrinfo(host, port, 0, SOCK_STREAM):\n\
                         socket.gaierror: [Errno -3] Temporary failure in name resolution";
        let d = r.analyze(traceback);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].target, "api.example.com");
        assert!(d[0].verdict.is_suggestable());
    }

    #[test]
    fn a_refusal_with_no_host_anywhere_is_still_reported() {
        let r = receipts();
        let d = r.analyze("socket.gaierror: [Errno -3] Temporary failure in name resolution");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].target, UNNAMED_HOST);
        assert!(
            !d[0].verdict.is_suggestable(),
            "there is nothing to suggest"
        );
        assert!(d[0].verdict.reason().unwrap().contains("did not name"));
    }

    #[test]
    fn ordinary_output_produces_no_denials() {
        // A false positive here becomes a suggestion to widen the boundary for
        // no reason, so the bar is high.
        let r = receipts();
        for benign in [
            "Successfully installed requests-2.32.0",
            "test_permissions.py::test_denied PASSED",
            "INFO: connected to database at db.internal",
            "  1 passed in 0.42s",
        ] {
            assert!(r.analyze(benign).is_empty(), "false positive on: {benign}");
        }
    }

    #[test]
    fn the_same_denial_is_recorded_once_however_often_it_repeats() {
        let r = receipts();
        let noisy = (0..50)
            .map(|_| "Permission denied: '/home/u/data'")
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(r.analyze(&noisy).len(), 1);
    }

    #[test]
    fn declared_secrets_and_stray_credentials_are_stripped_before_writing() {
        // A GitHub PAT nobody declared, alongside a declared value.
        let pat = format!("ghp_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let text = format!("token=supersecretvalue123 and {pat}");
        let clean = redact(&text, &["supersecretvalue123".to_string()]);
        assert!(!clean.contains("supersecretvalue123"), "{clean}");
        assert!(clean.contains("«redacted»"));
        // The declared value is senv's guarantee; the pattern scan is
        // defence in depth. If h5i's ruleset ever stops covering a shape this
        // obvious, this test is how we find out.
        assert!(
            !clean.contains(&pat),
            "undeclared credential leaked into a receipt: {clean}"
        );
    }

    #[test]
    fn a_module_path_is_never_mistaken_for_a_host() {
        // The bug this caught: `urllib3.exceptions.NameResolutionError` being
        // reported as the host to allowlist.
        assert!(!looks_like_host("urllib3.exceptions.NameResolutionError"));
        assert!(!looks_like_host("socket.gaierror"));
        assert!(!looks_like_host("requests.exceptions.ConnectionError"));
        assert!(!looks_like_host("setup.py"));
        assert!(looks_like_host("api.example.com"));
        assert!(looks_like_host("files.pythonhosted.org"));
    }

    #[test]
    fn records_round_trip_and_a_corrupt_line_does_not_hide_the_others() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("receipt.jsonl");
        let r = Receipts::new(path.clone(), Vec::new());
        let rec = Record {
            ts: util::format_utc(0),
            ts_ms: 0,
            phase: "run".into(),
            tier: "process".into(),
            policy_digest: "abc".into(),
            argv: vec!["pytest".into()],
            exit_code: Some(0),
            timed_out: false,
            wall_ms: 12,
            cpu_ms: None,
            max_rss_kb: None,
            net: NetRecord {
                mode: "deny".into(),
                egress: vec![],
                allowed: None,
                denied: None,
            },
            denials: vec![],
            notes: vec![],
            secrets: vec![],
            output_tail: None,
        };
        r.append(&rec).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{truncated\n")
            .unwrap();
        r.append(&rec).unwrap();

        let read = r.read();
        assert_eq!(read.len(), 2, "a bad line must not hide good records");
        assert_eq!(read[0].argv, vec!["pytest".to_string()]);
    }
}
