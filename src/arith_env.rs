//! Arith-v0 session on its own `MemoryEngine`.
//!
//! The shell builds the problem before `transition` and renders the
//! TextArena-shaped JSON after the engine commits. `Problem::value` stays in
//! the snapshot so the kernel can score `answer`. The observation and the
//! state frame do not include it. This is a custom senv task, not an upstream
//! environment.

use std::sync::Arc;

use axum::http::HeaderMap;
use h5i_app::http::{AuthError, Authenticator, H5iApp};
use h5i_app::pg::{DbError, Store, Tx};
use h5i_app::{Kernel, MemoryEngine, TenantId};
use serde_json::{Map, Number, Value, json};

use crate::arith::{self, Core, Problem, fresh, parse_action, prompt_text, step_core};
use crate::kernel::{Principal, SESSION_TENANT};
use crate::protocol::{MAX_EPISODE_ID_BYTES, WireError};

pub struct ArithKernel;

#[derive(Clone, Debug, PartialEq, Eq)]
struct LogEntry {
    player: String,
    reply: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub episode_id: String,
    pub step_count: u64,
    pub seed: Option<i64>,
    pub ops: u8,
    pub core: Core,
    log: Vec<LogEntry>,
    pub last_reward_milli: i64,
}

impl Default for Snapshot {
    fn default() -> Self {
        let ops = arith::DEFAULT_OPS;
        Self {
            episode_id: String::new(),
            step_count: 0,
            seed: None,
            ops,
            core: fresh(arith::generate(0, ops)),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArithError {
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
) -> Result<(Snapshot, Reply), ArithError> {
    match cmd {
        Command::Reset {
            seed,
            episode_id,
            problem,
        } => {
            let ops = u8::try_from(problem.ops.len()).unwrap_or(arith::DEFAULT_OPS);
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
            if message.len() > arith::MAX_ACTION_BYTES {
                return Err(ArithError::ActionTooLong);
            }
            let action = parse_action(message);
            let core = step_core(&snap.core, action).map_err(|error| match error {
                arith::CoreError::AlreadyDone => ArithError::AlreadyDone,
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

pub struct ArithStore;

impl Kernel for ArithKernel {
    type Principal = Principal;
    type Snapshot = Snapshot;
    type Command = Command;
    type WriteSet = Snapshot;
    type Reply = Reply;
    type Error = ArithError;

    fn tenant(actor: &Principal) -> TenantId {
        TenantId(actor.tenant)
    }

    fn transition(
        actor: &Principal,
        snap: &Snapshot,
        cmd: &Command,
    ) -> Result<(Snapshot, Reply), ArithError> {
        transition(actor, snap, cmd)
    }

    fn apply(snap: &Snapshot, ws: &Snapshot) -> Snapshot {
        apply(snap, ws)
    }

    fn read_only(cmd: &Command) -> bool {
        matches!(cmd, Command::State)
    }
}

impl Store<ArithKernel> for ArithStore {
    fn ddl() -> Vec<String> {
        Vec::new()
    }

    fn tables() -> Vec<&'static str> {
        Vec::new()
    }

    async fn load(_tx: &Tx<'_>, _tenant: TenantId) -> Result<Snapshot, DbError> {
        Err(DbError::Decode(
            "arith sessions execute on MemoryEngine; there is no PostgreSQL snapshot".into(),
        ))
    }

    async fn write(_tx: &Tx<'_>, _tenant: TenantId, _ws: &Snapshot) -> Result<(), DbError> {
        Err(DbError::Decode(
            "arith sessions execute on MemoryEngine; there is no PostgreSQL write".into(),
        ))
    }
}

pub struct ArithAuth;

impl Authenticator<ArithKernel> for ArithAuth {
    fn authenticate(&self, _headers: &HeaderMap) -> Result<Principal, AuthError> {
        Ok(Principal {
            tenant: SESSION_TENANT,
        })
    }
}

type ArithApp = H5iApp<ArithKernel, ArithStore, MemoryEngine<ArithKernel>>;

#[derive(Clone)]
pub struct ArithEnvironment {
    app: ArithApp,
    principal: Principal,
    ops: u8,
}

impl ArithEnvironment {
    pub fn open(episode_id: String, ops: u8) -> Self {
        let ops = arith::normalize_ops(ops);
        let problem = arith::generate(arith::episode_seed(&episode_id), ops);
        let snap = Snapshot {
            episode_id,
            step_count: 0,
            seed: None,
            ops,
            core: fresh(problem),
            log: Vec::new(),
            last_reward_milli: 0,
        };
        Self::from_snapshot(snap, ops)
    }

    fn from_snapshot(snap: Snapshot, ops: u8) -> Self {
        let engine = Arc::new(
            MemoryEngine::<ArithKernel>::default().with_snapshot(TenantId(SESSION_TENANT), snap),
        );
        let app = H5iApp::<ArithKernel, ArithStore, MemoryEngine<ArithKernel>>::with_executor(
            engine, ArithAuth,
        );
        Self {
            app,
            principal: Principal {
                tenant: SESSION_TENANT,
            },
            ops,
        }
    }

    fn execute(&self, cmd: Command) -> Result<Reply, ArithError> {
        self.app.engine().execute(&self.principal, &cmd)
    }

    fn snapshot(&self) -> Snapshot {
        self.app.engine().snapshot(TenantId(SESSION_TENANT))
    }

    pub fn reset(&self, seed: Option<i64>, episode_id: String) -> Result<String, ArithError> {
        let problem_seed = seed.unwrap_or_else(|| arith::episode_seed(&episode_id));
        let problem = arith::generate(problem_seed, self.ops);
        self.execute(Command::Reset {
            seed,
            episode_id,
            problem,
        })?;
        Ok(observation_frame(&self.snapshot()))
    }

    pub fn step(&self, message: String) -> Result<String, ArithError> {
        self.execute(Command::Step { message })?;
        Ok(observation_frame(&self.snapshot()))
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

fn observation_frame(snap: &Snapshot) -> String {
    let expr = arith::expression(&snap.core.problem);
    let prompt = prompt_text(&expr);
    let message_values: Vec<Value> = snap
        .log
        .iter()
        .flat_map(|entry| {
            [
                json!({
                    "sender_id": 0,
                    "content": entry.player,
                    "category": "PLAYER_ACTION",
                }),
                json!({
                    "sender_id": -1,
                    "content": entry.reply,
                    "category": "GAME_MESSAGE",
                }),
            ]
        })
        .collect();
    let metadata = json!({
        "env_id": arith::ENV_ID,
        "ops": snap.ops,
        "turn": snap.core.steps,
    });
    let mut info = Map::new();
    if snap.core.steps > 0 {
        info.insert(
            "arith.format_valid".to_string(),
            Value::Bool(snap.core.last_format_valid),
        );
        info.insert("arith.success".to_string(), Value::Bool(snap.core.won));
    }
    let observation = json!({
        "prompt": prompt,
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

fn state_frame(snap: &Snapshot) -> String {
    let mut last_info = Map::new();
    if snap.core.steps > 0 {
        last_info.insert(
            "arith.format_valid".to_string(),
            Value::Bool(snap.core.last_format_valid),
        );
        last_info.insert("arith.success".to_string(), Value::Bool(snap.core.won));
    }
    frame(
        "state",
        json!({
            "episode_id": snap.episode_id,
            "step_count": snap.step_count,
            "env_id": arith::ENV_ID,
            "num_players": 1,
            "max_turns": arith::MAX_STEPS,
            "turn": snap.core.steps,
            "last_reward": milli_f64(snap.last_reward_milli),
            "last_info": last_info,
            "raw_state": {
                "expression": arith::expression(&snap.core.problem),
                "ops": snap.ops,
                "steps": snap.core.steps,
                "max_steps": arith::MAX_STEPS,
                "done": snap.core.done,
                "last_reply": snap.core.last_public,
            }
        }),
    )
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
        "name": "arith_v0",
        "description": "Custom senv arithmetic task on the TextArenaEnv WebSocket contract. Not an upstream OpenEnv or TextArena environment. The expression value is scored inside the server and is not part of the observation or state.",
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
                "message": { "type": "string", "maxLength": arith::MAX_ACTION_BYTES }
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
                "env_id": { "const": arith::ENV_ID },
                "num_players": { "const": 1 },
                "turn": { "type": "integer", "minimum": 0 }
            },
            "required": ["step_count", "env_id"]
        }
    })
}
