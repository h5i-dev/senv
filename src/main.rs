//! `senv serve` — the Echo environment on the OpenEnv WebSocket protocol.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use senv::{AppState, EnvKind, H5I_GIT_REVISION, OPENENV_GIT_REVISION, ServerConfig, serve};

#[derive(Parser)]
#[command(
    name = "senv",
    version,
    about = "OpenEnv-compatible RL environments in Rust, executed on h5i-app."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve an OpenEnv environment.
    Serve {
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:8000", env = "SENV_BIND")]
        bind: String,
        /// `echo` (default) or `wordle`.
        #[arg(long, default_value = "echo", env = "SENV_ENV")]
        env: String,
        /// Maximum live sessions, including sockets and not-yet-attached HTTP sessions.
        #[arg(long, default_value_t = 8, env = "MAX_CONCURRENT_ENVS")]
        max_sessions: usize,
        /// Largest accepted WebSocket message, in bytes.
        #[arg(long, default_value_t = 16 * 1024 * 1024)]
        max_message_bytes: usize,
        /// Drop an unattached session after this many seconds. 0 keeps it until close.
        #[arg(long, default_value_t = 120)]
        idle_timeout_secs: u64,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("senv=info")),
        )
        .init();
    match Cli::parse().command {
        Command::Serve {
            bind,
            env,
            max_sessions,
            max_message_bytes,
            idle_timeout_secs,
        } => {
            if let Err(error) = run_serve(
                bind,
                env,
                max_sessions,
                max_message_bytes,
                idle_timeout_secs,
            ) {
                eprintln!("senv: {error}");
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

fn run_serve(
    bind: String,
    env: String,
    max_sessions: usize,
    max_message_bytes: usize,
    idle_timeout_secs: u64,
) -> Result<(), String> {
    if max_sessions == 0 {
        return Err("max sessions must be at least 1".into());
    }
    if max_message_bytes == 0 {
        return Err("max message size must be at least 1 byte".into());
    }
    let kind = match env.as_str() {
        "echo" => EnvKind::Echo,
        "wordle" => EnvKind::Wordle,
        other => {
            return Err(format!(
                "unknown environment '{other}', expected echo or wordle"
            ));
        }
    };
    let idle_timeout = if idle_timeout_secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(idle_timeout_secs))
    };
    let config = ServerConfig {
        max_sessions,
        max_message_bytes,
        idle_timeout,
        kind,
    };
    let state = AppState::new(config);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .map_err(|error| error.to_string())?;
        let local = listener.local_addr().map_err(|error| error.to_string())?;
        tracing::info!(
            %local,
            openenv = OPENENV_GIT_REVISION,
            h5i = H5I_GIT_REVISION,
            env = kind.as_str(),
            max_sessions,
            "environment listening"
        );
        serve(listener, state)
            .await
            .map_err(|error| error.to_string())
    })
}
