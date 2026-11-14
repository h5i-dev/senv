# senv

<p align="left">
  <a href="https://github.com/h5i-dev/senv/actions/workflows/ci.yaml"><img alt="CI" src="https://github.com/h5i-dev/senv/actions/workflows/ci.yaml/badge.svg"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/License-Apache%202.0-blue.svg"></a>
</p>

senv is a Rust library for building reinforcement-learning environments as [h5i](https://github.com/h5i-dev/h5i) apps and serving them to [OpenEnv](https://github.com/openenv/OpenEnv) clients. You write the environment's rules as an h5i-app kernel, a pure function that decides every reward and every `done`, and you can prove properties of that function in Lean 4 with h5i's tooling. senv adds the OpenEnv wire protocol, so the unmodified OpenEnv Python clients (`EchoEnv`, `TextArenaEnv`, or your own `EnvClient`) can train against it.

- The kernel is a normal h5i-app `Kernel`. The same code can sit behind senv for training and behind h5i's own HTTP router or tests anywhere else.
- Each OpenEnv session runs on its own `h5i_app::MemoryEngine`, so a property proved about the kernel holds for every session independently.
- `senv::Session` runs the same kernel and rendering in-process, without a socket, for tests and for rollouts that do not need the network.
- One process holds many sessions, so a trainer can run a whole GRPO group against one server.

> [!NOTE]
> senv is early. Versions before 0.3 were a single binary with three built-in environments, and before that a sandboxed Python environment manager. Those are not the library described here.

## 1. Install

```toml
[dependencies]
senv = { git = "https://github.com/h5i-dev/senv" }
h5i-app = "0.5.5"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
serde_json = "1"
```

Rust 1.88 or newer.

## 2. Write an environment

An environment is two crates, in the same layout as an h5i app.

The kernel crate holds the rules and nothing else, so `h5i app extract` can translate it to Lean:

```rust
// kernel/src/lib.rs
#[derive(Clone, Default)]
pub struct Snapshot { pub pulls: u64, pub last: Option<u8>, pub done: bool }
pub enum Command { Reset, Pull(u8), State }
pub enum Error { Done }

pub fn transition(_: &(), s: &Snapshot, c: &Command) -> Result<(Snapshot, ()), Error> {
    match c {
        Command::Reset => Ok((Snapshot::default(), ())),
        Command::Pull(_) if s.done => Err(Error::Done),
        Command::Pull(arm) => Ok((Snapshot { pulls: s.pulls + 1, last: Some(*arm), done: true }, ())),
        Command::State => Ok((s.clone(), ())),
    }
}

pub fn reward_milli(s: &Snapshot) -> i64 {
    if s.last == Some(1) { 1000 } else { 0 }
}
```

The server crate binds the kernel to h5i-app and implements `senv::Environment`, which parses OpenEnv requests into commands and renders committed snapshots. It reads the reward and `done` from the kernel's output and does not compute them:

```rust
// server/src/main.rs
use bandit_kernel as k;
use senv::{Environment, Kernel, Metadata, Observation, Phase, Reward, TenantId, WireError};
use serde_json::{Map, Value, json};

pub struct Bandit;
impl Kernel for Bandit {
    type Principal = ();
    type Snapshot = k::Snapshot;
    type Command = k::Command;
    type WriteSet = k::Snapshot;
    type Reply = ();
    type Error = k::Error;
    fn tenant(_: &()) -> TenantId { TenantId(1) }
    fn transition(a: &(), s: &k::Snapshot, c: &k::Command) -> Result<(k::Snapshot, ()), k::Error> {
        k::transition(a, s, c)
    }
    fn apply(_: &k::Snapshot, w: &k::Snapshot) -> k::Snapshot { w.clone() }
}

pub struct BanditEnv;
impl Environment for BanditEnv {
    type Kernel = Bandit;
    fn metadata(&self) -> Metadata { Metadata::new("bandit", "Two arms; arm 1 pays.") }
    fn principal(&self) {}
    fn reset(&self, _seed: Option<i64>, _episode_id: String) -> k::Command { k::Command::Reset }
    fn step(&self, action: &Map<String, Value>) -> Result<k::Command, WireError> {
        match action.get("arm").and_then(Value::as_u64) {
            Some(arm @ (0 | 1)) => Ok(k::Command::Pull(arm as u8)),
            _ => Err(WireError::validation("arm must be 0 or 1")),
        }
    }
    fn state(&self) -> k::Command { k::Command::State }
    fn observe(&self, _: Phase, s: &k::Snapshot, _: &()) -> Observation {
        Observation::new(json!({ "pulls": s.pulls }), Reward::Milli(k::reward_milli(s)), s.done)
    }
    fn render_state(&self, s: &k::Snapshot, _: &()) -> Value {
        json!({ "episode_id": null, "step_count": s.pulls })
    }
    fn refuse(&self, _: &k::Error) -> WireError { WireError::execution("episode is done") }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    senv::run(BanditEnv, senv::ServeArgs::from_env_and_args()).await
}
```

`cargo run` now listens on `127.0.0.1:8000`. Every senv binary accepts `--bind`, `--max-sessions`, `--max-message-bytes`, and `--idle-timeout-secs`. An environment with its own flags flattens `senv::ServeArgs` into its `clap` parser, as [`examples/arith`](examples/arith/server/src/main.rs) does for `--ops`.

`Environment` has optional methods too: `schema` for `GET /schema`, `initial` for the snapshot before the first reset, and `tools`, `call_tool`, and `tool_result` for MCP tools on `POST /mcp`, which [`examples/echo`](examples/echo/server/src/lib.rs) uses.

## 3. Connect from Python

The client side is OpenEnv's own. A new environment ships a small `EnvClient` subclass that knows its action and observation shapes, the way upstream environments do. The examples speak the existing `TextArenaEnv` and `EchoEnv` contracts, so those clients connect unmodified:

```bash
cargo run -p wordle-env -- --bind 127.0.0.1:8000
```

```python
from textarena_env import TextArenaEnv
from textarena_env.models import TextArenaAction

with TextArenaEnv(base_url="http://127.0.0.1:8000").sync() as env:
    result = env.reset(seed=7)
    result = env.step(TextArenaAction(message="[crane]"))
    print(result.observation.messages[-1].content)  # green / yellow feedback
    print(result.reward, result.done)
```

senv is tested against OpenEnv commit [`58d0344`](https://github.com/openenv/OpenEnv/tree/58d0344011fe961342706c90288f7e842b15b1c9).

## 4. Train an agent

A rollout is one client session. To run a GRPO group, start the server with enough `--max-sessions` and open one client per rollout. The same seed gives the same task, so a group can share one problem while its rollouts stay independent. [`scripts/arith_grpo.py`](scripts/arith_grpo.py) is a complete example: GRPO with LoRA on the Arith example, with generation batched across 64 sessions.

When the trainer is in Rust, or in tests, skip the network and drive a `Session` directly:

```rust
let session = senv::Session::open(std::sync::Arc::new(BanditEnv), "episode-0");
session.reset(Some(0), None)?;
let obs = session.step(json!({ "arm": 1 }).as_object().unwrap())?;
assert_eq!(obs.reward, senv::Reward::Milli(1000));
```

## 5. Prove properties

The kernel crate is where proofs apply. With h5i's CLI, `h5i app extract` translates a kernel written in the Aeneas subset (no `String`, `while` loops over indices) to Lean, and `h5i app prove` checks the theorems you state about it. h5i's [calculator tutorial](https://github.com/h5i-dev/h5i/blob/main/examples/app/calculator/TUTORIAL.md) walks through the layout.

Useful properties for an RL environment are about the reward and the observation: the reward is 1 only for a correct answer, an episode ends within N steps, the hidden answer never appears in what the agent can see. A proof covers the kernel only. Socket handling, session isolation, and the rendering in `observe` are trusted code, checked by senv's tests.

[`examples/wordle/proofs`](examples/wordle/proofs) is a Lean 4 model of the Wordle kernel (marking, rewards, the invalid-move allowance) with no `sorry`. It is hand-written and matched to the Rust in [`CORRESPONDENCE.md`](examples/wordle/proofs/CORRESPONDENCE.md), because that kernel uses `String` and is outside the subset Aeneas extracts today.

## 6. Examples

| Example | Run | Client | What it is |
|---|---|---|---|
| [`echo`](examples/echo) | `cargo run -p echo-env` | `EchoEnv` | The OpenEnv Echo MCP environment: `echo_message` and `echo_with_length` tools |
| [`wordle`](examples/wordle) | `cargo run -p wordle-env` | `TextArenaEnv` | Wordle-v0: 5 letters, 6 guesses, seeded secrets from a 15,034-word list, with a Lean model of the kernel |
| [`arith`](examples/arith) | `cargo run -p arith-env -- --ops 3` | `TextArenaEnv` | Arith-v0, a multi-turn tool-use task. The agent can send `calc <int> <op> <int>` to a calculator and must finish with `answer <int>`. The value is never in the observation or in `state()` |

Each example is a `kernel` crate with no web dependencies and a `server` crate that implements `Environment`, plus integration tests that run the server over real WebSockets.

## 7. How it works

```
OpenEnv client ── WebSocket / HTTP ──▶ senv::Server ──▶ Session ──▶ Environment ──▶ Kernel::transition
                                      routes, session    one          parse request,   pure: reward, done,
                                      table, capacity    engine each  render snapshot  next snapshot
```

`Server` owns the routes and the session table. Each session holds one `MemoryEngine`, and when its socket closes the engine is dropped. [DESIGN.md](DESIGN.md) has the details, and [PROTOCOL.md](PROTOCOL.md) lists the OpenEnv wire subset.

## 8. Compatibility and limits

The compatibility scripts run the official clients against the examples: [`scripts/echo_compat.sh`](scripts/echo_compat.sh), [`scripts/wordle_compat.sh`](scripts/wordle_compat.sh), and [`scripts/arith_compat.sh`](scripts/arith_compat.sh). Behavior that differs from upstream OpenEnv:

- A WebSocket disconnect destroys the session. Upstream keeps an HTTP-created session until `session/close`.
- There is no simulation-mode HTTP API (`POST /reset`, `/step`, `/state`), no CORS, and no Gradio UI.
- Sessions always run on `MemoryEngine`. There is no PostgreSQL-backed engine.

On one WebSocket, the Echo example averaged 0.12 ms per step against 2.1 ms for the Python Echo server, at about 5 MB of resident memory. That run used senv 0.2 on a 4 vCPU host ([docs/BENCHMARKS.md](docs/BENCHMARKS.md)). For language-model training, generation on the GPU dominates step time, so this rarely matters there.

## 9. Development

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
OPENENV_SRC=$PWD/OpenEnv bash scripts/echo_compat.sh   # official-client check
(cd examples/wordle/proofs && lake build)              # Lean model of the Wordle kernel
```

## License

Apache License 2.0. See [LICENSE](LICENSE). OpenEnv and h5i attributions are in [NOTICE](NOTICE).
