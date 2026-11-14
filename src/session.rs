//! One episode stream: an environment's shell plus a private h5i engine.
//!
//! The server keeps one [`Session`] per OpenEnv session. Tests and in-process
//! rollouts can drive a [`Session`] directly, with no socket in between, and
//! still go through the same kernel and the same rendering.

use std::sync::{Arc, Mutex};

use h5i_app::{Kernel, MemoryEngine, TenantId};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::env::{Environment, Observation, Phase, ToolError, WireError};
use crate::wire::{self, Incoming};

type K<E> = <E as Environment>::Kernel;

/// The snapshot a command committed, and the kernel's reply to it.
pub type Committed<E> = (<K<E> as Kernel>::Snapshot, <K<E> as Kernel>::Reply);

/// A live session. Dropping it drops its engine and every snapshot in it.
pub struct Session<E: Environment> {
    env: Arc<E>,
    engine: MemoryEngine<K<E>>,
    principal: <K<E> as Kernel>::Principal,
    tenant: TenantId,
    // Serializes execute + snapshot so a render sees the state its own
    // command committed, even when an HTTP tool call races a socket step.
    turn: Mutex<()>,
}

/// What the socket loop should do with one text frame.
pub enum Dispatch {
    Reply(String),
    Close,
}

/// Why an MCP tool call did not produce a result.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolCallError {
    NotFound(String),
    Tool(ToolError),
    Refused(WireError),
}

impl<E: Environment> Session<E> {
    /// Open a session whose snapshot is `env.initial(episode_id)`.
    pub fn open(env: Arc<E>, episode_id: &str) -> Self {
        let principal = env.principal();
        let tenant = <K<E> as Kernel>::tenant(&principal);
        let engine = MemoryEngine::<K<E>>::default().with_snapshot(tenant, env.initial(episode_id));
        Self {
            env,
            engine,
            principal,
            tenant,
            turn: Mutex::new(()),
        }
    }

    pub fn environment(&self) -> &E {
        &self.env
    }

    /// The committed snapshot. Useful for checking a kernel property from a test.
    pub fn snapshot(&self) -> <K<E> as Kernel>::Snapshot {
        self.engine.snapshot(self.tenant)
    }

    /// Run one command and return the reply with the snapshot it committed.
    pub fn execute(&self, command: &<K<E> as Kernel>::Command) -> Result<Committed<E>, WireError> {
        let _turn = match self.turn.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };
        match self.engine.execute(&self.principal, command) {
            Ok(reply) => Ok((self.engine.snapshot(self.tenant), reply)),
            Err(error) => Err(self.env.refuse(&error)),
        }
    }

    /// OpenEnv `reset`. A missing episode id becomes a fresh UUID.
    pub fn reset(
        &self,
        seed: Option<i64>,
        episode_id: Option<String>,
    ) -> Result<Observation, WireError> {
        let episode_id = episode_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let command = self.env.reset(seed, episode_id);
        let (snapshot, reply) = self.execute(&command)?;
        Ok(self.env.observe(Phase::Reset, &snapshot, &reply))
    }

    /// OpenEnv `step` with the request's `data` object.
    pub fn step(&self, action: &Map<String, Value>) -> Result<Observation, WireError> {
        let command = self.env.step(action)?;
        let (snapshot, reply) = self.execute(&command)?;
        Ok(self.env.observe(Phase::Step, &snapshot, &reply))
    }

    /// OpenEnv `state`.
    pub fn state(&self) -> Result<Value, WireError> {
        let command = self.env.state();
        let (snapshot, reply) = self.execute(&command)?;
        Ok(self.env.render_state(&snapshot, &reply))
    }

    /// MCP `tools/call`.
    pub fn call_tool(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
    ) -> Result<Value, ToolCallError> {
        let command = self
            .env
            .call_tool(name, arguments)
            .ok_or_else(|| ToolCallError::NotFound(name.to_string()))?;
        let (snapshot, reply) = self.execute(&command).map_err(ToolCallError::Refused)?;
        self.env
            .tool_result(&snapshot, &reply)
            .map_err(ToolCallError::Tool)
    }

    /// Handle one WebSocket text frame.
    pub fn dispatch(&self, text: &str) -> Dispatch {
        let reply = match wire::parse_incoming(text) {
            Err(error) => Err(error),
            Ok(Incoming::Close) => return Dispatch::Close,
            Ok(Incoming::Reset { seed, episode_id }) => self
                .reset(seed, episode_id)
                .map(|observation| wire::observation_frame(&observation)),
            Ok(Incoming::Step(action)) => self
                .step(&action)
                .map(|observation| wire::observation_frame(&observation)),
            Ok(Incoming::State) => self.state().map(wire::state_frame),
        };
        Dispatch::Reply(match reply {
            Ok(frame) => frame,
            Err(error) => wire::error_frame(&error),
        })
    }
}
