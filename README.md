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

An environment's kernel is an h5i app, so h5i's toolchain proves things about it. Start the project with h5i and add the senv server next to it:

```bash
h5i app new my-env            # kernel/, proofs/, h5i-app.toml
cargo new --lib my-env/server # implements senv::Environment and h5i_app::Kernel
h5i app prove my-env          # extract kernel/ to Lean, build and gate the proofs
h5i app mutate my-env         # every bug listed in h5i-app.toml must break a proof
```

Write the kernel in the subset of Rust that Charon and Aeneas translate (no `String`, no closures or iterators, `while` loops over indices). `proofs/Spec.lean` states what the environment means in plain math; `proofs/Theorems.lean` proves the extracted kernel does that. `h5i app check` rejects `sorry`, `native_decide` and `axiom`, and accepts only theorems that rest on Lean's standard axioms. The `[[mutant]]` entries in `h5i-app.toml` are bugs an agent could exploit; `h5i app mutate` applies each one and fails if no proof breaks.

Two examples are verified this way, and CI runs `h5i app extract --check` and `h5i app check` on both:

| Example | What is proved | Mutants caught |
|---|---|---|
| [`arith`](examples/arith/proofs/Theorems.lean) | The calculator computes the integer operation inside ±10^9. A valid problem's value is [`Spec.value`](examples/arith/proofs/Spec.lean), with no overflow. An answer pays 1 if and only if it equals that value, and nothing else pays. Every episode ends within 8 steps, a finished one refuses steps, and no step changes the problem. The kernel never panics. | 9 of 9, from "any answer wins" to "the task can be swapped" |
| [`wordle`](examples/wordle/proofs/Theorems.lean) | `mark` is the TextArena marking rule, stated with letter counts in [`Spec.mark`](examples/wordle/proofs/Spec.lean). A scored guess pays 1000 if and only if it is the secret, a losing guess pays less than 1000, and every reward is between 0 and 1000. A repeated guess is never scored. Every move ends the game or uses up one of twelve moves. A finished game refuses moves, and no move changes the secret. The kernel never panics. | 10 of 10, including a yellow that reuses a letter and a green that does not consume one |

A proof covers the kernel only. Parsing the agent's text, choosing the task from the seed, and rendering observations are trusted shell code, checked by tests.

## 6. Examples

| Example | Run | Client | What it is |
|---|---|---|---|
| [`echo`](examples/echo) | `cargo run -p echo-env` | `EchoEnv` | The OpenEnv Echo MCP environment: `echo_message` and `echo_with_length` tools |
| [`wordle`](examples/wordle) | `cargo run -p wordle-env` | `TextArenaEnv` | Wordle-v0: 5 letters, 6 guesses, seeded secrets from a 15,034-word list |
| [`arith`](examples/arith) | `cargo run -p arith-env -- --ops 3` | `TextArenaEnv` | Arith-v0, a multi-turn tool-use task. The agent can send `calc <int> <op> <int>` to a calculator and must finish with `answer <int>`. The value is never in the observation or in `state()` |

Each example has a `kernel` crate with no web dependencies and a `server` crate that implements `Environment`, plus integration tests that run the server over real WebSockets. `arith` and `wordle` also have `h5i-app.toml` and `proofs/`.

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
h5i app prove examples/arith examples/wordle            # extract and check the proofs
h5i app mutate examples/arith                          # the listed bugs must break a proof
```

## License

Apache License 2.0. See [LICENSE](LICENSE). OpenEnv and h5i attributions are in [NOTICE](NOTICE).
