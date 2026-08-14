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
    /// The child's stdout, kept separate from its stderr.
    ///
    /// Merging the two was a real bug, not a tidiness question:
    /// `senv uv -- export --format requirements-txt > reqs.txt` wrote the
    /// requirements to stderr and the status line to stdout, so every redirect
    /// through the documented escape hatch produced the wrong file.
    pub stdout: String,
    pub stderr: String,
    /// Both streams together, for denial analysis and receipts.
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

    let outcome = {
        let _progress = progress_label.map(util::Progress::start);
        sandbox::run_with_env(&plan.policy, &plan.work, argv, &env)
            .map_err(|e| explain_engine_error(e, plan))?
    };

    // Bounded before anything is copied. h5i collects the child's whole output
    // into a Vec with no cap, and senv then copied it three more times (lossy
    // conversion, concatenation, redaction) — a build backend streaming 1.5 GB
    // took senv, the one unconfined process here, to 5 GB of RSS. Slicing first
    // means senv adds a couple of megabytes instead of multiplying. Capping
    // h5i's own read is the real fix and belongs upstream.
    let stdout = bounded(&outcome.stdout);
    let stderr = bounded(&outcome.stderr);
    let output = format!("{stdout}{stderr}");

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

    Ok(Execution {
        exit_code: exit_code_of(outcome.exit_code, outcome.timed_out),
        timed_out: outcome.timed_out,
        wall_ms,
        stdout,
        stderr,
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
        stdout: String::new(),
        stderr: String::new(),
        output: String::new(),
        record,
    })
}

/// The head and tail of a byte stream, with the middle summarized.
///
/// Both ends matter: a failure explains itself at the end, and the command
/// being run is echoed at the beginning.
fn bounded(bytes: &[u8]) -> String {
    const KEEP: usize = 512 * 1024;
    // Anything that fits in both halves is returned whole — no truncation, and
    // no risk of the two halves overlapping.
    let Some(skipped) = bytes
        .len()
        .checked_sub(KEEP.saturating_mul(2))
        .filter(|&s| s > 0)
    else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    // Both ends are then taken fallibly, sharing that same fallback. The check
    // above rules the failure out already, but this is output chosen by the code
    // the boundary exists to contain, so it does not get to decide whether senv
    // panics.
    let (Some(head), Some(tail)) = (
        bytes.get(..KEEP),
        bytes.get(bytes.len().saturating_sub(KEEP)..),
    ) else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    let head = String::from_utf8_lossy(head).into_owned();
    let tail = String::from_utf8_lossy(tail).into_owned();
    format!("{head}\n… senv omitted {skipped} bytes of output …\n{tail}")
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
    /// Tells the reader to stop even if the pipe still has writers.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
        // Close-on-exec on both ends. The confined child inherits every
        // descriptor that is not, and it was inheriting the *read* end of this
        // pipe as fd 3 — handing sandboxed code the stream senv is mirroring,
        // and letting a child that simply read from it keep the pipe alive so
        // senv blocked forever waiting for EOF. `dup2` clears the flag on fd 2
        // itself, which is the one the child is meant to have.
        //
        // `pipe2` is Linux-only; macOS has to set the flag afterwards. That
        // leaves a window in which another thread could fork and inherit the
        // descriptors, which is exactly why `pipe2` exists — but senv has no
        // other thread running here (the reader below is spawned after, and the
        // child is spawned after that), so the window is empty.
        // SAFETY: `fds` is a two-element array, which is what pipe(2) writes.
        let created = unsafe {
            #[cfg(target_os = "linux")]
            {
                libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) == 0
            }
            #[cfg(not(target_os = "linux"))]
            {
                libc::pipe(fds.as_mut_ptr()) == 0
                    && libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC) != -1
                    && libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC) != -1
            }
        };
        if !created {
            return None;
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // `F_DUPFD_CLOEXEC` rather than `dup`, for the same reason: the saved
        // copy of the real stderr is senv's, not the child's.
        // SAFETY: duplicating fd 2; the result is checked below.
        let saved = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
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

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_stop = std::sync::Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            let mut collected = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                // Poll rather than block, so EOF is not the only way out. On
                // Linux the child is pid 1 of a namespace and its tree dies
                // with it, but macOS has no such namespace: a daemonized
                // grandchild holding fd 2 would keep the pipe open and senv
                // would wait in `join()` forever, after the command it ran had
                // already finished and printed.
                let mut fd = libc::pollfd {
                    fd: read_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: a single valid pollfd, with a millisecond timeout.
                let ready = unsafe { libc::poll(&mut fd, 1, 100) };
                if ready == 0 {
                    if reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    continue;
                }
                if ready < 0 {
                    break;
                }
                // SAFETY: reading into a stack buffer of known length.
                let n = unsafe {
                    libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n <= 0 {
                    break;
                }
                // `read` returns at most `buf.len()`, so this slice is the whole
                // of what was filled. Fallible anyway: a short read is data, a
                // panic in the tee thread would take the receipt with it.
                let n = n as usize;
                let Some(filled) = buf.get(..n) else { break };
                // Write through to the terminal first: the user's output must
                // never be delayed or dropped by senv's bookkeeping.
                let mut rest = filled;
                while !rest.is_empty() {
                    // SAFETY: writing a slice of `buf` that was just filled.
                    let w = unsafe {
                        libc::write(saved, rest.as_ptr() as *const libc::c_void, rest.len())
                    };
                    let Ok(w) = usize::try_from(w) else { break };
                    let Some(remaining) = rest.get(w..) else {
                        break;
                    };
                    if w == 0 {
                        break;
                    }
                    rest = remaining;
                }
                // A ring, not a prefix. Keeping the first 256 KB meant a
                // denial at the end of a noisy test run was never recorded —
                // and the end is exactly where a command explains why it
                // failed.
                collected.extend_from_slice(filled);
                if let Some(drop) = collected.len().checked_sub(MAX_OBSERVED) {
                    collected.drain(..drop);
                }
            }
            unsafe { libc::close(read_fd) };
            String::from_utf8_lossy(&collected).into_owned()
        });
        Some(StderrTee {
            saved,
            reader,
            stop,
        })
    }

    /// Restore the real stderr and collect what went past.
    fn finish(self) -> String {
        let _ = std::io::stderr().flush();
        // Restoring fd 2 drops senv's reference to the pipe's write end. If the
        // child left no other holder the reader sees EOF immediately; the stop
        // flag covers the case where something outlived it.
        unsafe {
            libc::dup2(self.saved, libc::STDERR_FILENO);
            libc::close(self.saved);
        }
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
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
///
/// It also has to be honest about where it got this. At the kernel tiers there
/// is no egress log, so a denial is *inferred from what the command printed* —
/// and a command chooses what it prints. A package that was refused nothing can
/// write a line shaped like a refusal and have senv answer, in senv's own
/// voice, "senv blocked network access to telemetry.attacker.example — allow it
/// with: senv allow telemetry.attacker.example". Verified. senv cannot tell the
/// two apart, so the only defensible thing is to stop implying that it can: the
/// header says these are read from the command's output, and the trailer says
/// to check them before widening anything.
pub fn print_denials(record: &Record) {
    if record.denials.is_empty() {
        return;
    }
    eprintln!();
    eprintln!(
        "senv read {} refusal(s) from what this command printed:",
        record.denials.len()
    );
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
    eprintln!(
        "  {}",
        wrap(
            "A program chooses what it prints, so check each one is a destination you expect \
             before allowing it. `senv report --suggest` turns everything recorded so far into \
             a policy stanza.",
            74,
            2
        )
    );
}

/// Wrap text to `width`, indenting continuation lines by `indent`.
pub fn wrap(text: &str, width: usize, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut line = 0usize;
    for word in text.split_whitespace() {
        // Saturating throughout: `text` here can be a message senv is quoting
        // from a contained program, so the word lengths are not senv's to
        // bound. A saturated column just forces an early wrap.
        if line > 0 && line.saturating_add(1).saturating_add(word.len()) > width {
            out.push('\n');
            out.push_str(&pad);
            line = 0;
        } else if line > 0 {
            out.push(' ');
            line = line.saturating_add(1);
        }
        out.push_str(word);
        line = line.saturating_add(word.len());
    }
    out
}

/// Replay captured output on the streams it came from.
///
/// Each stream goes back where it belongs, so `senv uv -- export > reqs.txt`
/// produces the requirements and not senv's status line. Not redacted: this is
/// the command's own output going to the person who ran it, exactly as it would
/// appear without senv. Redaction applies to the *receipt*, which is a file
/// that outlives the session.
pub fn emit_output(run: &Execution) {
    write_stream(&mut std::io::stdout(), &run.stdout);
    write_stream(&mut std::io::stderr(), &run.stderr);
}

fn write_stream(sink: &mut impl Write, text: &str) {
    if text.is_empty() {
        return;
    }
    let _ = sink.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = sink.write_all(b"\n");
    }
    let _ = sink.flush();
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
    // Tests assert; an assertion failing *is* a panic, and a test that
    // carefully propagated errors instead would report a pass on a broken
    // invariant. The panic discipline in `Cargo.toml` is about `senv` the
    // process, not about the suite that interrogates it.
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects
    )]
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
