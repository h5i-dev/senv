//! Executing a [`Plan`]: brokering secrets, invoking h5i, recording the result.
//!
//! Two execution modes, and the difference is deliberate:
//!
//! - **Captured** ([`run_captured`]) for the install phase. senv needs the full
//!   output to write a receipt and to infer what the boundary refused, so the
//!   child's streams are collected and printed on completion. A progress line
//!   keeps a slow `uv sync` from looking hung.
//! - **Streaming** ([`run_streaming`]) for `senv run` and `senv shell`, where
//!   the child owns the terminal: pytest's live output, a REPL, a dev server's
//!   log. Nothing is captured, so the receipt records the outcome but not the
//!   output — which is also the right privacy default for a command that may
//!   print anything at all.

use std::io::Write;

use h5i_sandbox::sandbox;
use h5i_sandbox::secrets_broker::{self, Brokered};

use crate::error::{Result, SenvError};
use crate::policy::{Note, NoteLevel, Plan};
use crate::project::Project;
use crate::receipt::{Completion, Receipts, Record};
use crate::util;

/// The result of running one command under a policy.
pub struct Execution {
    pub exit_code: i32,
    pub timed_out: bool,
    pub wall_ms: u64,
    /// Empty for streaming runs, which never capture.
    pub output: String,
    pub record: Record,
}

impl Execution {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }
}

/// Run `argv`, capturing output. Used by the install and provisioning phases.
pub fn run_captured(
    project: &Project,
    plan: &Plan,
    argv: &[String],
    progress_label: Option<&str>,
) -> Result<Execution> {
    let brokered = broker(project, plan)?;
    let env = merged_env(plan, brokered.as_ref());
    let start = std::time::Instant::now();

    let outcome = {
        let _progress = progress_label.map(util::Progress::start);
        sandbox::run_with_env(&plan.policy, &plan.work, argv, &env)
            .map_err(|e| explain_engine_error(e, plan))?
    };

    let mut output = String::from_utf8_lossy(&outcome.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&outcome.stderr));

    let wall_ms = outcome.wall_ms.min(u128::from(u64::MAX)) as u64;
    let record = receipts(project).record(
        plan,
        Completion {
            argv,
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            wall_ms,
            cpu_ms: Some(outcome.cpu_ms.min(u128::from(u64::MAX)) as u64),
            max_rss_kb: outcome.max_rss_kb,
            output: &output,
            secrets: grant_summaries(brokered.as_ref()),
            redactions: brokered
                .as_ref()
                .map(|b| b.redactions.as_slice())
                .unwrap_or(&[]),
        },
    )?;

    let _ = start;
    Ok(Execution {
        exit_code: exit_code_of(outcome.exit_code, outcome.timed_out),
        timed_out: outcome.timed_out,
        wall_ms,
        output,
        record,
    })
}

/// Run `argv` with the terminal attached. Used by `senv run` and `senv shell`.
///
/// `observe_stderr` mirrors the child's stderr through senv on its way to the
/// terminal, so a denial can still be recognised and explained. It is off for
/// `senv shell`, where the child owns the terminal completely and anything in
/// the way risks breaking line editing.
pub fn run_streaming(
    project: &Project,
    plan: &Plan,
    argv: &[String],
    observe_stderr: bool,
) -> Result<Execution> {
    let brokered = broker(project, plan)?;
    let env = merged_env(plan, brokered.as_ref());

    let start = std::time::Instant::now();
    let tee = observe_stderr.then(StderrTee::install).flatten();
    let outcome = sandbox::run_interactive(&plan.policy, &plan.work, argv, &env, None);
    let observed = tee.map(|t| t.finish()).unwrap_or_default();
    let outcome = outcome.map_err(|e| explain_engine_error(e, plan))?;
    let wall_ms = start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

    // Only what the child printed on stderr is known here — stdout went
    // straight to the terminal, and the kernel tiers keep no egress log. The
    // receipt records what it can and claims nothing else.
    let record = receipts(project).record(
        plan,
        Completion {
            argv,
            exit_code: Some(outcome.exit_code),
            timed_out: false,
            wall_ms,
            cpu_ms: None,
            max_rss_kb: None,
            output: &observed,
            secrets: grant_summaries(brokered.as_ref()),
            redactions: brokered
                .as_ref()
                .map(|b| b.redactions.as_slice())
                .unwrap_or(&[]),
        },
    )?;

    Ok(Execution {
        exit_code: outcome.exit_code,
        timed_out: false,
        wall_ms,
        output: String::new(),
        record,
    })
}

/// Mirrors this process's stderr into a buffer while still writing it through
/// to the real terminal.
///
/// `run_interactive` hands the child senv's own stdio, so redirecting fd 2 here
/// redirects the child's. stdout is deliberately left alone: it is what most
/// tools test with `isatty` to decide on colour and progress, and a `senv run
/// pytest` that lost its colours would be a visible tax for an invisible
/// benefit.
struct StderrTee {
    /// The real stderr, kept so the reader thread can write through to it.
    saved: libc::c_int,
    reader: std::thread::JoinHandle<String>,
}

/// Cap on mirrored stderr. A command that prints megabytes must not be able to
/// grow senv's memory without bound; the tail is the part that explains a
/// failure anyway.
const MAX_OBSERVED: usize = 256 * 1024;

impl StderrTee {
    /// Returns `None` if the plumbing cannot be set up — in which case senv
    /// simply observes nothing, rather than failing a command the user asked
    /// for.
    fn install() -> Option<StderrTee> {
        let _ = std::io::stderr().flush();
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a two-element array, which is what pipe(2) writes.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return None;
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: duplicating and replacing fd 2; both are checked below.
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved < 0 || unsafe { libc::dup2(write_fd, libc::STDERR_FILENO) } < 0 {
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
                if saved >= 0 {
                    libc::close(saved);
                }
            }
            return None;
        }
        // fd 2 now refers to the pipe, so this copy is redundant.
        unsafe { libc::close(write_fd) };

        let reader = std::thread::spawn(move || {
            let mut collected = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                // SAFETY: reading into a stack buffer of known length.
                let n = unsafe {
                    libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n <= 0 {
                    break;
                }
                let n = n as usize;
                // Write through to the terminal first: the user's output must
                // never be delayed or dropped by senv's bookkeeping.
                let mut written = 0;
                while written < n {
                    // SAFETY: writing a slice of `buf` that was just filled.
                    let w = unsafe {
                        libc::write(
                            saved,
                            buf[written..n].as_ptr() as *const libc::c_void,
                            n - written,
                        )
                    };
                    if w <= 0 {
                        break;
                    }
                    written += w as usize;
                }
                if collected.len() < MAX_OBSERVED {
                    collected.extend_from_slice(&buf[..n]);
                }
            }
            unsafe { libc::close(read_fd) };
            String::from_utf8_lossy(&collected).into_owned()
        });
        Some(StderrTee { saved, reader })
    }

    /// Restore the real stderr and collect what went past.
    fn finish(self) -> String {
        let _ = std::io::stderr().flush();
        // Restoring fd 2 drops the last reference to the pipe's write end, so
        // the reader thread sees EOF and returns.
        unsafe {
            libc::dup2(self.saved, libc::STDERR_FILENO);
            libc::close(self.saved);
        }
        self.reader.join().unwrap_or_default()
    }
}

/// Translate an engine-level refusal into senv's vocabulary.
///
/// The one that matters in practice: the supervised tier pins its allowlist to
/// resolved addresses, so a host that does not resolve — a typo in
/// `senv allow`, or an internal name that only exists on the VPN — makes every
/// later command fail with a message about egress resolution. Left untranslated
/// it reads as senv being broken, when the fix is one line of config.
fn explain_engine_error(e: h5i_error::H5iError, plan: &Plan) -> SenvError {
    let text = e.to_string();
    if text.contains("net.egress resolved to no reachable address") {
        let hosts = plan.policy.profile.net_egress.join(", ");
        return SenvError::refused(
            "none of the allowed hosts could be resolved, so senv refused to run",
            format!(
                "the allowlist is pinned to resolved addresses, and none of [{hosts}] resolves \
                 from here. A typo, or a name that only exists on a VPN you are not connected \
                 to, produces exactly this."
            ),
            "check the spelling in senv.toml under [run] net, remove the host, or connect to \
             the network where it resolves",
        );
    }
    SenvError::Sandbox(e)
}

fn receipts(project: &Project) -> Receipts {
    // The environment is the path senv refuses to suggest making writable; the
    // state directory holds the receipts themselves.
    Receipts::new(
        project.receipt_path(),
        vec![project.venv(), project.state_dir.clone()],
    )
}

/// Resolve declared secrets into values, fail-closed.
///
/// Returns `None` when the policy grants none, which is always true for the
/// install and provisioning phases.
fn broker(project: &Project, plan: &Plan) -> Result<Option<Brokered>> {
    let grants = &plan.policy.profile.secret_grants;
    if grants.is_empty() {
        return Ok(None);
    }
    let secret_dir = project.state_dir.join("secrets");
    crate::error::fs::create_dir_all(&secret_dir)?;
    let fp_key = secrets_broker::fingerprint_key(&project.state_dir)?;
    let brokered = secrets_broker::broker(
        grants,
        &secret_dir,
        // Never the workspace tier: senv has no unconfined path, which is why
        // `inject = "file"` is refused with a clear message from the broker.
        false,
        plan.policy.profile.allow_command_extractors,
        &fp_key,
    )?;
    Ok(Some(brokered))
}

/// Policy environment plus brokered secrets. Secrets go last so a grant is
/// never shadowed.
fn merged_env(plan: &Plan, brokered: Option<&Brokered>) -> Vec<(String, String)> {
    let mut env = plan.env.clone();
    if let Some(b) = brokered {
        env.extend(b.env.iter().cloned());
    }
    env
}

/// Grant names and value fingerprints — never values.
fn grant_summaries(brokered: Option<&Brokered>) -> Vec<String> {
    brokered
        .map(|b| b.records.iter().map(|r| r.detail()).collect())
        .unwrap_or_default()
}

/// A command killed by a signal has no exit code. Report it the way a shell
/// does (128 + signal is not recoverable here, so use the conventional 137-ish
/// stand-in of 1) rather than pretending it succeeded.
fn exit_code_of(code: Option<i32>, timed_out: bool) -> i32 {
    match code {
        Some(c) => c,
        None if timed_out => 124, // the `timeout(1)` convention
        None => 1,
    }
}

/// Print the notes attached to a plan — the announced widenings.
pub fn print_notes(notes: &[Note]) {
    for note in notes {
        let (marker, stream): (&str, fn(&str)) = match note.level {
            NoteLevel::Warn => ("warning", |s| eprintln!("{s}")),
            NoteLevel::Info => ("note", |s| eprintln!("{s}")),
        };
        stream(&format!("{marker}: {}", wrap(&note.text, 76, 9)));
    }
}

/// Print what the boundary refused, with the change that would allow it.
///
/// This is the message that decides whether someone keeps using senv. It has to
/// be specific about what happened and exact about the fix.
pub fn print_denials(record: &Record) {
    if record.denials.is_empty() {
        return;
    }
    eprintln!();
    eprintln!("senv blocked {} operation(s):", record.denials.len());
    for d in &record.denials {
        let what = match d.kind {
            crate::receipt::DenialKind::Network if d.target == crate::receipt::UNNAMED_HOST => {
                "a network connection".to_string()
            }
            crate::receipt::DenialKind::Network => format!("network access to {}", d.target),
            crate::receipt::DenialKind::Filesystem => format!("access to {}", d.target),
        };
        eprintln!("  • {what}");
        match d.verdict.reason() {
            // Blocked on purpose, or blocked without naming a target: either
            // way there is an explanation and nothing to suggest.
            Some(reason) => eprintln!("    {}", wrap(reason, 74, 4)),
            None => {
                let fix = match d.kind {
                    crate::receipt::DenialKind::Network => format!("senv allow {}", d.target),
                    crate::receipt::DenialKind::Filesystem => {
                        format!("add \"{}\" to [run.fs] read in senv.toml", d.target)
                    }
                };
                eprintln!("    allow it with: {fix}");
            }
        }
    }
    eprintln!("  `senv report --suggest` turns everything recorded so far into a policy stanza.");
}

/// Wrap text to `width`, indenting continuation lines by `indent`.
pub fn wrap(text: &str, width: usize, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut line = 0;
    for word in text.split_whitespace() {
        if line > 0 && line + 1 + word.len() > width {
            out.push('\n');
            out.push_str(&pad);
            line = 0;
        } else if line > 0 {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += word.len();
    }
    out
}

/// Write captured output to the user's terminal, unchanged.
///
/// Not redacted: this is the command's own output going to the person who ran
/// it, exactly as it would appear without senv. Redaction applies to the
/// *receipt*, which is a file that outlives the session.
pub fn emit_output(output: &str) {
    if output.is_empty() {
        return;
    }
    let mut err = std::io::stderr();
    let _ = err.write_all(output.as_bytes());
    if !output.ends_with('\n') {
        let _ = err.write_all(b"\n");
    }
    let _ = err.flush();
}

/// Fail with a message naming the environment that does not exist yet.
pub fn require_venv(project: &Project) -> Result<()> {
    if project.venv_exists() {
        return Ok(());
    }
    Err(SenvError::refused(
        "this project has no environment yet",
        format!("nothing is installed at {}", project.venv().display()),
        "run `senv sync` to create it",
    ))
}

/// The absolute path of an executable inside the environment, if it exists.
///
/// Used to turn `senv run pytest` into an absolute path: the confined child
/// gets a `PATH` senv constructs, and resolving here means a missing tool is
/// reported by senv with a useful message instead of by the shell as a bare
/// `command not found`.
pub fn venv_program(project: &Project, program: &str) -> Option<String> {
    if program.contains('/') {
        return None;
    }
    let candidate = project.venv().join("bin").join(program);
    candidate.is_file().then(|| candidate.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_keeps_words_whole_and_indents_continuations() {
        let text = "the quick brown fox jumps over the lazy dog and keeps going for a while";
        let wrapped = wrap(text, 20, 2);
        for line in wrapped.lines() {
            assert!(line.trim_end().len() <= 22, "too long: {line:?}");
        }
        assert!(wrapped.lines().skip(1).all(|l| l.starts_with("  ")));
        let rejoined: String = wrapped.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(rejoined, text, "wrapping must not lose or reorder words");
    }

    #[test]
    fn a_timeout_reports_the_conventional_code_rather_than_success() {
        assert_eq!(exit_code_of(Some(0), false), 0);
        assert_eq!(exit_code_of(Some(3), false), 3);
        assert_eq!(exit_code_of(None, true), 124);
        assert_eq!(exit_code_of(None, false), 1);
    }
}
