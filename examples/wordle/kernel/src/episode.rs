//! One Wordle episode as an h5i kernel: snapshot, commands, and `transition`.
//!
//! The shell picks the secret from the seed and passes it in `Reset`, so this
//! module never chooses a word. It decides the guess log, the reward, and
//! `done`. The server crate binds it to h5i-app's `Kernel` trait.

use crate::{
    Core, CoreError, LogEntry, MAX_ACTION_BYTES, WORD_LEN, fresh, parse_message, push_phase,
    step_core,
};

/// One session's actor. Sessions are isolated by separate engines, not by this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Principal {
    pub tenant: u64,
}

pub const SESSION_TENANT: u64 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub episode_id: String,
    pub step_count: u64,
    pub seed: Option<i64>,
    pub core: Core,
    pub log: Vec<LogEntry>,
    pub last_reward_milli: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Reset {
        seed: Option<i64>,
        episode_id: String,
        secret: [u8; WORD_LEN],
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
pub enum WordleError {
    AlreadyDone,
    StepCountOverflow,
    ActionTooLong,
}

pub fn transition(
    _actor: &Principal,
    snap: &Snapshot,
    cmd: &Command,
) -> Result<(Snapshot, Reply), WordleError> {
    match cmd {
        Command::Reset {
            seed,
            episode_id,
            secret,
        } => {
            let next = Snapshot {
                episode_id: episode_id.clone(),
                step_count: 0,
                seed: *seed,
                core: fresh(*secret),
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
            if message.len() > MAX_ACTION_BYTES {
                return Err(WordleError::ActionTooLong);
            }
            let mv = parse_message(message);
            let (core, phase) = step_core(&snap.core, mv).map_err(|error| match error {
                CoreError::AlreadyDone => WordleError::AlreadyDone,
            })?;
            let step_count = snap
                .step_count
                .checked_add(1)
                .ok_or(WordleError::StepCountOverflow)?;
            let mut log = snap.log.clone();
            push_phase(&mut log, message, &phase);
            let reward_milli = core.reward_milli;
            let done = core.done;
            Ok((
                Snapshot {
                    episode_id: snap.episode_id.clone(),
                    step_count,
                    seed: snap.seed,
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
