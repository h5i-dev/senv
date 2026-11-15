//! Command-line flags and a `main` helper for environment binaries.
//!
//! ```ignore
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     senv::run(MyEnv::default(), senv::ServeArgs::from_env_and_args()).await
//! }
//! ```
//!
//! An environment with its own flags flattens [`ServeArgs`] into its parser:
//!
//! ```ignore
//! #[derive(clap::Parser)]
//! struct Args {
//!     #[command(flatten)]
//!     serve: senv::ServeArgs,
//!     #[arg(long, default_value_t = 3)]
//!     ops: u8,
//! }
//! ```

use std::time::Duration;

use clap::{Args, Parser};

use crate::env::Environment;
use crate::server::{Server, ServerConfig};

/// Flags every senv environment binary accepts.
#[derive(Args, Clone, Debug)]
pub struct ServeArgs {
    /// Address to listen on.
    #[arg(long, env = "SENV_BIND", default_value = "127.0.0.1:8000")]
    pub bind: String,

    /// Maximum live sessions, including sockets and not-yet-attached HTTP sessions.
    #[arg(long, env = "MAX_CONCURRENT_ENVS", default_value_t = 8)]
    pub max_sessions: usize,

    /// Largest accepted WebSocket message, in bytes.
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    pub max_message_bytes: usize,

    /// Drop an unattached session after this many seconds. 0 keeps it until close.
    #[arg(long, default_value_t = 120)]
    pub idle_timeout_secs: u64,
}

#[derive(Parser)]
#[command(about = "Serve an OpenEnv environment")]
struct OnlyServe {
    #[command(flatten)]
    serve: ServeArgs,
}

impl ServeArgs {
    /// Parse `std::env::args()` when the binary has no flags of its own.
    pub fn from_env_and_args() -> Self {
        OnlyServe::parse().serve
    }

    pub fn config(&self) -> Result<ServerConfig, String> {
        if self.max_sessions == 0 {
            return Err("max sessions must be at least 1".into());
        }
        if self.max_message_bytes == 0 {
            return Err("max message bytes must be at least 1".into());
        }
        Ok(ServerConfig {
            max_sessions: self.max_sessions,
            max_message_bytes: self.max_message_bytes,
            idle_timeout: (self.idle_timeout_secs > 0)
                .then(|| Duration::from_secs(self.idle_timeout_secs)),
        })
    }
}

/// Initialize logging from `RUST_LOG`, bind, and serve `env` until Ctrl-C.
pub async fn run<E: Environment>(
    env: E,
    args: ServeArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let config = args.config()?;
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    let local = listener.local_addr()?;
    let metadata = env.metadata();
    tracing::info!(
        %local,
        env = %metadata.name,
        openenv = crate::OPENENV_GIT_REVISION,
        max_sessions = config.max_sessions,
        "environment listening"
    );
    Server::new(env, config).serve(listener).await?;
    Ok(())
}
