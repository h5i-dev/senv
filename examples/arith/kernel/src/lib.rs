//! Arith-v0: the kernel. Every decision the environment makes, in the subset
//! of Rust that Charon and Aeneas translate to Lean (no `?`, no closures, no
//! `String`, no macros). `h5i app extract` turns it into `proofs/generated/`,
//! and the theorems in `proofs/` are about this code.
//!
//! The task: evaluate a left-nested expression such as `((6 * 8) * 9)`. The
//! agent may ask the calculator for one operation at a time and must finish
//! with an answer. The snapshot keeps the problem, not its value; the value is
//! computed only when an answer is scored.

// Range methods, iterator adapters and closures are outside the Rust that
// Charon and Aeneas extract, so the kernel spells them out.
#![allow(
    clippy::manual_range_contains,
    clippy::needless_range_loop,
    clippy::single_match
)]

/// Steps per episode. The step that reaches it ends the episode.
pub const MAX_STEPS: u8 = 8;
/// Calculator operands beyond ±10^9 are refused, so every product fits in i64.
pub const MAX_OPERAND: i64 = 1_000_000_000;
pub const MIN_OPERAND: i64 = -1_000_000_000;
/// Reward for a correct answer, in thousandths.
pub const WIN_MILLI: u64 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
}

/// `((d0 o0 d1) o1 d2) ...` with `n_ops` operators. Digits are 1..=9.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Problem {
    pub digits: [i64; 5],
    pub ops: [Op; 4],
    pub n_ops: u8,
}

/// What the agent did. The shell parses text into this; anything it cannot
/// parse arrives as `Invalid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Calc { left: i64, op: Op, right: i64 },
    Answer { value: i64 },
    Invalid,
}

/// The environment's reply to the last action. It never carries the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    Start,
    Computed { value: i64 },
    Overflow,
    Correct,
    Incorrect,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub problem: Problem,
    pub steps: u8,
    pub done: bool,
    pub won: bool,
    pub last: Feedback,
    pub reward_milli: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Reset { problem: Problem },
    Step { action: Action },
    State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub reward_milli: u64,
    pub done: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A digit outside 1..=9, or an operator count outside 2..=4.
    InvalidProblem,
    /// The episode has ended.
    Done,
}

pub fn empty_problem() -> Problem {
    Problem {
        digits: [1, 1, 1, 1, 1],
        ops: [Op::Add, Op::Add, Op::Add, Op::Add],
        n_ops: 2,
    }
}

pub fn fresh(problem: Problem) -> Snapshot {
    Snapshot {
        problem,
        steps: 0,
        done: false,
        won: false,
        last: Feedback::Start,
        reward_milli: 0,
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        fresh(empty_problem())
    }
}

fn digit_ok(d: i64) -> bool {
    1 <= d && d <= 9
}

/// The problems `Reset` accepts.
pub fn valid(p: &Problem) -> bool {
    2 <= p.n_ops
        && p.n_ops <= 4
        && digit_ok(p.digits[0])
        && digit_ok(p.digits[1])
        && digit_ok(p.digits[2])
        && digit_ok(p.digits[3])
        && digit_ok(p.digits[4])
}

fn operand_ok(x: i64) -> bool {
    MIN_OPERAND <= x && x <= MAX_OPERAND
}

/// `a op b`, or `None` when an operand is outside ±10^9.
pub fn calc(a: i64, op: Op, b: i64) -> Option<i64> {
    if operand_ok(a) && operand_ok(b) {
        match op {
            Op::Add => Some(a + b),
            Op::Sub => Some(a - b),
            Op::Mul => Some(a * b),
        }
    } else {
        None
    }
}

fn then(acc: Option<i64>, op: Op, b: i64) -> Option<i64> {
    match acc {
        Some(a) => calc(a, op, b),
        None => None,
    }
}

/// The value of the expression, left to right. `None` only for a problem
/// `valid` rejects.
pub fn value(p: &Problem) -> Option<i64> {
    let two = then(
        calc(p.digits[0], p.ops[0], p.digits[1]),
        p.ops[1],
        p.digits[2],
    );
    if p.n_ops == 2 {
        two
    } else {
        let three = then(two, p.ops[2], p.digits[3]);
        if p.n_ops == 3 {
            three
        } else {
            then(three, p.ops[3], p.digits[4])
        }
    }
}

/// Score an answer: `Correct` exactly when it equals the value.
pub fn judge(p: &Problem, answer: i64) -> Feedback {
    match value(p) {
        Some(v) => {
            if answer == v {
                Feedback::Correct
            } else {
                Feedback::Incorrect
            }
        }
        None => Feedback::Incorrect,
    }
}

fn respond(p: &Problem, action: &Action) -> Feedback {
    match action {
        Action::Calc { left, op, right } => match calc(*left, *op, *right) {
            Some(v) => Feedback::Computed { value: v },
            None => Feedback::Overflow,
        },
        Action::Answer { value } => judge(p, *value),
        Action::Invalid => Feedback::Invalid,
    }
}

/// The step that reaches `MAX_STEPS` is the last.
fn at_limit(steps: u8) -> bool {
    steps >= MAX_STEPS
}

fn is_answer(action: &Action) -> bool {
    match action {
        Action::Answer { .. } => true,
        Action::Calc { .. } => false,
        Action::Invalid => false,
    }
}

/// One command: the snapshot to commit (`None` writes nothing) and the reply,
/// or a refusal that commits nothing.
pub fn transition(
    _actor: &(),
    s: &Snapshot,
    cmd: &Command,
) -> Result<(Option<Snapshot>, Reply), Error> {
    match cmd {
        Command::Reset { problem } => {
            if valid(problem) {
                Ok((
                    Some(fresh(*problem)),
                    Reply {
                        reward_milli: 0,
                        done: false,
                    },
                ))
            } else {
                Err(Error::InvalidProblem)
            }
        }
        Command::Step { action } => {
            if s.done || s.steps >= MAX_STEPS {
                Err(Error::Done)
            } else {
                let steps = s.steps + 1;
                let last = respond(&s.problem, action);
                let won = last == Feedback::Correct;
                let done = is_answer(action) || at_limit(steps);
                let reward_milli = if won { WIN_MILLI } else { 0 };
                let next = Snapshot {
                    problem: s.problem,
                    steps,
                    done,
                    won,
                    last,
                    reward_milli,
                };
                Ok((Some(next), Reply { reward_milli, done }))
            }
        }
        Command::State => Ok((
            None,
            Reply {
                reward_milli: s.reward_milli,
                done: s.done,
            },
        )),
    }
}

/// Commit a write set: the next snapshot, or no change.
pub fn apply(s: &Snapshot, w: &Option<Snapshot>) -> Snapshot {
    match w {
        Some(next) => *next,
        None => *s,
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

    fn problem(digits: [i64; 5], ops: [Op; 4], n_ops: u8) -> Problem {
        Problem { digits, ops, n_ops }
    }

    fn step(s: &Snapshot, action: Action) -> Result<(Snapshot, Reply), Error> {
        transition(&(), s, &Command::Step { action }).map(|(w, r)| (apply(s, &w), r))
    }

    #[test]
    fn value_is_left_to_right() {
        use Op::*;
        assert_eq!(
            value(&problem([6, 8, 9, 1, 1], [Mul, Mul, Add, Add], 2)),
            Some(432)
        );
        assert_eq!(
            value(&problem([8, 3, 2, 5, 1], [Sub, Mul, Add, Add], 3)),
            Some(15)
        );
        assert_eq!(
            value(&problem([1, 6, 8, 2, 3], [Sub, Sub, Mul, Sub], 4)),
            Some(-29)
        );
    }

    #[test]
    fn reset_refuses_problems_outside_the_task() {
        let bad_digit = problem([0, 1, 1, 1, 1], [Op::Add; 4], 2);
        let bad_count = problem([1; 5], [Op::Add; 4], 5);
        for p in [bad_digit, bad_count] {
            assert_eq!(
                transition(&(), &Snapshot::default(), &Command::Reset { problem: p }),
                Err(Error::InvalidProblem)
            );
        }
    }

    #[test]
    fn only_the_value_scores() {
        let p = problem([6, 8, 9, 1, 1], [Op::Mul; 4], 2);
        let s = fresh(p);
        let (won, reply) = step(&s, Action::Answer { value: 432 }).unwrap();
        assert_eq!(
            reply,
            Reply {
                reward_milli: 1000,
                done: true
            }
        );
        assert!(won.won);
        let (lost, reply) = step(&s, Action::Answer { value: 431 }).unwrap();
        assert_eq!(
            reply,
            Reply {
                reward_milli: 0,
                done: true
            }
        );
        assert_eq!(lost.last, Feedback::Incorrect);
        assert_eq!(step(&won, Action::Invalid), Err(Error::Done));
    }

    #[test]
    fn calc_answers_one_operation_and_refuses_huge_operands() {
        let s = fresh(problem([6, 8, 9, 1, 1], [Op::Mul; 4], 2));
        let (s, reply) = step(
            &s,
            Action::Calc {
                left: 6,
                op: Op::Mul,
                right: 8,
            },
        )
        .unwrap();
        assert_eq!(s.last, Feedback::Computed { value: 48 });
        assert_eq!(
            reply,
            Reply {
                reward_milli: 0,
                done: false
            }
        );
        let (s, _) = step(
            &s,
            Action::Calc {
                left: MAX_OPERAND + 1,
                op: Op::Add,
                right: 1,
            },
        )
        .unwrap();
        assert_eq!(s.last, Feedback::Overflow);
        let (s, _) = step(
            &s,
            Action::Calc {
                left: MAX_OPERAND,
                op: Op::Mul,
                right: MIN_OPERAND,
            },
        )
        .unwrap();
        assert_eq!(
            s.last,
            Feedback::Computed {
                value: -1_000_000_000_000_000_000
            }
        );
    }

    #[test]
    fn the_eighth_step_ends_the_episode() {
        let mut s = fresh(problem([1; 5], [Op::Add; 4], 2));
        for i in 1..=MAX_STEPS {
            let (next, reply) = step(&s, Action::Invalid).unwrap();
            assert_eq!(next.steps, i);
            assert_eq!(reply.done, i == MAX_STEPS);
            assert_eq!(reply.reward_milli, 0);
            s = next;
        }
        assert_eq!(step(&s, Action::Answer { value: 3 }), Err(Error::Done));
    }
}
