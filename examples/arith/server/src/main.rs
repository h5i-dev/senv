//! `arith-env`: serve Arith-v0.

use clap::Parser;

#[derive(Parser)]
#[command(about = "Serve Arith-v0, a multi-turn calculator task, as an OpenEnv environment")]
struct Args {
    #[command(flatten)]
    serve: senv::ServeArgs,

    /// Operators per expression: 2, 3, or 4.
    #[arg(long, env = "SENV_ARITH_OPS", default_value_t = 3)]
    ops: u8,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if !(2..=4).contains(&args.ops) {
        return Err(format!("--ops must be 2, 3, or 4 (got {})", args.ops).into());
    }
    senv::run(arith_env::Arith::new(args.ops), args.serve).await
}
