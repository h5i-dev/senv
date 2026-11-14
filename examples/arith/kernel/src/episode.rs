//! One Arith episode as an h5i kernel: snapshot, commands, and `transition`.
//!
//! The shell derives the problem from the seed and passes it in `Reset`.
//! `Problem::value` stays in the snapshot so `answer` can be scored; the
//! server's observation and state JSON omit it. The server crate binds this
//! module to h5i-app's `Kernel` trait.

use crate::{Core, Problem, fresh, parse_action, step_core};

/// One session's actor. Sessions are isolated by separate engines, not by this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Principal {
    pub tenant: u64,
}

pub const SESSION_TENANT: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub player: String,
    pub reply: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub episode_id: String,
    pub step_count: u64,
    pub seed: Option<i64>,
    pub ops: u8,
    pub core: Core,
    pub log: Vec<LogEntry>,
    pub last_reward_milli: i64,
}

impl Default for Snapshot {
    fn default() -> Self {
        let ops = crate::DEFAULT_OPS;
        Self {
            episode_id: String::new(),
            step_count: 0,
            seed: None,
            ops,
            core: fresh(crate::generate(0, ops)),
            log: Vec::new(),
            last_reward_milli: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Reset {
        seed: Option<i64>,
        episode_id: String,
        problem: Problem,
    },
    Step {
        message: String,
    },
    State,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Observation { reward_milli: i64, done: bool },
    State { episode_id: String, step_count: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithError {
    AlreadyDone,
    StepCountOverflow,
    ActionTooLong,
}

pub fn transition(
    _actor: &Principal,
    snap: &Snapshot,
    cmd: &Command,
) -> Result<(Snapshot, Reply), ArithError> {
    match cmd {
        Command::Reset {
            seed,
            episode_id,
            problem,
        } => {
            let ops = u8::try_from(problem.ops.len()).unwrap_or(crate::DEFAULT_OPS);
            let next = Snapshot {
                episode_id: episode_id.clone(),
                step_count: 0,
                seed: *seed,
                ops,
                core: fresh(problem.clone()),
                log: Vec::new(),
                last_reward_milli: 0,
            };
            Ok((
                next,
                Reply::Observation {
                    reward_milli: 0,
                    done: false,
                },
            ))
        }
        Command::Step { message } => {
            if message.len() > crate::MAX_ACTION_BYTES {
                return Err(ArithError::ActionTooLong);
            }
            let action = parse_action(message);
            let core = step_core(&snap.core, action).map_err(|error| match error {
                crate::CoreError::AlreadyDone => ArithError::AlreadyDone,
            })?;
            let step_count = snap
                .step_count
                .checked_add(1)
                .ok_or(ArithError::StepCountOverflow)?;
            let mut log = snap.log.clone();
            log.push(LogEntry {
                player: message.clone(),
                reply: core.last_public.clone(),
            });
            let reward_milli = core.reward_milli;
            let done = core.done;
            Ok((
                Snapshot {
                    episode_id: snap.episode_id.clone(),
                    step_count,
                    seed: snap.seed,
                    ops: snap.ops,
                    core,
                    log,
                    last_reward_milli: reward_milli,
                },
                Reply::Observation { reward_milli, done },
            ))
        }
        Command::State => Ok((
            snap.clone(),
            Reply::State {
                episode_id: snap.episode_id.clone(),
                step_count: snap.step_count,
            },
        )),
    }
}

pub fn apply(_snap: &Snapshot, next: &Snapshot) -> Snapshot {
    next.clone()
}
