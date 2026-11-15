//! Wordle text: the dictionary, secrets from seeds, parsing the agent's message,
//! and the TextArena-shaped transcript. All of it is trusted shell code; the
//! decisions it reports come from `wordle-kernel`.

use std::sync::LazyLock;

use wordle_kernel::{Mark, Move, Phase, PlayOutcome, Reason, WORD_LEN};

pub const ENV_ID: &str = "Wordle-v0";
pub const MAX_ACTION_BYTES: usize = 4096;

/// Prompt text from TextArena Wordle-v0 for a 5-letter word and 6 guesses,
/// including the trailing newline that environment stores on the message.
pub const PROMPT: &str = "\
You are Playing Wordle.\n\
A secret 5-letter word has been chosen. You have 6 attempts to guess it.\n\
For each guess, wrap your word in square brackets (e.g., '[apple]').\n\
Feedback for each letter will be given as follows:\n\
  - G (green): correct letter in the correct position\n\
  - Y (yellow): letter exists in the word but in the wrong position\n\
  - X (wrong): letter is not in the word\n\
Enter your guess to begin.\n";

pub fn mark_char(mark: Mark) -> char {
    match mark {
        Mark::Green => 'G',
        Mark::Yellow => 'Y',
        Mark::Wrong => 'X',
    }
}

pub fn letters_from_ascii(word: &str) -> Option<[u8; WORD_LEN]> {
    let bytes = word.as_bytes();
    if bytes.len() != WORD_LEN {
        return None;
    }
    let mut out = [0u8; WORD_LEN];
    for (i, byte) in bytes.iter().enumerate() {
        if !byte.is_ascii_lowercase() {
            return None;
        }
        out[i] = byte - b'a';
    }
    Some(out)
}

pub fn ascii_word(letters: &[u8; WORD_LEN]) -> String {
    letters
        .iter()
        .map(|letter| char::from(b'a'.saturating_add(*letter)))
        .collect()
}

fn words() -> &'static [&'static str] {
    static WORDS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
        include_str!("../words.txt")
            .split('\n')
            .filter(|line| !line.is_empty())
            .collect()
    });
    WORDS.as_slice()
}

pub fn word_count() -> usize {
    words().len()
}

pub fn in_dictionary(word: &str) -> bool {
    words().binary_search(&word).is_ok()
}

/// Index into the bundled list. `seed as u64` is the two's-complement bit
/// pattern, then modulo the list length. This is not CPython's RNG.
pub fn secret_from_seed(seed: i64) -> [u8; WORD_LEN] {
    secret_at(u64::from_ne_bytes(seed.to_ne_bytes()))
}

/// FNV-1a 64 of the episode id. Stable for one id, not a CSPRNG.
pub fn secret_from_episode(episode_id: &str) -> [u8; WORD_LEN] {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in episode_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    secret_at(hash)
}

fn secret_at(index: u64) -> [u8; WORD_LEN] {
    let list = words();
    if list.is_empty() {
        return letters_from_ascii("apple").unwrap_or([0; WORD_LEN]);
    }
    let len = u64::try_from(list.len()).unwrap_or(u64::MAX);
    let idx = usize::try_from(index % len).unwrap_or(0);
    letters_from_ascii(list[idx]).unwrap_or([0; WORD_LEN])
}

/// First `\[(\w+)\]` match, lowercased, or a format failure.
pub fn parse_message(message: &str) -> Move {
    let Some(word) = bracket_word(message) else {
        return Move::Malformed {
            reason: Reason::Format,
        };
    };
    if word.len() != WORD_LEN {
        return Move::Malformed {
            reason: Reason::Length,
        };
    }
    match letters_from_ascii(word) {
        Some(letters) => Move::Candidate {
            letters,
            in_dictionary: in_dictionary(word),
        },
        None => Move::Malformed {
            reason: Reason::NotEnglish,
        },
    }
}

fn bracket_word(message: &str) -> Option<&str> {
    let bytes = message.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && is_word_byte(bytes[j]) {
                j += 1;
            }
            if j > start && j < bytes.len() && bytes[j] == b']' {
                return Some(&message[start..j]);
            }
        }
        i += 1;
    }
    None
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogEntry {
    Action(String),
    Feedback {
        letters: [u8; WORD_LEN],
        marks: [Mark; WORD_LEN],
        guesses_left: u8,
    },
    Retry(Reason),
    InvalidEnd(Reason),
    Win,
    Loss,
}

pub fn push_phase(log: &mut Vec<LogEntry>, raw: &str, phase: &Phase) {
    log.push(LogEntry::Action(raw.to_string()));
    match phase {
        Phase::Retry { reason } => log.push(LogEntry::Retry(*reason)),
        Phase::InvalidEnd { reason, .. } => log.push(LogEntry::InvalidEnd(*reason)),
        Phase::Played {
            letters,
            marks,
            guesses_left,
            outcome,
        } => match outcome {
            PlayOutcome::Win => log.push(LogEntry::Win),
            PlayOutcome::Ongoing => log.push(LogEntry::Feedback {
                letters: *letters,
                marks: *marks,
                guesses_left: *guesses_left,
            }),
            PlayOutcome::Loss { .. } => {
                log.push(LogEntry::Feedback {
                    letters: *letters,
                    marks: *marks,
                    guesses_left: *guesses_left,
                });
                log.push(LogEntry::Loss);
            }
        },
    }
}

pub fn prompt_text() -> &'static str {
    PROMPT.trim()
}

pub struct RenderedMessage {
    pub sender_id: i64,
    pub content: String,
    pub category: &'static str,
}

pub fn render_messages(
    secret: &[u8; WORD_LEN],
    reward_milli: i64,
    log: &[LogEntry],
) -> Vec<RenderedMessage> {
    let mut out = vec![RenderedMessage {
        sender_id: -1,
        content: PROMPT.to_string(),
        category: "PROMPT",
    }];
    let mut last_action = String::new();
    for entry in log {
        match entry {
            LogEntry::Action(raw) => {
                last_action.clone_from(raw);
                out.push(RenderedMessage {
                    sender_id: 0,
                    content: raw.clone(),
                    category: "PLAYER_ACTION",
                });
            }
            LogEntry::Feedback {
                letters,
                marks,
                guesses_left,
            } => out.push(RenderedMessage {
                sender_id: -1,
                content: feedback_message(letters, marks, *guesses_left),
                category: "GAME_MESSAGE",
            }),
            LogEntry::Retry(reason) => out.push(RenderedMessage {
                sender_id: -1,
                content: invalid_sentence(&invalid_detail(*reason, &last_action)),
                category: "GAME_ADMIN",
            }),
            LogEntry::InvalidEnd(reason) => {
                let detail = invalid_detail(*reason, &last_action);
                out.push(RenderedMessage {
                    sender_id: -1,
                    content: format!("\n[GAME] Invalid Move: {detail}"),
                    category: "GAME_ADMIN",
                });
            }
            LogEntry::Win => out.push(RenderedMessage {
                sender_id: -1,
                content: "\n[GAME] Congratulations! You guessed the word correctly!".to_string(),
                category: "GAME_ADMIN",
            }),
            LogEntry::Loss => out.push(RenderedMessage {
                sender_id: -1,
                content: format!("\n[GAME] {}", loss_reason(secret, reward_milli)),
                category: "GAME_ADMIN",
            }),
        }
    }
    out
}

fn feedback_message(
    letters: &[u8; WORD_LEN],
    marks: &[Mark; WORD_LEN],
    guesses_left: u8,
) -> String {
    let word = ascii_word(letters);
    format!(
        "You submitted [{word}].\nFeedback:\n{}\n{}\nYou have {guesses_left} guesses left.",
        spaced_upper(&word),
        spaced_marks(marks)
    )
}

fn spaced_upper(word: &str) -> String {
    word.chars()
        .map(|ch| ch.to_ascii_uppercase())
        .map(|ch| ch.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn spaced_marks(marks: &[Mark; WORD_LEN]) -> String {
    marks
        .iter()
        .map(|mark| mark_char(*mark).to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn invalid_detail(reason: Reason, raw: &str) -> String {
    match reason {
        Reason::Format => "You tried submitting a word in the wrong format. Please make sure to use squared brackets.".to_string(),
        Reason::Length => "Your word must be exactly 5 letters.".to_string(),
        Reason::Repeat => {
            let word = bracket_word(raw).unwrap_or("?");
            format!("You have already guessed '{word}' before. Please try a different word.")
        }
        Reason::NotEnglish => {
            let word = bracket_word(raw).unwrap_or("?");
            format!("'{word}' is not an English word.")
        }
    }
}

fn invalid_sentence(detail: &str) -> String {
    format!(
        "You attempted an invalid move. Reason: {detail} Please resubmit a valid move and remember to follow the game rules to avoid penalties."
    )
}

fn loss_reason(secret: &[u8; WORD_LEN], reward_milli: i64) -> String {
    let pct = reward_milli / 10;
    format!(
        "The turn limit has been reached. You didn't guess the word, but your best guess matched {pct}% of the letters in the correct positions.\nThe secret word was: **{}**.",
        ascii_word(secret)
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Signals {
    pub greens: f64,
    pub yellows: f64,
    pub repetitions: f64,
    pub correct: f64,
}

/// OpenEnv `_WordleRewardProvider` for the action most recently appended.
/// Wins have no feedback line, so green and yellow scores are 0 there.
pub fn signals_for_log(log: &[LogEntry], reward_milli: i64) -> Option<Signals> {
    let last_action = log
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| match entry {
            LogEntry::Action(raw) => Some((index, raw.as_str())),
            _ => None,
        })?;
    let (action_index, raw) = last_action;
    let key = extract_guess(raw);
    let record = (key != "[dunno]").then_some(key.as_str());
    let mut previous = 0i64;
    if let Some(record) = record {
        for entry in log.iter().take(action_index) {
            if let LogEntry::Action(earlier) = entry {
                let earlier_key = extract_guess(earlier);
                if earlier_key == record {
                    previous += 1;
                }
            }
        }
    }
    let marks = log
        .iter()
        .skip(action_index + 1)
        .find_map(|entry| match entry {
            LogEntry::Feedback { marks, .. } => Some(*marks),
            _ => None,
        });
    let (greens, yellows) = match marks {
        Some(marks) => {
            let mut g = 0.0;
            let mut y = 0.0;
            for mark in marks {
                match mark {
                    Mark::Green => g += 1.0,
                    Mark::Yellow => y += 1.0,
                    Mark::Wrong => {}
                }
            }
            (g / 5.0, y / 5.0)
        }
        None => (0.0, 0.0),
    };
    Some(Signals {
        greens,
        yellows,
        repetitions: 1.0 - previous as f64,
        correct: reward_milli as f64 / 1000.0,
    })
}

fn extract_guess(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 6 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 6] == b']' {
            let inner = &bytes[i + 1..i + 6];
            if inner.iter().all(|byte| byte.is_ascii_alphabetic()) {
                let mut out = String::from("[");
                for byte in inner {
                    out.push(char::from(byte.to_ascii_lowercase()));
                }
                out.push(']');
                return out;
            }
        }
        i += 1;
    }
    let cleaned: String = text
        .chars()
        .flat_map(|ch| ch.to_lowercase())
        .filter(|ch| ch.is_ascii_lowercase())
        .collect();
    if cleaned.len() >= WORD_LEN {
        format!("[{}]", &cleaned[..WORD_LEN])
    } else {
        "[dunno]".to_string()
    }
}

pub fn marks_code(marks: &[Mark; WORD_LEN]) -> String {
    marks.iter().map(|mark| mark_char(*mark)).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use wordle_kernel::{Core, fresh, step_core};

    fn letters(word: &str) -> [u8; 5] {
        letters_from_ascii(word).unwrap()
    }

    fn play(core: &Core, word: &str) -> (Core, Phase) {
        let mv = Move::Candidate {
            letters: letters(word),
            in_dictionary: true,
        };
        step_core(core, &mv).unwrap()
    }

    #[test]
    fn seed_is_stable_and_not_the_only_word() {
        assert!(word_count() > 1000);
        assert!(in_dictionary("apple"));
        assert!(in_dictionary("crane"));
        assert!(!in_dictionary("qqqqq"));
        let a = secret_from_seed(7);
        assert_eq!(a, secret_from_seed(7));
        assert_ne!(secret_from_seed(7), secret_from_seed(8));
        assert_eq!(secret_from_episode("same"), secret_from_episode("same"));
        let word = ascii_word(&a);
        assert!(in_dictionary(&word), "{word}");
    }

    #[test]
    fn rendered_feedback_matches_the_openenv_regex_shape() {
        let secret = letters("crane");
        let (core, phase) = play(&fresh(secret), "cigar");
        let mut log = Vec::new();
        push_phase(&mut log, "[cigar]", &phase);
        let messages = render_messages(&secret, core.reward_milli, &log);
        let text = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<String>();
        assert!(text.contains("You have 6 attempts"));
        assert!(text.contains(
            "You submitted [cigar].\nFeedback:\nC I G A R\nG X X Y Y\nYou have 5 guesses left."
        ));
        let signals = signals_for_log(&log, core.reward_milli).unwrap();
        assert_eq!(signals.greens, 0.2);
        assert_eq!(signals.yellows, 0.4);
        assert_eq!(signals.repetitions, 1.0);
        assert_eq!(signals.correct, 0.0);
    }
}
