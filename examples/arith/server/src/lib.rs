//! Arith-v0 as a senv environment, on the OpenEnv `TextArenaEnv` WebSocket contract.
//!
//! A custom multi-turn tool-use task, not an upstream OpenEnv environment.
//! Every decision that affects the score (the calculator's results, whether an
//! answer is correct, the reward, when the episode ends) is made by
//! `arith-kernel`, which `h5i app extract` translates to Lean and
//! `../proofs/Theorems.lean` proves things about.
//!
//! This crate is the trusted shell around it:
//!
//! - it derives the problem from the seed (`generate`),
//! - it parses the agent's text into a kernel `Action` (`parse_action`),
//! - it keeps the episode id and the transcript, which the kernel does not
//!   need to decide anything, and
//! - it renders TextArena-shaped JSON.
//!
//! None of that is proved. The reward and `done` it reports are copied from
//! the kernel's committed snapshot.

use arith_kernel::{self as k, Feedback, MAX_STEPS, Op};
use senv::wire::{reject_unknown, string_field};
use senv::{Environment, Kernel, Metadata, Observation, Phase, Reward, TenantId, WireError};
use serde_json::{Map, Value, json};

pub const ENV_ID: &str = "Arith-v0";
pub const MAX_ACTION_BYTES: usize = 4096;
pub const MIN_OPS: u8 = 2;
pub const MAX_OPS: u8 = 4;
pub const DEFAULT_OPS: u8 = 3;
const MAX_TOKEN_DIGITS: usize = 12;

/// One exchange in the transcript the agent sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub message: String,
    pub reply: String,
}

/// A session's state: the kernel's snapshot plus the shell's bookkeeping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Episode {
    pub episode_id: String,
    pub seed: Option<i64>,
    pub step_count: u64,
    pub ops: u8,
    pub log: Vec<Turn>,
    pub core: k::Snapshot,
}

impl Default for Episode {
    fn default() -> Self {
        Self {
            episode_id: String::new(),
            seed: None,
            step_count: 0,
            ops: DEFAULT_OPS,
            log: Vec::new(),
            core: k::Snapshot::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Reset {
        seed: Option<i64>,
        episode_id: String,
        problem: k::Problem,
    },
    /// `message` is the agent's raw text; `action` is what the shell parsed.
    Step {
        message: String,
        action: k::Action,
    },
    State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Kernel(k::Error),
    ActionTooLong,
    StepCountOverflow,
}

/// The h5i-app binding: the extracted kernel decides, this wrapper records.
pub struct ArithKernel;

impl Kernel for ArithKernel {
    type Principal = ();
    type Snapshot = Episode;
    type Command = Command;
    type WriteSet = Option<Episode>;
    type Reply = k::Reply;
    type Error = Error;

    fn tenant(_: &()) -> TenantId {
        TenantId(1)
    }

    fn transition(
        _: &(),
        ep: &Episode,
        cmd: &Command,
    ) -> Result<(Option<Episode>, k::Reply), Error> {
        match cmd {
            Command::Reset {
                seed,
                episode_id,
                problem,
            } => {
                let (w, reply) =
                    k::transition(&(), &ep.core, &k::Command::Reset { problem: *problem })
                        .map_err(Error::Kernel)?;
                let core = k::apply(&ep.core, &w);
                Ok((
                    Some(Episode {
                        episode_id: episode_id.clone(),
                        seed: *seed,
                        step_count: 0,
                        ops: problem.n_ops,
                        log: Vec::new(),
                        core,
                    }),
                    reply,
                ))
            }
            Command::Step { message, action } => {
                if message.len() > MAX_ACTION_BYTES {
                    return Err(Error::ActionTooLong);
                }
                let step_count = ep
                    .step_count
                    .checked_add(1)
                    .ok_or(Error::StepCountOverflow)?;
                let (w, reply) =
                    k::transition(&(), &ep.core, &k::Command::Step { action: *action })
                        .map_err(Error::Kernel)?;
                let core = k::apply(&ep.core, &w);
                let mut log = ep.log.clone();
                log.push(Turn {
                    message: message.clone(),
                    reply: reply_text(&core, action),
                });
                Ok((
                    Some(Episode {
                        step_count,
                        log,
                        core,
                        ..ep.clone()
                    }),
                    reply,
                ))
            }
            Command::State => {
                let (_, reply) =
                    k::transition(&(), &ep.core, &k::Command::State).map_err(Error::Kernel)?;
                Ok((None, reply))
            }
        }
    }

    fn apply(ep: &Episode, w: &Option<Episode>) -> Episode {
        match w {
            Some(next) => next.clone(),
            None => ep.clone(),
        }
    }

    fn read_only(cmd: &Command) -> bool {
        matches!(cmd, Command::State)
    }
}

/// Arith-v0 with a fixed operator count.
#[derive(Clone, Copy, Debug)]
pub struct Arith {
    ops: u8,
}

impl Arith {
    /// `ops` is clamped to `2..=4`.
    pub fn new(ops: u8) -> Self {
        Self {
            ops: ops.clamp(MIN_OPS, MAX_OPS),
        }
    }

    pub fn ops(&self) -> u8 {
        self.ops
    }
}

impl Default for Arith {
    fn default() -> Self {
        Self::new(DEFAULT_OPS)
    }
}

impl Environment for Arith {
    type Kernel = ArithKernel;

    fn metadata(&self) -> Metadata {
        Metadata::new(
            "arith_v0",
            "Custom senv arithmetic task on the TextArenaEnv WebSocket contract. Not an upstream OpenEnv or TextArena environment. The expression value is scored inside the server and is not part of the observation or state.",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .author("h5i-dev")
        .documentation_url("https://github.com/h5i-dev/senv")
    }

    fn schema(&self) -> Value {
        schema_document()
    }

    fn principal(&self) {}

    fn initial(&self, episode_id: &str) -> Episode {
        Episode {
            episode_id: episode_id.to_string(),
            ops: self.ops,
            core: k::fresh(generate(episode_seed(episode_id), self.ops)),
            ..Episode::default()
        }
    }

    fn reset(&self, seed: Option<i64>, episode_id: String) -> Command {
        let problem_seed = seed.unwrap_or_else(|| episode_seed(&episode_id));
        Command::Reset {
            seed,
            problem: generate(problem_seed, self.ops),
            episode_id,
        }
    }

    fn step(&self, action: &Map<String, Value>) -> Result<Command, WireError> {
        reject_unknown(action, &["message"])?;
        let message = string_field(action, "message")?;
        Ok(Command::Step {
            action: parse_action(&message),
            message,
        })
    }

    fn state(&self) -> Command {
        Command::State
    }

    fn observe(&self, _phase: Phase, ep: &Episode, reply: &k::Reply) -> Observation {
        let metadata = json!({
            "env_id": ENV_ID,
            "ops": ep.ops,
            "turn": ep.core.steps,
        });
        let messages: Vec<Value> = ep
            .log
            .iter()
            .flat_map(|turn| {
                [
                    json!({ "sender_id": 0, "content": turn.message, "category": "PLAYER_ACTION" }),
                    json!({ "sender_id": -1, "content": turn.reply, "category": "GAME_MESSAGE" }),
                ]
            })
            .collect();
        let observation = json!({
            "prompt": prompt_text(&expression(&ep.core.problem)),
            "messages": messages,
            "current_player_id": 0,
            "legal_players": [0],
            "info": info(ep),
            "metadata": metadata.clone(),
        });
        Observation::new(
            observation,
            Reward::Milli(reward_milli(reply.reward_milli)),
            reply.done,
        )
        .with_metadata(metadata)
    }

    fn render_state(&self, ep: &Episode, reply: &k::Reply) -> Value {
        json!({
            "episode_id": ep.episode_id,
            "step_count": ep.step_count,
            "env_id": ENV_ID,
            "num_players": 1,
            "max_turns": MAX_STEPS,
            "turn": ep.core.steps,
            "last_reward": reward_milli(reply.reward_milli) as f64 / 1000.0,
            "last_info": info(ep),
            "raw_state": {
                "expression": expression(&ep.core.problem),
                "ops": ep.ops,
                "steps": ep.core.steps,
                "max_steps": MAX_STEPS,
                "done": ep.core.done,
                "last_reply": ep.log.last().map(|turn| turn.reply.as_str()).unwrap_or(""),
            }
        })
    }

    fn refuse(&self, error: &Error) -> WireError {
        match error {
            Error::ActionTooLong => {
                WireError::validation("action message is longer than 4096 bytes")
            }
            Error::Kernel(k::Error::Done) => WireError::execution("episode is done"),
            Error::Kernel(k::Error::InvalidProblem) => {
                WireError::execution("the generated problem is outside the task")
            }
            Error::StepCountOverflow => WireError::execution("step count overflowed"),
        }
    }
}

fn reward_milli(milli: u64) -> i64 {
    i64::try_from(milli).unwrap_or(i64::MAX)
}

fn info(ep: &Episode) -> Map<String, Value> {
    let mut info = Map::new();
    if ep.core.steps > 0 {
        info.insert(
            "arith.format_valid".to_string(),
            Value::Bool(ep.core.last != Feedback::Invalid),
        );
        info.insert("arith.success".to_string(), Value::Bool(ep.core.won));
    }
    info
}

/// The text the agent reads for a committed step.
pub fn reply_text(core: &k::Snapshot, action: &k::Action) -> String {
    let mut text = match core.last {
        Feedback::Computed { value } => format!("Result: {value}"),
        Feedback::Overflow => "Result: overflow".to_string(),
        Feedback::Correct => "Correct.".to_string(),
        Feedback::Incorrect => "Incorrect.".to_string(),
        Feedback::Invalid => {
            "Invalid action. Send `calc <int> <op> <int>` or `answer <int>`.".to_string()
        }
        Feedback::Start => String::new(),
    };
    if core.done && !matches!(action, k::Action::Answer { .. }) {
        text.push_str(" Step limit reached.");
    }
    text
}

/// Seed for an episode reset without one: FNV-1a of the episode id.
pub fn episode_seed(episode_id: &str) -> i64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in episode_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash as i64
}

/// The problem for a seed: digits 1..=9 and operators drawn by splitmix64.
/// Unused slots stay `1` and `+`; the kernel reads only `n_ops` of them.
pub fn generate(seed: i64, ops: u8) -> k::Problem {
    let ops = ops.clamp(MIN_OPS, MAX_OPS);
    let mut state = (seed as u64) ^ 0xA5A5_A5A5_A5A5_A5A5;
    let mut problem = k::empty_problem();
    problem.n_ops = ops;
    problem.digits[0] = draw_digit(&mut state);
    for i in 0..usize::from(ops) {
        problem.ops[i] = draw_op(&mut state);
        problem.digits[i + 1] = draw_digit(&mut state);
    }
    problem
}

fn draw_digit(state: &mut u64) -> i64 {
    // In 1..=9, so the conversion cannot fail.
    i64::try_from((splitmix(state) % 9) + 1).unwrap_or(1)
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

fn op_str(op: Op) -> &'static str {
    match op {
        Op::Add => "+",
        Op::Sub => "-",
        Op::Mul => "*",
    }
}

/// `((d0 o0 d1) o1 d2)`: the expression the agent is shown.
pub fn expression(p: &k::Problem) -> String {
    let mut text = p.digits[0].to_string();
    for i in 0..usize::from(p.n_ops.min(MAX_OPS)) {
        text = format!("({text} {} {})", op_str(p.ops[i]), p.digits[i + 1]);
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

/// The first non-empty line, read as `calc <int> <op> <int>` or
/// `answer <int>`. Trailing tokens after a complete command are ignored.
pub fn parse_action(message: &str) -> k::Action {
    let Some(line) = message.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return k::Action::Invalid;
    };
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let Some(command) = tokens.first() else {
        return k::Action::Invalid;
    };
    match command.to_ascii_lowercase().as_str() {
        "calc" if tokens.len() >= 4 => {
            match (
                parse_i64(tokens[1]),
                parse_op(tokens[2]),
                parse_i64(tokens[3]),
            ) {
                (Some(left), Some(op), Some(right)) => k::Action::Calc { left, op, right },
                _ => k::Action::Invalid,
            }
        }
        "answer" if tokens.len() >= 2 => match parse_i64(tokens[1]) {
            Some(value) => k::Action::Answer { value },
            None => k::Action::Invalid,
        },
        _ => k::Action::Invalid,
    }
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
    if digits.is_empty()
        || digits.len() > MAX_TOKEN_DIGITS
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let mut value: i64 = 0;
    for byte in digits.bytes() {
        value = value.checked_mul(10)?.checked_add(i64::from(byte - b'0'))?;
    }
    if negative {
        value.checked_neg()
    } else {
        Some(value)
    }
}

fn schema_document() -> Value {
    json!({
        "action": {
            "type": "object",
            "properties": {
                "message": { "type": "string", "maxLength": MAX_ACTION_BYTES }
            },
            "required": ["message"]
        },
        "observation": {
            "type": "object",
            "description": "TextArenaObservation. The expression value is omitted."
        },
        "state": {
            "type": "object",
            "properties": {
                "episode_id": { "type": "string" },
                "step_count": { "type": "integer", "minimum": 0 },
                "env_id": { "const": ENV_ID },
                "num_players": { "const": 1 },
                "turn": { "type": "integer", "minimum": 0 }
            },
            "required": ["step_count", "env_id"]
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn seeds_draw_the_same_problems_as_before() {
        // Seed 2000 at two operators was `((6 * 8) * 9)` in every earlier run.
        assert_eq!(expression(&generate(2000, 2)), "((6 * 8) * 9)");
        assert_eq!(k::value(&generate(2000, 2)), Some(432));
        for seed in 0..500 {
            for ops in MIN_OPS..=MAX_OPS {
                let p = generate(seed, ops);
                assert!(k::valid(&p), "{seed} {ops}");
                assert_eq!(
                    expression(&p).matches(['+', '-', '*']).count(),
                    usize::from(ops)
                );
            }
        }
    }

    #[test]
    fn parsing_reads_one_command() {
        use k::Action::*;
        assert_eq!(
            parse_action("calc 2 * 3"),
            Calc {
                left: 2,
                op: Op::Mul,
                right: 3
            }
        );
        assert_eq!(
            parse_action("  CALC\t2 * 3\nplease"),
            Calc {
                left: 2,
                op: Op::Mul,
                right: 3
            }
        );
        assert_eq!(parse_action("answer -4"), Answer { value: -4 });
        assert_eq!(parse_action("answer 5 please"), Answer { value: 5 });
        assert_eq!(
            parse_action("calc 10 * 2 - 5"),
            Calc {
                left: 10,
                op: Op::Mul,
                right: 2
            }
        );
        for bad in [
            "The value is 10.",
            "17",
            "answer 5.0",
            "calc 1+2",
            "",
            "calc 9999999999999 + 1",
        ] {
            assert_eq!(parse_action(bad), Invalid, "{bad}");
        }
    }
}
