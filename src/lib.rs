//! senv is an OpenEnv-compatible reinforcement-learning environment server.
//!
//! The Echo environment's reset, step and state transitions are an h5i-app
//! kernel. Each WebSocket session owns a [`h5i_app::MemoryEngine`]. The Axum
//! process is the trusted shell: it speaks the OpenEnv wire protocol and
//! does not decide what a transition means.
//!
//! The kernel proofs, when they exist, are about that pure transition. They
//! do not establish WebSocket isolation, session cleanup, or any other
//! property of this shell.

pub mod env;
pub mod kernel;
pub mod protocol;
pub mod server;
pub mod store;

/// `meta-pytorch/OpenEnv` commit the wire protocol and Echo behavior were
/// derived from. The Python compatibility test imports that tree unmodified.
pub const OPENENV_GIT_REVISION: &str = "58d0344011fe961342706c90288f7e842b15b1c9";

/// `h5i-dev/h5i` commit that provides `Kernel`, `MemoryEngine` and `H5iApp`.
pub const H5I_GIT_REVISION: &str = "5e0f8a929c57bb12a321090d98a14b3a2714b849";

pub use server::{AppState, ServerConfig, serve};
