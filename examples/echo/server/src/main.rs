//! `echo-env`: serve the Echo environment.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    senv::run(echo_env::Echo, senv::ServeArgs::from_env_and_args()).await
}
