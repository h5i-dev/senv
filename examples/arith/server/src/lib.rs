//! Arith-v0 as a senv environment, on the OpenEnv `TextArenaEnv` WebSocket contract.
//!
//! A custom multi-turn tool-use task, not an upstream OpenEnv environment. The
//! kernel (`arith-kernel`) scores `answer` against the expression value it
//! keeps in the snapshot. This shell derives the problem from the seed and
//! renders TextArena-shaped JSON. Neither the observation nor `state` contains
//! the value.

use arith_kernel::episode::{ArithError, Command, Principal, Reply, SESSION_TENANT, Snapshot};
use arith_kernel::{self as arith, fresh, prompt_text};
use senv::wire::{reject_unknown, string_field};
use senv::{Environment, Kernel, Metadata, Observation, Phase, Reward, TenantId, WireError};
use serde_json::{Map, Value, json};

/// The h5i-app binding of the Arith kernel.
pub struct ArithKernel;

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
        arith_kernel::episode::transition(actor, snap, cmd)
    }

    fn apply(snap: &Snapshot, ws: &Snapshot) -> Snapshot {
        arith_kernel::episode::apply(snap, ws)
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
            ops: arith::normalize_ops(ops),
        }
    }

    pub fn ops(&self) -> u8 {
        self.ops
    }
}

impl Default for Arith {
    fn default() -> Self {
        Self::new(arith::DEFAULT_OPS)
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

    fn principal(&self) -> Principal {
        Principal {
            tenant: SESSION_TENANT,
        }
    }

    fn initial(&self, episode_id: &str) -> Snapshot {
        Snapshot {
            episode_id: episode_id.to_string(),
            ops: self.ops,
            core: fresh(arith::generate(arith::episode_seed(episode_id), self.ops)),
            ..Snapshot::default()
        }
    }

    fn reset(&self, seed: Option<i64>, episode_id: String) -> Command {
        let problem_seed = seed.unwrap_or_else(|| arith::episode_seed(&episode_id));
        Command::Reset {
            seed,
            problem: arith::generate(problem_seed, self.ops),
            episode_id,
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

    fn observe(&self, _phase: Phase, snapshot: &Snapshot, _reply: &Reply) -> Observation {
        observation(snapshot)
    }

    fn render_state(&self, snapshot: &Snapshot, _reply: &Reply) -> Value {
        state(snapshot)
    }

    fn refuse(&self, error: &ArithError) -> WireError {
        match error {
            ArithError::ActionTooLong => {
                WireError::validation("action message is longer than 4096 bytes")
            }
            ArithError::AlreadyDone => WireError::execution("episode is done"),
            ArithError::StepCountOverflow => WireError::execution("step count overflowed"),
        }
    }
}

fn observation(snap: &Snapshot) -> Observation {
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
    Observation::new(
        observation,
        Reward::Milli(snap.last_reward_milli),
        snap.core.done,
    )
    .with_metadata(metadata)
}

fn state(snap: &Snapshot) -> Value {
    let mut last_info = Map::new();
    if snap.core.steps > 0 {
        last_info.insert(
            "arith.format_valid".to_string(),
            Value::Bool(snap.core.last_format_valid),
        );
        last_info.insert("arith.success".to_string(), Value::Bool(snap.core.won));
    }
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
