//! senv is an OpenEnv-compatible reinforcement-learning environment server.
//!
//! Echo, Wordle, and the custom Arith-v0 task run reset, step, and state as
//! h5i-app kernels.
//! Each WebSocket session owns a [`h5i_app::MemoryEngine`]. The Axum process
//! is the trusted shell: it speaks the OpenEnv wire protocol and does not
//! decide what a transition means.
//!
//! The kernel proofs, when they exist, are about that pure transition. They
//! do not establish WebSocket isolation, session cleanup, or any other
//! property of this shell.

pub mod arith;
pub mod arith_env;
pub mod env;
pub mod kernel;
pub mod protocol;
pub mod server;
pub mod store;
pub mod wordle;
pub mod wordle_env;

/// `meta-pytorch/OpenEnv` commit the wire protocol and Echo behavior were
/// derived from. The Python compatibility test imports that tree unmodified.
pub const OPENENV_GIT_REVISION: &str = "58d0344011fe961342706c90288f7e842b15b1c9";

/// crates.io release of `h5i-app` that provides `Kernel`, `MemoryEngine` and `H5iApp`.
pub const H5I_APP_VERSION: &str = "0.5.5";

pub use server::{AppState, EnvKind, ServerConfig, serve};
