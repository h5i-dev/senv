//! One Echo environment: an h5i-app engine plus the OpenEnv operations.
//!
//! Dropping the value drops the engine. The session table does that when the
//! WebSocket disconnects. Nothing in this module is shared across sessions.

use std::sync::Arc;

use h5i_app::http::H5iApp;
use h5i_app::{MemoryEngine, TenantId};

use crate::kernel::{
    self, Body, Command, EchoError, Outcome, Principal, Reply, SESSION_TENANT, Snapshot, StepAction,
};
use crate::store::{AnonAuth, EchoStore};

pub type EchoApp = H5iApp<kernel::EchoKernel, EchoStore, MemoryEngine<kernel::EchoKernel>>;

#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    #[error("step count overflowed")]
    StepCountOverflow,
    #[error("engine failed: {0}")]
    Engine(String),
    #[error("kernel returned an unexpected reply")]
    UnexpectedReply,
}

pub trait Environment {
    type Action;
    type Observation;
    type State;
    type Error;

    fn reset(
        &self,
        seed: Option<i64>,
        episode_id: String,
    ) -> Result<Self::Observation, Self::Error>;
    fn step(&self, action: Self::Action) -> Result<Self::Observation, Self::Error>;
    fn state(&self) -> Result<Self::State, Self::Error>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnvState {
    pub episode_id: String,
    pub step_count: u64,
}

/// A live Echo session. `Clone` shares the engine; it does not copy the episode.
#[derive(Clone)]
pub struct EchoEnvironment {
    app: EchoApp,
    principal: Principal,
}

impl EchoEnvironment {
    pub fn open(episode_id: String) -> Self {
        let snap = Snapshot {
            episode_id,
            step_count: 0,
            reset_count: 0,
            seed: None,
        };
        let engine = Arc::new(
            MemoryEngine::<kernel::EchoKernel>::default()
                .with_snapshot(TenantId(SESSION_TENANT), snap),
        );
        let app = H5iApp::<kernel::EchoKernel, EchoStore, MemoryEngine<kernel::EchoKernel>>::with_executor(
            engine,
            AnonAuth,
        );
        Self {
            app,
            principal: Principal {
                tenant: SESSION_TENANT,
            },
        }
    }

    fn execute(&self, cmd: Command) -> Result<Reply, EnvError> {
        // Inherent `MemoryEngine::execute`: transition, then apply, under the
        // engine lock. `H5iApp` is what hands the engine out; the OpenEnv
        // socket is not an h5i-app HTTP route because that route cannot write
        // the float rewards the Echo client requires.
        match self.app.engine().execute(&self.principal, &cmd) {
            Ok(reply) => Ok(reply),
            Err(EchoError::StepCountOverflow) => Err(EnvError::StepCountOverflow),
        }
    }

    fn outcome(&self, cmd: Command) -> Result<Outcome, EnvError> {
        match self.execute(cmd)? {
            Reply::Outcome(outcome) => Ok(outcome),
            Reply::State { .. } => Err(EnvError::UnexpectedReply),
        }
    }

    /// HTTP MCP tool calls. They do not increment `step_count`.
    pub fn invoke(&self, action: StepAction) -> Result<Outcome, EnvError> {
        self.outcome(Command::Invoke(action))
    }
}

impl Environment for EchoEnvironment {
    type Action = StepAction;
    type Observation = Outcome;
    type State = EnvState;
    type Error = EnvError;

    fn reset(&self, seed: Option<i64>, episode_id: String) -> Result<Outcome, EnvError> {
        self.outcome(Command::Reset { seed, episode_id })
    }

    fn step(&self, action: StepAction) -> Result<Outcome, EnvError> {
        self.outcome(Command::Step(action))
    }

    fn state(&self) -> Result<EnvState, EnvError> {
        match self.execute(Command::State)? {
            Reply::State {
                episode_id,
                step_count,
            } => Ok(EnvState {
                episode_id,
                step_count,
            }),
            Reply::Outcome(_) => Err(EnvError::UnexpectedReply),
        }
    }
}

/// Used by the wire encoder and by tests that match on a body.
pub fn body(outcome: &Outcome) -> &Body {
    &outcome.body
}
