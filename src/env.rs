//! The contract between an h5i kernel and the OpenEnv wire.
//!
//! An environment is an ordinary h5i-app [`Kernel`]: a pure `transition` from
//! `(principal, snapshot, command)` to a write set and a reply. The kernel
//! decides every reward and every `done`. [`Environment`] is the trusted shell
//! around it. It turns OpenEnv requests into kernel commands and renders the
//! committed snapshot back into OpenEnv JSON. It must not decide outcomes.
//!
//! senv runs one [`h5i_app::MemoryEngine`] per session, so a property you prove
//! about the kernel holds for every session independently.

use h5i_app::Kernel;
use serde_json::{Map, Value};

/// The trusted shell around one kernel.
///
/// `&self` is the environment's configuration (for example a difficulty
/// level). It is shared by every session and must not hold episode state:
/// episode state lives in the kernel's snapshot.
pub trait Environment: Send + Sync + 'static {
    /// The pure transition this environment executes.
    type Kernel: Kernel;

    /// `GET /metadata`. OpenEnv clients read `name` and `description`.
    fn metadata(&self) -> Metadata;

    /// `GET /schema`: JSON Schemas of the action, observation, and state.
    fn schema(&self) -> Value {
        Value::Object(Map::new())
    }

    /// The principal every command of a session runs as. Sessions are isolated
    /// by having separate engines, not by this value.
    fn principal(&self) -> <Self::Kernel as Kernel>::Principal;

    /// The snapshot a session holds before its first `reset`.
    fn initial(&self, episode_id: &str) -> <Self::Kernel as Kernel>::Snapshot {
        let _ = episode_id;
        Default::default()
    }

    /// The command for a `reset` request. `episode_id` is the client's id, or
    /// a fresh UUID when the client sent none. Deriving a task from the seed
    /// here is fine as long as the derivation is deterministic.
    fn reset(&self, seed: Option<i64>, episode_id: String) -> <Self::Kernel as Kernel>::Command;

    /// Parse the `data` object of a `step` request.
    fn step(
        &self,
        action: &Map<String, Value>,
    ) -> Result<<Self::Kernel as Kernel>::Command, WireError>;

    /// The command for a `state` request. It should be read-only.
    fn state(&self) -> <Self::Kernel as Kernel>::Command;

    /// Render a committed `reset` or `step`. Take the reward and `done` from
    /// the reply or the snapshot the kernel produced; do not compute them here.
    fn observe(
        &self,
        phase: Phase,
        snapshot: &<Self::Kernel as Kernel>::Snapshot,
        reply: &<Self::Kernel as Kernel>::Reply,
    ) -> Observation;

    /// Render a `state` reply. OpenEnv clients expect `episode_id` and `step_count`.
    fn render_state(
        &self,
        snapshot: &<Self::Kernel as Kernel>::Snapshot,
        reply: &<Self::Kernel as Kernel>::Reply,
    ) -> Value;

    /// The wire error for a refused command. Nothing was committed.
    fn refuse(&self, error: &<Self::Kernel as Kernel>::Error) -> WireError;

    /// MCP tools served on `POST /mcp` (`tools/list`). Empty by default.
    fn tools(&self) -> Vec<Tool> {
        Vec::new()
    }

    /// The command for an MCP `tools/call`. `None` means the tool does not
    /// exist. Upstream OpenEnv does not count these calls as steps, so the
    /// command should leave the step count alone.
    fn call_tool(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
    ) -> Option<<Self::Kernel as Kernel>::Command> {
        let _ = (name, arguments);
        None
    }

    /// Render the result of an MCP tool call.
    fn tool_result(
        &self,
        snapshot: &<Self::Kernel as Kernel>::Snapshot,
        reply: &<Self::Kernel as Kernel>::Reply,
    ) -> Result<Value, ToolError> {
        let _ = (snapshot, reply);
        Err(ToolError::Execution("this environment has no tools".into()))
    }
}

/// Which request an observation answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Reset,
    Step,
}

/// One OpenEnv observation frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    /// The environment-specific observation object.
    pub observation: Value,
    pub reward: Reward,
    pub done: bool,
    /// Optional top-level `metadata`.
    pub metadata: Option<Value>,
}

impl Observation {
    pub fn new(observation: Value, reward: Reward, done: bool) -> Self {
        Self {
            observation,
            reward,
            done,
            metadata: None,
        }
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A reward as the kernel stated it.
///
/// Kernels usually avoid floats, so [`Reward::Milli`] carries thousandths and
/// is converted only on the wire. [`Reward::None`] is JSON `null`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reward {
    None,
    Milli(i64),
    Float(f64),
}

impl Reward {
    pub fn as_f64(self) -> Option<f64> {
        match self {
            Self::None => None,
            Self::Milli(milli) => Some(milli as f64 / 1000.0),
            Self::Float(value) => Some(value),
        }
    }
}

/// An OpenEnv `error` frame: `{"type":"error","data":{"code":..,"message":..}}`.
#[derive(Clone, Debug, PartialEq)]
pub struct WireError {
    pub code: &'static str,
    pub message: String,
    pub extra: Vec<(&'static str, Value)>,
}

impl WireError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            extra: Vec::new(),
        }
    }

    /// A malformed or out-of-contract request.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new("VALIDATION_ERROR", message)
    }

    /// The kernel refused a well-formed command.
    pub fn execution(message: impl Into<String>) -> Self {
        Self::new("EXECUTION_ERROR", message)
    }

    pub fn with_extra(mut self, extra: Vec<(&'static str, Value)>) -> Self {
        self.extra = extra;
        self
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}

/// `GET /metadata`.
#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: Option<String>,
    pub documentation_url: Option<String>,
}

impl Metadata {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            author: None,
            documentation_url: None,
        }
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    pub fn documentation_url(mut self, url: impl Into<String>) -> Self {
        self.documentation_url = Some(url.into());
        self
    }

    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("name".into(), Value::String(self.name.clone()));
        map.insert(
            "description".into(),
            Value::String(self.description.clone()),
        );
        map.insert("version".into(), Value::String(self.version.clone()));
        if let Some(author) = &self.author {
            map.insert("author".into(), Value::String(author.clone()));
        }
        if let Some(url) = &self.documentation_url {
            map.insert("documentation_url".into(), Value::String(url.clone()));
        }
        Value::Object(map)
    }
}

/// One MCP tool for `tools/list`.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// Why an MCP tool call failed. Maps to JSON-RPC -32602 or -32603.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolError {
    InvalidArgs(String),
    Execution(String),
}
