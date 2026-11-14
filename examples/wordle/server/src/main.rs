//! `wordle-env`: serve Wordle-v0.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    senv::run(wordle_env::Wordle, senv::ServeArgs::from_env_and_args()).await
}
