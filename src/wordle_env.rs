//! Wordle session on its own `MemoryEngine`.
//!
//! The shell chooses the secret before `transition`, renders TextArena JSON
//! after the engine commits, and does not share this engine with any other
//! session. Reward signals are computed from the committed log the same way
//! OpenEnv's Wordle provider reads the observation text.

use std::sync::Arc;

use axum::http::HeaderMap;
use h5i_app::http::{AuthError, Authenticator, H5iApp};
use h5i_app::pg::{DbError, Store, Tx};
use h5i_app::{Kernel, MemoryEngine, TenantId};
use serde_json::{Map, Number, Value, json};

use crate::kernel::{Principal, SESSION_TENANT};
use crate::protocol::{MAX_EPISODE_ID_BYTES, WireError};
use crate::wordle::{
    self, Core, CoreError, LogEntry, WORD_LEN, ascii_word, fresh, parse_message, prompt_text,
    push_phase, render_messages, secret_from_episode, secret_from_seed, signals_for_log, step_core,
};

pub struct WordleKernel;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WordleError {
    #[error("episode is done")]
    AlreadyDone,
    #[error("step count overflowed")]
    StepCountOverflow,
    #[error("action message is longer than 4096 bytes")]
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
            if message.len() > wordle::MAX_ACTION_BYTES {
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

pub struct WordleStore;

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
        transition(actor, snap, cmd)
    }

    fn apply(snap: &Snapshot, ws: &Snapshot) -> Snapshot {
        apply(snap, ws)
    }

    fn read_only(cmd: &Command) -> bool {
        matches!(cmd, Command::State)
    }
}

impl Store<WordleKernel> for WordleStore {
    fn ddl() -> Vec<String> {
        Vec::new()
    }

    fn tables() -> Vec<&'static str> {
        Vec::new()
    }

    async fn load(_tx: &Tx<'_>, _tenant: TenantId) -> Result<Snapshot, DbError> {
        Err(DbError::Decode(
            "wordle sessions execute on MemoryEngine; there is no PostgreSQL snapshot".into(),
        ))
    }

    async fn write(_tx: &Tx<'_>, _tenant: TenantId, _ws: &Snapshot) -> Result<(), DbError> {
        Err(DbError::Decode(
            "wordle sessions execute on MemoryEngine; there is no PostgreSQL write".into(),
        ))
    }
}

pub struct WordleAuth;

impl Authenticator<WordleKernel> for WordleAuth {
    fn authenticate(&self, _headers: &HeaderMap) -> Result<Principal, AuthError> {
        Ok(Principal {
            tenant: SESSION_TENANT,
        })
    }
}

type WordleApp = H5iApp<WordleKernel, WordleStore, MemoryEngine<WordleKernel>>;

#[derive(Clone)]
pub struct WordleEnvironment {
    app: WordleApp,
    principal: Principal,
}

impl WordleEnvironment {
    pub fn open(episode_id: String) -> Self {
        let secret = secret_from_episode(&episode_id);
        let snap = Snapshot {
            episode_id,
            core: fresh(secret),
            ..Snapshot::default()
        };
        Self::from_snapshot(snap)
    }

    fn from_snapshot(snap: Snapshot) -> Self {
        let engine = Arc::new(
            MemoryEngine::<WordleKernel>::default().with_snapshot(TenantId(SESSION_TENANT), snap),
        );
        let app = H5iApp::<WordleKernel, WordleStore, MemoryEngine<WordleKernel>>::with_executor(
            engine, WordleAuth,
        );
        Self {
            app,
            principal: Principal {
                tenant: SESSION_TENANT,
            },
        }
    }

    fn execute(&self, cmd: Command) -> Result<Reply, WordleError> {
        self.app.engine().execute(&self.principal, &cmd)
    }

    fn snapshot(&self) -> Snapshot {
        self.app.engine().snapshot(TenantId(SESSION_TENANT))
    }

    pub fn reset(&self, seed: Option<i64>, episode_id: String) -> Result<String, WordleError> {
        let secret = match seed {
            Some(seed) => secret_from_seed(seed),
            None => secret_from_episode(&episode_id),
        };
        self.execute(Command::Reset {
            seed,
            episode_id,
            secret,
        })?;
        Ok(observation_frame(&self.snapshot(), false))
    }

    pub fn step(&self, message: String) -> Result<String, WordleError> {
        self.execute(Command::Step { message })?;
        Ok(observation_frame(&self.snapshot(), true))
    }

    pub fn state_frame(&self) -> String {
        state_frame(&self.snapshot())
    }
}

pub fn parse_incoming(text: &str) -> Result<Incoming, WireError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| WireError::new("INVALID_JSON", format!("Invalid JSON: {error}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| WireError::new("VALIDATION_ERROR", "Invalid message"))?;
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| WireError::new("VALIDATION_ERROR", "Invalid message"))?;
    match ty {
        "reset" => {
            reject_unknown(obj, &["type", "data"])?;
            let data = object_field(obj, "data", true)?;
            Ok(Incoming::Reset {
                seed: parse_seed(&data)?,
                episode_id: parse_episode_id(&data)?,
            })
        }
        "step" => {
            reject_unknown(obj, &["type", "data"])?;
            let data = object_field(obj, "data", false)?;
            reject_unknown(&data, &["message"])?;
            let message = match data.get("message") {
                Some(Value::String(message)) => message.clone(),
                Some(_) => {
                    return Err(WireError::new(
                        "VALIDATION_ERROR",
                        "Invalid message: message must be a string",
                    ));
                }
                None => {
                    return Err(WireError::new(
                        "VALIDATION_ERROR",
                        "Invalid message: message is required",
                    ));
                }
            };
            Ok(Incoming::Step { message })
        }
        "state" => {
            reject_unknown(obj, &["type"])?;
            Ok(Incoming::State)
        }
        "close" => {
            reject_unknown(obj, &["type"])?;
            Ok(Incoming::Close)
        }
        other => Err(WireError::new(
            "UNKNOWN_TYPE",
            format!("Unknown message type: {other}"),
        )),
    }
}

pub enum Incoming {
    Reset {
        seed: Option<i64>,
        episode_id: Option<String>,
    },
    Step {
        message: String,
    },
    State,
    Close,
}

fn reject_unknown(obj: &Map<String, Value>, allowed: &[&str]) -> Result<(), WireError> {
    for key in obj.keys() {
        if !allowed.iter().any(|name| name == key) {
            return Err(WireError::new(
                "VALIDATION_ERROR",
                format!("Invalid message: unexpected field '{key}'"),
            ));
        }
    }
    Ok(())
}

fn object_field(
    obj: &Map<String, Value>,
    name: &str,
    default_empty: bool,
) -> Result<Map<String, Value>, WireError> {
    match obj.get(name) {
        None if default_empty => Ok(Map::new()),
        None => Err(WireError::new(
            "VALIDATION_ERROR",
            format!("Invalid message: missing {name}"),
        )),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err(WireError::new(
            "VALIDATION_ERROR",
            format!("Invalid message: {name} must be an object"),
        )),
    }
}

fn observation_frame(snap: &Snapshot, with_signals: bool) -> String {
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
    let mut data = Map::new();
    data.insert("observation".to_string(), observation);
    data.insert("reward".to_string(), reward_value(snap.last_reward_milli));
    data.insert("done".to_string(), Value::Bool(snap.core.done));
    data.insert("metadata".to_string(), metadata);
    frame("observation", Value::Object(data))
}

fn parse_seed(data: &Map<String, Value>) -> Result<Option<i64>, WireError> {
    match data.get("seed") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_i64().map(Some).ok_or_else(|| {
            WireError::new(
                "VALIDATION_ERROR",
                "Invalid message: seed must be an integer",
            )
        }),
        Some(_) => Err(WireError::new(
            "VALIDATION_ERROR",
            "Invalid message: seed must be an integer",
        )),
    }
}

fn parse_episode_id(data: &Map<String, Value>) -> Result<Option<String>, WireError> {
    match data.get("episode_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if id.len() <= MAX_EPISODE_ID_BYTES => Ok(Some(id.clone())),
        Some(Value::String(_)) => Err(WireError::new(
            "VALIDATION_ERROR",
            format!("Invalid message: episode_id longer than {MAX_EPISODE_ID_BYTES} bytes"),
        )),
        Some(_) => Err(WireError::new(
            "VALIDATION_ERROR",
            "Invalid message: episode_id must be a string",
        )),
    }
}

fn state_frame(snap: &Snapshot) -> String {
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
    frame(
        "state",
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
        }),
    )
}

fn reward_value(milli: i64) -> Value {
    match Number::from_f64(milli_f64(milli)) {
        Some(number) => Value::Number(number),
        None => Value::from(0),
    }
}

fn milli_f64(milli: i64) -> f64 {
    (milli as f64) / 1000.0
}

fn frame(ty: &str, data: Value) -> String {
    json!({ "type": ty, "data": data }).to_string()
}

pub fn metadata_document() -> Value {
    json!({
        "name": "wordle_v0",
        "description": "Classic Wordle on the OpenEnv TextArenaEnv WebSocket contract. Not the full TextArena suite, not the NLTK or New York Times word lists, and not CPython's RNG.",
        "version": env!("CARGO_PKG_VERSION"),
        "author": "h5i-dev",
        "documentation_url": "https://github.com/h5i-dev/senv"
    })
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
