//! Wordle-v0: the kernel. Marking, rewards, and termination, in the subset of
//! Rust that Charon and Aeneas translate to Lean (no `?`, no closures, no
//! iterators, no `String`; loops over indices). `h5i app extract` turns it
//! into `proofs/generated/`, and the theorems in `proofs/` are about this code.
//!
//! Letters are `0..26` for `a..z`. Marks follow the TextArena Wordle rule:
//! greens consume a secret letter, then a later yellow consumes one remaining
//! copy, otherwise the mark is wrong. The shell picks the secret, looks words
//! up in the dictionary, and renders text; none of that is here.

// Range methods, iterator adapters and closures are outside the Rust that
// Charon and Aeneas extract, so the kernel spells them out.
#![allow(clippy::manual_range_contains, clippy::needless_range_loop)]

pub const WORD_LEN: usize = 5;
pub const MAX_GUESSES: u8 = 6;
/// Matches TextArena `SinglePlayerState`: the first invalid move is returned
/// to the player, and the next consecutive invalid move ends the episode.
pub const ERROR_ALLOWANCE: u8 = 1;
/// Reward for guessing the word, in thousandths.
pub const WIN_MILLI: i64 = 1000;
/// A secret letter already matched. Letters are below 26, so no guess equals it.
const CONSUMED: u8 = 0xFF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Green,
    Yellow,
    Wrong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Format,
    Length,
    Repeat,
    NotEnglish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// Five letters. `in_dictionary` is decided by the shell.
    Candidate {
        letters: [u8; WORD_LEN],
        in_dictionary: bool,
    },
    Malformed {
        reason: Reason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayOutcome {
    Ongoing,
    Win,
    Loss { reward_milli: i64 },
}

/// What a step did, for the shell's transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Retry {
        reason: Reason,
    },
    InvalidEnd {
        reason: Reason,
        reward_milli: i64,
    },
    Played {
        letters: [u8; WORD_LEN],
        marks: [Mark; WORD_LEN],
        guesses_left: u8,
        outcome: PlayOutcome,
    },
}

/// One game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Core {
    pub secret: [u8; WORD_LEN],
    pub guesses: [[u8; WORD_LEN]; 6],
    pub guess_count: u8,
    pub error_count: u8,
    pub turn: u8,
    pub done: bool,
    pub won: bool,
    pub reward_milli: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Reset { secret: [u8; WORD_LEN] },
    Step { mv: Move },
    State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub reward_milli: i64,
    pub done: bool,
    /// `None` for `Reset` and `State`.
    pub phase: Option<Phase>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The game has ended.
    Done,
    /// A secret with a letter outside `0..26`.
    InvalidSecret,
}

pub fn fresh(secret: [u8; WORD_LEN]) -> Core {
    Core {
        secret,
        guesses: [[0; WORD_LEN]; 6],
        guess_count: 0,
        error_count: 0,
        turn: 0,
        done: false,
        won: false,
        reward_milli: 0,
    }
}

impl Default for Core {
    fn default() -> Self {
        fresh([0; WORD_LEN])
    }
}

/// Two passes: greens first, each consuming its secret letter; then each
/// non-green letter takes the first remaining copy as a yellow.
pub fn mark(secret: &[u8; WORD_LEN], guess: &[u8; WORD_LEN]) -> [Mark; WORD_LEN] {
    let mut feedback = [Mark::Wrong; WORD_LEN];
    let mut remaining = *secret;
    let mut i = 0;
    while i < WORD_LEN {
        if guess[i] == remaining[i] {
            feedback[i] = Mark::Green;
            remaining[i] = CONSUMED;
        }
        i += 1;
    }
    let mut i = 0;
    while i < WORD_LEN {
        if feedback[i] != Mark::Green {
            let mut j = 0;
            let mut found = false;
            while j < WORD_LEN && !found {
                if remaining[j] == guess[i] {
                    feedback[i] = Mark::Yellow;
                    remaining[j] = CONSUMED;
                    found = true;
                }
                j += 1;
            }
        }
        i += 1;
    }
    feedback
}

/// `(greens + 0.5 * yellows) / 5` in thousandths.
pub fn completion_milli(marks: &[Mark; WORD_LEN]) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < WORD_LEN {
        match marks[i] {
            Mark::Green => total += 200,
            Mark::Yellow => total += 100,
            Mark::Wrong => {}
        }
        i += 1;
    }
    total
}

pub fn all_green(marks: &[Mark; WORD_LEN]) -> bool {
    let mut i = 0;
    while i < WORD_LEN {
        if marks[i] != Mark::Green {
            return false;
        }
        i += 1;
    }
    true
}

/// Every letter is in `0..26`.
pub fn is_word(letters: &[u8; WORD_LEN]) -> bool {
    let mut i = 0;
    while i < WORD_LEN {
        if letters[i] >= 26 {
            return false;
        }
        i += 1;
    }
    true
}

fn already_guessed(core: &Core, letters: &[u8; WORD_LEN]) -> bool {
    let mut i = 0;
    while i < core.guess_count as usize && i < 6 {
        if core.guesses[i] == *letters {
            return true;
        }
        i += 1;
    }
    false
}

/// `None` for a playable guess, or why the move is invalid. A repeat is
/// checked before the dictionary flag.
fn invalid_reason(core: &Core, mv: &Move) -> Option<Reason> {
    match mv {
        Move::Malformed { reason } => Some(*reason),
        Move::Candidate {
            letters,
            in_dictionary,
        } => {
            if !is_word(letters) {
                Some(Reason::NotEnglish)
            } else if already_guessed(core, letters) {
                Some(Reason::Repeat)
            } else if !*in_dictionary {
                Some(Reason::NotEnglish)
            } else {
                None
            }
        }
    }
}

/// The completion of the last guess, or 0 before any guess.
fn latest_completion(core: &Core) -> i64 {
    if core.guess_count == 0 || core.guess_count > 6 {
        0
    } else {
        let last = core.guesses[(core.guess_count - 1) as usize];
        completion_milli(&mark(&core.secret, &last))
    }
}

fn letters_of(mv: &Move) -> [u8; WORD_LEN] {
    match mv {
        Move::Candidate { letters, .. } => *letters,
        Move::Malformed { .. } => [0; WORD_LEN],
    }
}

/// One move on an unfinished game.
pub fn step_core(core: &Core, mv: &Move) -> Result<(Core, Phase), Error> {
    if core.done || core.guess_count >= MAX_GUESSES || core.turn >= MAX_GUESSES {
        return Err(Error::Done);
    }
    let mut next = *core;
    match invalid_reason(core, mv) {
        Some(reason) => {
            if core.error_count < ERROR_ALLOWANCE {
                next.error_count = core.error_count + 1;
                next.reward_milli = 0;
                Ok((next, Phase::Retry { reason }))
            } else {
                let reward_milli = latest_completion(core);
                next.done = true;
                next.won = false;
                next.reward_milli = reward_milli;
                Ok((
                    next,
                    Phase::InvalidEnd {
                        reason,
                        reward_milli,
                    },
                ))
            }
        }
        None => {
            let letters = letters_of(mv);
            let marks = mark(&core.secret, &letters);
            let guesses_left = MAX_GUESSES - core.turn - 1;
            next.guesses[core.guess_count as usize] = letters;
            next.guess_count = core.guess_count + 1;
            next.error_count = 0;
            next.turn = core.turn + 1;
            let outcome = if all_green(&marks) {
                next.done = true;
                next.won = true;
                next.reward_milli = WIN_MILLI;
                PlayOutcome::Win
            } else if next.guess_count >= MAX_GUESSES {
                let reward_milli = completion_milli(&marks);
                next.done = true;
                next.won = false;
                next.reward_milli = reward_milli;
                PlayOutcome::Loss { reward_milli }
            } else {
                next.reward_milli = 0;
                PlayOutcome::Ongoing
            };
            Ok((
                next,
                Phase::Played {
                    letters,
                    marks,
                    guesses_left,
                    outcome,
                },
            ))
        }
    }
}

/// One command: the game to commit (`None` writes nothing) and the reply.
pub fn transition(_actor: &(), core: &Core, cmd: &Command) -> Result<(Option<Core>, Reply), Error> {
    match cmd {
        Command::Reset { secret } => {
            if is_word(secret) {
                Ok((
                    Some(fresh(*secret)),
                    Reply {
                        reward_milli: 0,
                        done: false,
                        phase: None,
                    },
                ))
            } else {
                Err(Error::InvalidSecret)
            }
        }
        Command::Step { mv } => match step_core(core, mv) {
            Ok((next, phase)) => Ok((
                Some(next),
                Reply {
                    reward_milli: next.reward_milli,
                    done: next.done,
                    phase: Some(phase),
                },
            )),
            Err(e) => Err(e),
        },
        Command::State => Ok((
            None,
            Reply {
                reward_milli: core.reward_milli,
                done: core.done,
                phase: None,
            },
        )),
    }
}

pub fn apply(core: &Core, w: &Option<Core>) -> Core {
    match w {
        Some(next) => *next,
        None => *core,
    }
}

/// Only `State` reads without writing.
pub fn read_only(cmd: &Command) -> bool {
    match cmd {
        Command::State => true,
        Command::Reset { .. } => false,
        Command::Step { .. } => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn letters(word: &str) -> [u8; 5] {
        let mut out = [0u8; 5];
        for (i, byte) in word.bytes().enumerate() {
            out[i] = byte - b'a';
        }
        out
    }

    fn play(core: &Core, word: &str) -> (Core, Phase) {
        let mv = Move::Candidate {
            letters: letters(word),
            in_dictionary: true,
        };
        step_core(core, &mv).unwrap()
    }

    #[test]
    fn greens_consume_before_yellows() {
        let marks = mark(&letters("crane"), &letters("cigar"));
        assert_eq!(
            marks,
            [
                Mark::Green,
                Mark::Wrong,
                Mark::Wrong,
                Mark::Yellow,
                Mark::Yellow
            ]
        );
        assert_eq!(completion_milli(&marks), 400);
        let doubled = mark(&letters("alley"), &letters("llama"));
        assert_eq!(
            doubled,
            [
                Mark::Yellow,
                Mark::Green,
                Mark::Yellow,
                Mark::Wrong,
                Mark::Wrong
            ]
        );
    }

    #[test]
    fn reset_shape_and_win_and_six_failures() {
        let secret = letters("apple");
        let core = fresh(secret);
        assert_eq!(core.guess_count, 0);
        assert!(!core.done);
        assert_eq!(core.reward_milli, 0);
        assert_eq!(core.turn, 0);

        let (won, phase) = play(&core, "apple");
        assert!(won.done && won.won);
        assert_eq!(won.reward_milli, 1000);
        assert_eq!(won.turn, 1);
        assert_eq!(won.guess_count, 1);
        match phase {
            Phase::Played {
                guesses_left,
                outcome: PlayOutcome::Win,
                ..
            } => assert_eq!(guesses_left, 5),
            other => panic!("expected a win, got {other:?}"),
        }
        assert!(
            step_core(
                &won,
                &Move::Malformed {
                    reason: Reason::Format
                }
            )
            .is_err()
        );

        let mut cursor = fresh(secret);
        let words = ["crane", "stone", "house", "plant", "query", "bread"];
        for (n, word) in words.iter().enumerate() {
            let (next, phase) = play(&cursor, word);
            if n < 5 {
                assert!(!next.done, "{word}");
                assert_eq!(next.reward_milli, 0);
                match phase {
                    Phase::Played {
                        outcome: PlayOutcome::Ongoing,
                        guesses_left,
                        ..
                    } => assert_eq!(guesses_left, 5 - u8::try_from(n).unwrap()),
                    other => panic!("{other:?}"),
                }
            } else {
                assert!(next.done && !next.won);
                assert_eq!(next.turn, 6);
                match phase {
                    Phase::Played {
                        outcome: PlayOutcome::Loss { reward_milli },
                        ..
                    } => assert_eq!(next.reward_milli, reward_milli),
                    other => panic!("{other:?}"),
                }
            }
            cursor = next;
        }
    }

    #[test]
    fn first_invalid_does_not_consume_a_guess() {
        let core = fresh(letters("apple"));
        let (once, phase) = step_core(
            &core,
            &Move::Malformed {
                reason: Reason::Format,
            },
        )
        .unwrap();
        assert!(!once.done);
        assert_eq!(once.turn, 0);
        assert_eq!(once.guess_count, 0);
        assert_eq!(once.error_count, 1);
        assert_eq!(once.reward_milli, 0);
        assert_eq!(
            phase,
            Phase::Retry {
                reason: Reason::Format
            }
        );

        let (played, _) = play(&once, "crane");
        assert_eq!(played.turn, 1);
        assert_eq!(played.error_count, 0);
        assert!(!played.done);

        let (bad, _) = step_core(
            &played,
            &Move::Malformed {
                reason: Reason::Format,
            },
        )
        .unwrap();
        assert!(!bad.done);
        assert_eq!(bad.turn, 1);
        let (ended, phase) = step_core(
            &bad,
            &Move::Candidate {
                letters: letters("zzzzz"),
                in_dictionary: false,
            },
        )
        .unwrap();
        assert!(ended.done && !ended.won);
        assert_eq!(ended.turn, 1);
        assert_eq!(ended.guess_count, 1);
        match phase {
            Phase::InvalidEnd { reward_milli, .. } => {
                assert_eq!(
                    reward_milli,
                    completion_milli(&mark(&letters("apple"), &letters("crane")))
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn second_invalid_with_no_guess_scores_zero() {
        let core = fresh(letters("apple"));
        let (once, _) = step_core(
            &core,
            &Move::Malformed {
                reason: Reason::Length,
            },
        )
        .unwrap();
        let (twice, phase) = step_core(
            &once,
            &Move::Malformed {
                reason: Reason::Format,
            },
        )
        .unwrap();
        assert!(twice.done);
        assert_eq!(twice.reward_milli, 0);
        assert_eq!(
            phase,
            Phase::InvalidEnd {
                reason: Reason::Format,
                reward_milli: 0
            }
        );
    }

    #[test]
    fn repeat_is_checked_before_the_dictionary_flag() {
        let (core, _) = play(&fresh(letters("apple")), "crane");
        let (next, phase) = step_core(
            &core,
            &Move::Candidate {
                letters: letters("crane"),
                in_dictionary: false,
            },
        )
        .unwrap();
        assert!(!next.done);
        assert_eq!(next.guess_count, 1);
        assert_eq!(
            phase,
            Phase::Retry {
                reason: Reason::Repeat
            }
        );
    }
}
