//! Wordle as a senv environment, on the OpenEnv `TextArenaEnv` WebSocket contract.
//!
//! The kernel (`wordle-kernel`) decides marks, rewards, and `done`. This crate
//! is the trusted shell: it picks the secret from the seed, binds the kernel to
//! h5i-app, and renders TextArena-shaped JSON from the committed snapshot.
//! Reward signals are computed from the committed log the same way OpenEnv's
//! Wordle provider reads the observation text.

use senv::wire::{reject_unknown, string_field};
use senv::{Environment, Kernel, Metadata, Observation, Phase, Reward, TenantId, WireError};
use serde_json::{Map, Value, json};
use wordle_kernel::episode::{Command, Principal, Reply, SESSION_TENANT, Snapshot, WordleError};
use wordle_kernel::{
    self as wordle, WORD_LEN, ascii_word, fresh, prompt_text, render_messages, secret_from_episode,
    secret_from_seed, signals_for_log,
};

/// The h5i-app binding of the Wordle kernel.
pub struct WordleKernel;

impl Kernel for WordleKernel {
    type Principal = Principal;
    type Snapshot = Snapshot;
    type Command = Command;
    type WriteSet = Snapshot;
    type Reply = Reply;
    type Error = WordleError;

    fn tenant(actor: &Principal) -> TenantId {
        TenantId(actor.tenant)
    }

    fn transition(
        actor: &Principal,
        snap: &Snapshot,
        cmd: &Command,
    ) -> Result<(Snapshot, Reply), WordleError> {
        wordle_kernel::episode::transition(actor, snap, cmd)
    }

    fn apply(snap: &Snapshot, ws: &Snapshot) -> Snapshot {
        wordle_kernel::episode::apply(snap, ws)
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

    fn principal(&self) -> Principal {
        Principal {
            tenant: SESSION_TENANT,
        }
    }

    fn initial(&self, episode_id: &str) -> Snapshot {
        Snapshot {
            episode_id: episode_id.to_string(),
            core: fresh(secret_from_episode(episode_id)),
            ..Snapshot::default()
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
        Ok(Command::Step {
            message: string_field(action, "message")?,
        })
    }

    fn state(&self) -> Command {
        Command::State
    }

    fn observe(&self, phase: Phase, snapshot: &Snapshot, _reply: &Reply) -> Observation {
        observation(snapshot, phase == Phase::Step)
    }

    fn render_state(&self, snapshot: &Snapshot, _reply: &Reply) -> Value {
        state(snapshot)
    }

    fn refuse(&self, error: &WordleError) -> WireError {
        match error {
            WordleError::ActionTooLong => {
                WireError::validation("action message is longer than 4096 bytes")
            }
            WordleError::AlreadyDone => WireError::execution("episode is done"),
            WordleError::StepCountOverflow => WireError::execution("step count overflowed"),
        }
    }
}

fn observation(snap: &Snapshot, with_signals: bool) -> Observation {
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
        "env_id": wordle::ENV_ID,
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
        Reward::Milli(snap.last_reward_milli),
        snap.core.done,
    )
    .with_metadata(metadata)
}

fn state(snap: &Snapshot) -> Value {
    let mut history = Vec::new();
    for index in 0..usize::from(snap.core.guess_count) {
        let letters = snap.core.guesses[index];
        let marks = wordle::mark(&snap.core.secret, &letters);
        let code: Vec<String> = marks
            .iter()
            .map(|mark| mark.as_char().to_string())
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
        "env_id": wordle::ENV_ID,
        "num_players": 1,
        "max_turns": null,
        "turn": snap.core.turn,
        "last_reward": milli_f64(snap.last_reward_milli),
        "last_info": last_info,
        "raw_state": {
            "turn": snap.core.turn,
            "done": snap.core.done,
            "won": snap.core.won,
            "guess_count": snap.core.guess_count,
            "rewards": rewards,
            "game_state": {
                "word_length": WORD_LEN,
                "num_guesses": wordle::MAX_GUESSES,
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
                "message": { "type": "string", "maxLength": wordle::MAX_ACTION_BYTES }
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
                "env_id": { "const": wordle::ENV_ID },
                "num_players": { "const": 1 },
                "turn": { "type": "integer", "minimum": 0 }
            },
            "required": ["step_count", "env_id"]
        }
    })
}
