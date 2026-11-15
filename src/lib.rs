//! senv: serve h5i-app kernels as OpenEnv reinforcement-learning environments.
//!
//! Write the environment the way you write any h5i app. The kernel is a pure
//! `transition` from `(principal, snapshot, command)` to a write set and a
//! reply. It decides every reward and every `done`, and it is the part you
//! extract to Lean and prove things about. [`Environment`] is the trusted
//! shell around it: it turns OpenEnv requests into kernel commands and renders
//! committed snapshots back into OpenEnv JSON. [`run`] serves it, and the
//! unmodified OpenEnv Python clients connect to the result.
//!
//! Each OpenEnv session gets its own [`h5i_app::MemoryEngine`], so a property
//! proved about the kernel holds per session. The proof does not cover this
//! crate: socket handling, session isolation, and rendering are trusted code,
//! checked by tests.
//!
//! # Example
//!
//! A one-step bandit: the agent pulls arm 0 or 1, and arm 1 pays 1.
//!
//! ```
//! use senv::{Environment, Kernel, Metadata, Observation, Phase, Reward, TenantId, WireError};
//! use serde_json::{Map, Value, json};
//!
//! // The kernel. In a real project this lives in its own crate with no
//! // dependencies, so Aeneas can extract it to Lean.
//! #[derive(Clone, Default)]
//! pub struct Snap { pulls: u64, last: Option<u8>, done: bool }
//! pub enum Cmd { Reset, Pull(u8), State }
//! pub enum Error { Done }
//!
//! pub fn transition(_: &(), s: &Snap, c: &Cmd) -> Result<(Snap, ()), Error> {
//!     match c {
//!         Cmd::Reset => Ok((Snap::default(), ())),
//!         Cmd::Pull(_) if s.done => Err(Error::Done),
//!         Cmd::Pull(arm) => Ok((Snap { pulls: s.pulls + 1, last: Some(*arm), done: true }, ())),
//!         Cmd::State => Ok((s.clone(), ())),
//!     }
//! }
//!
//! pub fn reward_milli(s: &Snap) -> i64 {
//!     if s.last == Some(1) { 1000 } else { 0 }
//! }
//!
//! // The h5i-app binding.
//! pub struct Bandit;
//! impl Kernel for Bandit {
//!     type Principal = ();
//!     type Snapshot = Snap;
//!     type Command = Cmd;
//!     type WriteSet = Snap;
//!     type Reply = ();
//!     type Error = Error;
//!     fn tenant(_: &()) -> TenantId { TenantId(1) }
//!     fn transition(a: &(), s: &Snap, c: &Cmd) -> Result<(Snap, ()), Error> { transition(a, s, c) }
//!     fn apply(_: &Snap, w: &Snap) -> Snap { w.clone() }
//! }
//!
//! // The OpenEnv shell.
//! pub struct BanditEnv;
//! impl Environment for BanditEnv {
//!     type Kernel = Bandit;
//!     fn metadata(&self) -> Metadata { Metadata::new("bandit", "Two arms; arm 1 pays.") }
//!     fn principal(&self) {}
//!     fn reset(&self, _seed: Option<i64>, _episode_id: String) -> Cmd { Cmd::Reset }
//!     fn step(&self, action: &Map<String, Value>) -> Result<Cmd, WireError> {
//!         match action.get("arm").and_then(Value::as_u64) {
//!             Some(arm @ (0 | 1)) => Ok(Cmd::Pull(arm as u8)),
//!             _ => Err(WireError::validation("arm must be 0 or 1")),
//!         }
//!     }
//!     fn state(&self) -> Cmd { Cmd::State }
//!     fn observe(&self, _: Phase, s: &Snap, _: &()) -> Observation {
//!         Observation::new(json!({ "pulls": s.pulls }), Reward::Milli(reward_milli(s)), s.done)
//!     }
//!     fn render_state(&self, s: &Snap, _: &()) -> Value {
//!         json!({ "episode_id": null, "step_count": s.pulls })
//!     }
//!     fn refuse(&self, _: &Error) -> WireError { WireError::execution("episode is done") }
//! }
//!
//! // Drive it in-process, the same way the server does for each socket.
//! let session = senv::Session::open(std::sync::Arc::new(BanditEnv), "ep");
//! session.reset(None, None).unwrap();
//! let obs = session.step(json!({ "arm": 1 }).as_object().unwrap()).unwrap();
//! assert_eq!(obs.reward, Reward::Milli(1000));
//! assert!(obs.done);
//! assert!(session.step(json!({ "arm": 0 }).as_object().unwrap()).is_err());
//!
//! // Or serve it to OpenEnv clients:
//! // senv::run(BanditEnv, senv::ServeArgs::from_env_and_args()).await?;
//! ```
//!
//! The repository's `examples/` directory has three larger environments built
//! the same way: Echo (an MCP tool environment), Wordle (with a Lean model of
//! its kernel), and Arith (a multi-turn calculator task).

pub mod cli;
pub mod env;
pub mod server;
pub mod session;
pub mod wire;

/// `openenv/OpenEnv` commit the wire protocol was derived from. The Python
/// compatibility tests import that tree unmodified.
pub const OPENENV_GIT_REVISION: &str = "58d0344011fe961342706c90288f7e842b15b1c9";

pub use cli::{ServeArgs, run};
pub use env::{Environment, Metadata, Observation, Phase, Reward, Tool, ToolError, WireError};
pub use h5i_app::{Kernel, MemoryEngine, TenantId};
pub use server::{Server, ServerConfig};
pub use session::{Committed, Dispatch, Session, ToolCallError};
