//! Small shared helpers: hashing, time formatting, terminal niceties.
//!
//! Deliberately dependency-light. A UTC formatter and a spinner are not worth
//! pulling `chrono` and `indicatif` into a security tool's dependency tree —
//! senv's supply chain is part of its argument.

use sha2::{Digest, Sha256};
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Full lowercase hex sha256 of some bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// sha256 of a file's contents, or `None` if it cannot be read.
pub fn sha256_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| sha256_hex(&b))
}

/// Milliseconds since the Unix epoch. Saturates rather than panicking on a
/// clock before 1970 — a receipt with a wrong timestamp beats a crash.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Format epoch milliseconds as RFC 3339 UTC (`2026-08-13T21:04:05Z`).
///
/// Uses Howard Hinnant's `civil_from_days` algorithm, which is exact for all
/// dates we care about and is ~15 lines. Receipts are read by people; an
/// integer would make them unreadable and a date crate would make senv's own
/// dependency tree harder to justify.
pub fn format_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Human-readable byte size, for `senv gc` and `senv status`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Recursive size of a directory tree, following no symlinks. Best-effort:
/// unreadable entries contribute zero rather than failing the whole walk, since
/// this only ever feeds a human-readable number.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(md) = entry.metadata() else { continue };
        if md.is_dir() {
            total += dir_size(&entry.path());
        } else if md.is_file() {
            total += md.len();
        }
    }
    total
}

/// Is stderr a terminal? Progress and colour are suppressed when it is not, so
/// piped output and CI logs stay clean.
pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// A minimal elapsed-time indicator for the install phase.
///
/// The install phase captures its child's output (senv needs the full text for
/// receipts and for `senv report --suggest`), which means a slow `uv sync`
/// would otherwise print nothing at all until it finished. This prints a single
/// rewriting line to stderr so the user can tell the difference between "slow"
/// and "hung". Silent when stderr is not a terminal.
pub struct Progress {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Progress {
    pub fn start(label: &str) -> Progress {
        let stop = Arc::new(AtomicBool::new(false));
        if !stderr_is_tty() {
            return Progress { stop, handle: None };
        }
        let flag = Arc::clone(&stop);
        let label = label.to_string();
        let handle = std::thread::spawn(move || {
            const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let start = std::time::Instant::now();
            let mut i = 0usize;
            while !flag.load(Ordering::Relaxed) {
                eprint!(
                    "\r{} {label} ({:.0}s)\x1b[K",
                    FRAMES[i % FRAMES.len()],
                    start.elapsed().as_secs_f32()
                );
                let _ = std::io::Write::flush(&mut std::io::stderr());
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            eprint!("\r\x1b[K");
            let _ = std::io::Write::flush(&mut std::io::stderr());
        });
        Progress {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Replace a leading `~` with `$HOME`, matching how h5i expands grant paths.
/// Anything else is returned unchanged.
pub fn expand_tilde(path: &str) -> String {
    expand_tilde_in(path, std::env::var("HOME").ok().as_deref())
}

/// [`expand_tilde`] with the home directory supplied — the pure half.
///
/// Separate so it can be tested without `set_var`: cargo runs tests as threads
/// of one process, so a test that reassigns `HOME` reassigns it for every test
/// running beside it, including ones resolving real paths under the real home.
fn expand_tilde_in(path: &str, home: Option<&str>) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Some(home) = home.filter(|h| !h.is_empty())
    {
        return format!("{home}{}", &path[1..]);
    }
    path.to_string()
}

/// Strip terminal control sequences from text that came from somewhere
/// untrusted.
///
/// senv frames program output inside its own messages ("senv blocked access to
/// X"), and a program chooses what it prints. Without this, a package can emit
/// ANSI escapes that repaint or erase senv's lines — turning a refusal into
/// something that reads like an approval — and BEL/bidi characters that garble
/// the rest of the session. The command's *own* output is passed through
/// untouched, exactly as it would appear without senv; this is only for
/// fragments senv quotes as if senv were saying them.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // ESC: drop the whole sequence, not just the escape byte, or the
            // payload (`[31m`) would survive as visible garbage.
            '\u{1b}' => {
                match chars.peek() {
                    // CSI: parameters and intermediates, then a final byte in
                    // 0x40..=0x7e.
                    Some('[') => {
                        chars.next();
                        for f in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&f) {
                                break;
                            }
                        }
                    }
                    // OSC: runs until BEL or ST (ESC \).
                    Some(']') => {
                        chars.next();
                        while let Some(f) = chars.next() {
                            if f == '\u{7}' {
                                break;
                            }
                            if f == '\u{1b}' && chars.peek() == Some(&'\\') {
                                chars.next();
                                break;
                            }
                        }
                    }
                    // Any other two-character escape.
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            // Other C0 controls and DEL: a lone \r rewrites the line a caller
            // already printed, and \n would forge a new message.
            c if (c < '\u{20}' && c != '\t') || c == '\u{7f}' => {}
            // Bidi overrides, which can visually reverse a path or a hostname.
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {}
            c => out.push(c),
        }
    }
    out
}

/// [`sanitize`], but keeping line breaks.
///
/// For untrusted text senv renders as a block (a parser's error with its caret
/// line, say) rather than as a fragment inside one of its own sentences. Line
/// breaks survive; escape sequences and every other control character do not.
pub fn sanitize_multiline(text: &str) -> String {
    text.lines().map(sanitize).collect::<Vec<_>>().join("\n")
}

/// The last `n` bytes of `text`, on a character boundary, prefixed with an
/// ellipsis when truncated. Used to bound what a receipt stores from a failing
/// command's output.
pub fn tail(text: &str, n: usize) -> String {
    if text.len() <= n {
        return text.to_string();
    }
    let mut start = text.len() - n;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting_matches_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        // 2026-08-13T00:00:00Z
        assert_eq!(format_utc(1_786_579_200_000), "2026-08-13T00:00:00Z");
        // A leap day, the case a hand-rolled calendar gets wrong.
        assert_eq!(format_utc(1_709_164_800_000), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn byte_sizes_read_the_way_people_expect() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn tail_never_splits_a_character() {
        let s = "αβγδεζηθ";
        let t = tail(s, 5);
        assert!(t.starts_with('…'));
        // The point of the boundary walk: this must not panic and must stay
        // valid UTF-8 (guaranteed by String's type, so reaching here is the
        // assertion).
        assert!(t.len() <= s.len() + 3);
    }

    #[test]
    fn terminal_escapes_cannot_be_smuggled_through_senvs_own_messages() {
        // The attack: a package prints escapes that repaint senv's framing so
        // a refusal reads as an approval.
        let hostile = "\u{1b}[2K\rPermission denied: /home/u/\u{1b}[32mSAFE\u{1b}[0m\u{7}";
        let clean = sanitize(hostile);
        assert_eq!(clean, "Permission denied: /home/u/SAFE", "{clean:?}");
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\r'));

        // OSC (window title / hyperlink) runs to BEL or ST.
        assert_eq!(sanitize("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(sanitize("a\u{1b}]8;;http://evil\u{1b}\\b"), "ab");

        // A newline would let output forge an extra senv line.
        assert_eq!(sanitize("one\ntwo"), "onetwo");

        // Bidi overrides can visually reverse a hostname.
        assert_eq!(sanitize("evil\u{202e}moc.elpmaxe"), "evilmoc.elpmaxe");

        // Ordinary text, including non-ASCII and tabs, is untouched.
        assert_eq!(
            sanitize("ドメイン\tok-1.example.com"),
            "ドメイン\tok-1.example.com"
        );
    }

    #[test]
    fn tilde_expands_only_at_the_front() {
        let home = Some("/home/example");
        assert_eq!(expand_tilde_in("~/x", home), "/home/example/x");
        assert_eq!(expand_tilde_in("~", home), "/home/example");
        assert_eq!(expand_tilde_in("/a/~/b", home), "/a/~/b");
        assert_eq!(expand_tilde_in("~notauser/x", home), "~notauser/x");
        // No home to expand against: better a literal `~` than a bare relative
        // path silently becoming a grant.
        assert_eq!(expand_tilde_in("~/x", None), "~/x");
        assert_eq!(expand_tilde_in("~/x", Some("")), "~/x");
    }
}
