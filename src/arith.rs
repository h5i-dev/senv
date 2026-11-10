//! Custom left-associative arithmetic task.
//!
//! `generate` and `step_core` are deterministic. They do not read the clock
//! or another session. `Problem::value` exists so an `answer` action can be
//! scored. Observation text and the public state frame must not copy it.
//! This task is not an upstream OpenEnv or TextArena environment.

pub const ENV_ID: &str = "Arith-v0";
pub const MAX_ACTION_BYTES: usize = 4096;
pub const MAX_STEPS: u8 = 8;
pub const MIN_OPS: u8 = 2;
pub const MAX_OPS: u8 = 4;
pub const DEFAULT_OPS: u8 = 3;
const MAX_TOKEN_DIGITS: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub numbers: Vec<i64>,
    pub ops: Vec<Op>,
    /// Scored by `answer` only. Do not render this.
    pub value: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Calc { left: i64, op: Op, right: i64 },
    Answer(i64),
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Tool { text: String },
    Correct,
    Incorrect,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreError {
    AlreadyDone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Core {
    pub problem: Problem,
    pub done: bool,
    pub won: bool,
    pub steps: u8,
    pub last_public: String,
    pub last_format_valid: bool,
    pub reward_milli: i64,
}

pub fn normalize_ops(ops: u8) -> u8 {
    ops.clamp(MIN_OPS, MAX_OPS)
}

pub fn episode_seed(episode_id: &str) -> i64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in episode_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash as i64
}

pub fn generate(seed: i64, ops: u8) -> Problem {
    let ops = normalize_ops(ops);
    let mut state = (seed as u64) ^ 0xA5A5_A5A5_A5A5_A5A5;
    let count = usize::from(ops);
    let mut numbers = Vec::with_capacity(count + 1);
    let mut operators = Vec::with_capacity(count);
    numbers.push(draw_digit(&mut state));
    for _ in 0..count {
        operators.push(draw_op(&mut state));
        numbers.push(draw_digit(&mut state));
    }
    let value = eval_left(&numbers, &operators).unwrap_or(0);
    Problem {
        numbers,
        ops: operators,
        value,
    }
}

pub fn expression(problem: &Problem) -> String {
    let Some(first) = problem.numbers.first() else {
        return String::new();
    };
    let mut text = first.to_string();
    for (index, op) in problem.ops.iter().enumerate() {
        let Some(right) = problem.numbers.get(index + 1) else {
            break;
        };
        text = format!("({text} {} {right})", op.as_str());
    }
    text
}

pub fn prompt_text(expression: &str) -> String {
    format!(
        "Expression: {expression}\n\
The parentheses are left to right.\n\
The whole reply is one line. It starts with calc or answer.\n\
calc <integer> <op> <integer>\n\
answer <integer>\n\
op is + or - or *. One operator per calc. calc does not solve the expression.\n\
Words after a complete command are ignored.\n\
Example lines, which are not this problem:\n\
calc 2 + 3\n\
answer 5\n"
    )
}

pub fn parse_action(message: &str) -> Action {
    let Some(line) = message.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return Action::Invalid;
    };
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let Some(command) = tokens.first() else {
        return Action::Invalid;
    };
    let command = command.to_ascii_lowercase();
    // The command has to be the start of the line. A longer line still counts
    // when its first tokens are a complete command. That is formatting, not a
    // search over candidate answers.
    match command.as_str() {
        "calc" if tokens.len() >= 4 => {
            let Some(left) = parse_i64(tokens[1]) else {
                return Action::Invalid;
            };
            let Some(op) = parse_op(tokens[2]) else {
                return Action::Invalid;
            };
            let Some(right) = parse_i64(tokens[3]) else {
                return Action::Invalid;
            };
            Action::Calc { left, op, right }
        }
        "answer" if tokens.len() >= 2 => match parse_i64(tokens[1]) {
            Some(value) => Action::Answer(value),
            None => Action::Invalid,
        },
        _ => Action::Invalid,
    }
}

pub fn fresh(problem: Problem) -> Core {
    Core {
        problem,
        done: false,
        won: false,
        steps: 0,
        last_public: String::new(),
        last_format_valid: false,
        reward_milli: 0,
    }
}

pub fn step_core(core: &Core, action: Action) -> Result<Core, CoreError> {
    if core.done {
        return Err(CoreError::AlreadyDone);
    }
    let steps = core.steps.saturating_add(1);
    let at_limit = steps >= MAX_STEPS;
    let mut terminal_answer = false;
    let (mut last_public, reward_milli, won, format_valid) = match action {
        Action::Calc { left, op, right } => {
            let text = match apply_op(left, op, right) {
                Some(value) => format!("Result: {value}"),
                None => "Result: overflow".to_string(),
            };
            (text, 0, false, true)
        }
        Action::Answer(value) => {
            terminal_answer = true;
            if value == core.problem.value {
                ("Correct.".to_string(), 1000, true, true)
            } else {
                ("Incorrect.".to_string(), 0, false, true)
            }
        }
        Action::Invalid => (
            "Invalid action. Send `calc <int> <op> <int>` or `answer <int>`.".to_string(),
            0,
            false,
            false,
        ),
    };
    if at_limit && !terminal_answer {
        last_public.push_str(" Step limit reached.");
    }
    Ok(Core {
        problem: core.problem.clone(),
        done: terminal_answer || at_limit,
        won,
        steps,
        last_public,
        last_format_valid: format_valid,
        reward_milli,
    })
}

fn apply_op(left: i64, op: Op, right: i64) -> Option<i64> {
    match op {
        Op::Add => left.checked_add(right),
        Op::Sub => left.checked_sub(right),
        Op::Mul => left.checked_mul(right),
    }
}

fn eval_left(numbers: &[i64], ops: &[Op]) -> Option<i64> {
    let mut acc = *numbers.first()?;
    if numbers.len() != ops.len() + 1 {
        return None;
    }
    for (op, right) in ops.iter().zip(numbers.iter().skip(1)) {
        acc = apply_op(acc, *op, *right)?;
    }
    Some(acc)
}

fn draw_digit(state: &mut u64) -> i64 {
    let digit = (splitmix(state) % 9) + 1;
    i64::try_from(digit).unwrap_or(1)
}

fn draw_op(state: &mut u64) -> Op {
    match splitmix(state) % 3 {
        0 => Op::Add,
        1 => Op::Sub,
        _ => Op::Mul,
    }
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn parse_op(token: &str) -> Option<Op> {
    match token {
        "+" => Some(Op::Add),
        "-" => Some(Op::Sub),
        "*" => Some(Op::Mul),
        _ => None,
    }
}

fn parse_i64(token: &str) -> Option<i64> {
    let (negative, digits) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    if digits.is_empty() || digits.len() > MAX_TOKEN_DIGITS {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut value: i64 = 0;
    for byte in digits.bytes() {
        let digit = i64::from(byte - b'0');
        value = value.checked_mul(10)?.checked_add(digit)?;
    }
    if negative {
        value.checked_neg()
    } else {
        Some(value)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn sample(numbers: &[i64], ops: &[Op]) -> Problem {
        let value = eval_left(numbers, ops).unwrap();
        Problem {
            numbers: numbers.to_vec(),
            ops: ops.to_vec(),
            value,
        }
    }

    #[test]
    fn left_assoc_matches_manual_values() {
        let problem = sample(&[2, 3, 4], &[Op::Mul, Op::Add]);
        assert_eq!(problem.value, 10);
        assert_eq!(expression(&problem), "((2 * 3) + 4)");
        let nested = sample(&[8, 3, 2, 5], &[Op::Sub, Op::Mul, Op::Add]);
        assert_eq!(nested.value, 15);
        assert_eq!(expression(&nested), "(((8 - 3) * 2) + 5)");
        assert!(!prompt_text(&expression(&problem)).contains("\n10\n"));
    }

    #[test]
    fn generate_is_deterministic_and_in_range() {
        let first = generate(7, 3);
        assert_eq!(first, generate(7, 3));
        assert_eq!(first.ops.len(), 3);
        assert_eq!(first.numbers.len(), 4);
        assert!(first.numbers.iter().all(|number| (1..=9).contains(number)));
        assert_ne!(generate(1, 3).value, generate(2, 4).value);
        let mut differed = false;
        for seed in 0..30 {
            if generate(seed, 3) != generate(seed + 1, 3) {
                differed = true;
                break;
            }
        }
        assert!(differed);
        for seed in 0..200 {
            for ops in MIN_OPS..=MAX_OPS {
                let problem = generate(seed, ops);
                let walked = eval_left(&problem.numbers, &problem.ops).unwrap();
                assert_eq!(walked, problem.value);
                assert_eq!(
                    expression(&problem)
                        .chars()
                        .filter(|ch| matches!(ch, '+' | '-' | '*'))
                        .count(),
                    usize::from(ops)
                );
            }
        }
    }

    #[test]
    fn parse_accepts_one_command_line() {
        assert_eq!(
            parse_action("calc 2 * 3"),
            Action::Calc {
                left: 2,
                op: Op::Mul,
                right: 3
            }
        );
        assert_eq!(
            parse_action("  CALC\t2 * 3\nplease ignore"),
            Action::Calc {
                left: 2,
                op: Op::Mul,
                right: 3
            }
        );
        assert_eq!(parse_action("answer -4"), Action::Answer(-4));
        assert_eq!(parse_action("answer 5 please"), Action::Answer(5));
        assert_eq!(
            parse_action("calc 10 * 2 - 5"),
            Action::Calc { left: 10, op: Op::Mul, right: 2 }
        );
        assert_eq!(parse_action("The expression evaluates to 10."), Action::Invalid);
        assert_eq!(parse_action("17"), Action::Invalid);
        assert_eq!(parse_action("answer 5.0"), Action::Invalid);
        assert_eq!(parse_action("calc 1+2"), Action::Invalid);
        assert_eq!(parse_action(""), Action::Invalid);
        assert_eq!(parse_action("calc 9999999999999 + 1"), Action::Invalid);
    }

    #[test]
    fn scoring_hides_the_value_and_stops_at_the_cap() {
        let core = fresh(sample(&[2, 3, 4], &[Op::Mul, Op::Add]));
        let calc = step_core(
            &core,
            Action::Calc {
                left: 2,
                op: Op::Mul,
                right: 3,
            },
        )
        .unwrap();
        assert_eq!(calc.last_public, "Result: 6");
        assert!(!calc.done);
        assert!(calc.last_format_valid);
        assert_eq!(calc.reward_milli, 0);

        let wrong = step_core(&calc, Action::Answer(11)).unwrap();
        assert!(wrong.done && !wrong.won);
        assert_eq!(wrong.reward_milli, 0);
        assert_eq!(wrong.last_public, "Incorrect.");
        assert!(!wrong.last_public.contains("10"));
        assert!(step_core(&wrong, Action::Answer(10)).is_err());

        let again = fresh(core.problem.clone());
        let right = step_core(&again, Action::Answer(10)).unwrap();
        assert!(right.done && right.won);
        assert_eq!(right.reward_milli, 1000);
        assert_eq!(right.last_public, "Correct.");
        assert!(!right.last_public.contains("10"));

        let mut limited = fresh(core.problem);
        for _ in 0..(MAX_STEPS - 1) {
            limited = step_core(&limited, Action::Invalid).unwrap();
            assert!(!limited.done);
            assert!(!limited.last_format_valid);
            assert!(!limited.last_public.contains("10"));
        }
        limited = step_core(&limited, Action::Invalid).unwrap();
        assert!(limited.done && !limited.won);
        assert_eq!(limited.reward_milli, 0);
        assert!(limited.last_public.contains("Step limit reached."));
        assert!(!limited.last_public.contains("10"));

        let overflow = step_core(
            &fresh(sample(&[1, 1], &[Op::Add])),
            Action::Calc {
                left: 999_999_999_999,
                op: Op::Mul,
                right: 999_999_999_999,
            },
        )
        .unwrap();
        assert_eq!(overflow.last_public, "Result: overflow");
        assert!(overflow.last_format_valid);
        assert!(!overflow.done);
    }
}
