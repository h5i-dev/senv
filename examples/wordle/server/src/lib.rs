//! Wordle as a senv environment, on the OpenEnv `TextArenaEnv` WebSocket contract.
//!
//! Marking, rewards, and termination are decided by `wordle-kernel`, which
//! `h5i app extract` translates to Lean and `../proofs/Theorems.lean` proves
//! things about. This crate is the trusted shell around it: it picks the
//! secret from the seed, looks guesses up in the dictionary, keeps the episode
//! id and the transcript, and renders TextArena-shaped JSON. Reward signals are
//! computed from the committed transcript the same way OpenEnv's Wordle
//! provider reads the observation text.

pub mod game;

use game::{
    ENV_ID, LogEntry, MAX_ACTION_BYTES, ascii_word, mark_char, parse_message, prompt_text,
    push_phase, render_messages, secret_from_episode, secret_from_seed, signals_for_log,
};
use senv::wire::{reject_unknown, string_field};
use senv::{Environment, Kernel, Metadata, Observation, Phase, Reward, TenantId, WireError};
use serde_json::{Map, Value, json};
use wordle_kernel::{self as k, WORD_LEN};

/// A session's state: the kernel's game plus the shell's bookkeeping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Episode {
    pub episode_id: String,
    pub seed: Option<i64>,
    pub step_count: u64,
    pub log: Vec<LogEntry>,
    pub core: k::Core,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Reset {
        seed: Option<i64>,
        episode_id: String,
        secret: [u8; WORD_LEN],
    },
    /// `message` is the agent's raw text; `mv` is what the shell parsed.
    Step {
        message: String,
        mv: k::Move,
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
pub struct WordleKernel;

impl Kernel for WordleKernel {
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
                secret,
            } => {
                let (w, reply) =
                    k::transition(&(), &ep.core, &k::Command::Reset { secret: *secret })
                        .map_err(Error::Kernel)?;
                Ok((
                    Some(Episode {
                        episode_id: episode_id.clone(),
                        seed: *seed,
                        step_count: 0,
                        log: Vec::new(),
                        core: k::apply(&ep.core, &w),
                    }),
                    reply,
                ))
            }
            Command::Step { message, mv } => {
                if message.len() > MAX_ACTION_BYTES {
                    return Err(Error::ActionTooLong);
                }
                let step_count = ep
                    .step_count
                    .checked_add(1)
                    .ok_or(Error::StepCountOverflow)?;
                let (w, reply) = k::transition(&(), &ep.core, &k::Command::Step { mv: *mv })
                    .map_err(Error::Kernel)?;
                let mut log = ep.log.clone();
                if let Some(phase) = &reply.phase {
                    push_phase(&mut log, message, phase);
                }
                Ok((
                    Some(Episode {
                        step_count,
                        log,
                        core: k::apply(&ep.core, &w),
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

/// Wordle-v0. It has no configuration.
#[derive(Clone, Copy, Debug, Default)]
pub struct Wordle;

impl Environment for Wordle {
    type Kernel = WordleKernel;

    fn metadata(&self) -> Metadata {
        Metadata::new(
            "wordle_v0",
            "Classic Wordle on the OpenEnv TextArenaEnv WebSocket contract. Not the full TextArena suite, not the NLTK or New York Times word lists, and not CPython's RNG.",
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
            core: k::fresh(secret_from_episode(episode_id)),
            ..Episode::default()
        }
    }

    fn reset(&self, seed: Option<i64>, episode_id: String) -> Command {
        let secret = match seed {
            Some(seed) => secret_from_seed(seed),
            None => secret_from_episode(&episode_id),
        };
        Command::Reset {
            seed,
            episode_id,
            secret,
        }
    }

    fn step(&self, action: &Map<String, Value>) -> Result<Command, WireError> {
        reject_unknown(action, &["message"])?;
        let message = string_field(action, "message")?;
        Ok(Command::Step {
            mv: parse_message(&message),
            message,
        })
    }

    fn state(&self) -> Command {
        Command::State
    }

    fn observe(&self, phase: Phase, snapshot: &Episode, _reply: &k::Reply) -> Observation {
        observation(snapshot, phase == Phase::Step)
    }

    fn render_state(&self, snapshot: &Episode, _reply: &k::Reply) -> Value {
        state(snapshot)
    }

    fn refuse(&self, error: &Error) -> WireError {
        match error {
            Error::ActionTooLong => {
                WireError::validation("action message is longer than 4096 bytes")
            }
            Error::Kernel(k::Error::Done) => WireError::execution("episode is done"),
            Error::Kernel(k::Error::InvalidSecret) => {
                WireError::execution("the secret is not a five-letter word")
            }
            Error::StepCountOverflow => WireError::execution("step count overflowed"),
        }
    }
}

fn observation(snap: &Episode, with_signals: bool) -> Observation {
    let messages = render_messages(&snap.core.secret, snap.core.reward_milli, &snap.log);
    let message_values: Vec<Value> = messages
        .iter()
        .map(|message| {
            json!({
                "sender_id": message.sender_id,
                "content": message.content,
                "category": message.category,
            })
        })
        .collect();
    let mut metadata = json!({
        "env_id": ENV_ID,
        "turn": snap.core.turn,
        "raw_messages": message_values.clone(),
    });
    let mut info = Map::new();
    if with_signals && let Some(signals) = signals_for_log(&snap.log, snap.core.reward_milli) {
        let value = json!({
            "wordle.greens": signals.greens,
            "wordle.yellows": signals.yellows,
            "wordle.repetitions": signals.repetitions,
            "wordle.correct": signals.correct,
        });
        info.insert("reward_signals".to_string(), value.clone());
        if let Some(object) = metadata.as_object_mut() {
            object.insert("reward_signals".to_string(), value);
        }
    }
    let observation = json!({
        "prompt": prompt_text(),
        "messages": message_values,
        "current_player_id": 0,
        "legal_players": [0],
        "info": info,
        "metadata": metadata.clone(),
    });
    Observation::new(
        observation,
        Reward::Milli(snap.core.reward_milli),
        snap.core.done,
    )
    .with_metadata(metadata)
}

fn state(snap: &Episode) -> Value {
    let mut history = Vec::new();
    for index in 0..usize::from(snap.core.guess_count) {
        let letters = snap.core.guesses[index];
        let marks = k::mark(&snap.core.secret, &letters);
        let code: Vec<String> = marks
            .iter()
            .map(|mark| mark_char(*mark).to_string())
            .collect();
        history.push(json!([ascii_word(&letters), code]));
    }
    let rewards = if snap.core.done {
        json!({ "0": milli_f64(snap.core.reward_milli) })
    } else {
        Value::Null
    };
    let mut last_info = Map::new();
    if let Some(signals) = signals_for_log(&snap.log, snap.core.reward_milli) {
        last_info.insert(
            "reward_signals".to_string(),
            json!({
                "wordle.greens": signals.greens,
                "wordle.yellows": signals.yellows,
                "wordle.repetitions": signals.repetitions,
                "wordle.correct": signals.correct,
            }),
        );
    }
    json!({
        "episode_id": snap.episode_id,
        "step_count": snap.step_count,
        "env_id": ENV_ID,
        "num_players": 1,
        "max_turns": null,
        "turn": snap.core.turn,
        "last_reward": milli_f64(snap.core.reward_milli),
        "last_info": last_info,
        "raw_state": {
            "turn": snap.core.turn,
            "done": snap.core.done,
            "won": snap.core.won,
            "guess_count": snap.core.guess_count,
            "rewards": rewards,
            "game_state": {
                "word_length": WORD_LEN,
                "num_guesses": k::MAX_GUESSES,
                "secret_word": ascii_word(&snap.core.secret),
                "guess_history": history,
            }
        }
    })
}

fn milli_f64(milli: i64) -> f64 {
    (milli as f64) / 1000.0
}

pub fn schema_document() -> Value {
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
            "description": "TextArenaObservation packed like OpenEnv serialize_observation"
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
