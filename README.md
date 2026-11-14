# senv

<p align="left">
  <a href="https://github.com/h5i-dev/senv/actions/workflows/ci.yaml"><img alt="CI" src="https://github.com/h5i-dev/senv/actions/workflows/ci.yaml/badge.svg"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/License-Apache%202.0-blue.svg"></a>
</p>

**senv** is a Rust server for [OpenEnv](https://github.com/openenv/OpenEnv) reinforcement-learning environments. It speaks the OpenEnv wire protocol, so the **unmodified OpenEnv Python clients** (`EchoEnv`, `TextArenaEnv`) connect to it as they would to the Python servers. Every session is a pure [h5i-app](https://github.com/h5i-dev/h5i) kernel running on its own in-memory engine, which keeps transitions deterministic, isolated per session, and small enough to prove in Lean 4.

- **Drop-in for OpenEnv clients.** `reset()`, `step()`, and `state()` over a WebSocket, plus MCP tools on `/mcp`. No client patches.
- **Fast and small.** On one WebSocket, an Echo step averages 0.12 ms against 2.1 ms for the Python Echo server, at about 5 MB RSS ([benchmarks](docs/BENCHMARKS.md)).
- **One kernel per session.** Sessions never share state. Reset, step, and state are pure functions of the snapshot, the seed, and the action.
- **Verifiable.** The Wordle transition is proved in [Lean 4](proofs/SenvWordle.lean).
- **Built for parallel rollouts.** Many sessions per process, so a trainer can run a whole GRPO group at once (see [Train an agent](#4-train-an-agent)).

> [!NOTE]
> senv is early. This repository used to ship a sandboxed Python environment manager; that product and its `v0.1.0` release are not the server described here.

---

## 1. Install

Build from source. You need Rust 1.88 or newer.

```bash
git clone https://github.com/h5i-dev/senv && cd senv
cargo build --release --locked
./target/release/senv serve            # listens on 127.0.0.1:8000
```

Or use Docker:

```bash
docker build -t senv .
docker run --rm -p 8000:8000 senv      # listens on 0.0.0.0:8000
```

## 2. Quick start

Start a server for the environment you want:

```bash
senv serve --env wordle --bind 127.0.0.1:8000
```

Then connect with the **unmodified** OpenEnv client. senv is tested against OpenEnv commit [`58d0344`](https://github.com/openenv/OpenEnv/tree/58d0344011fe961342706c90288f7e842b15b1c9):

```bash
git clone https://github.com/openenv/OpenEnv.git
git -C OpenEnv checkout 58d0344011fe961342706c90288f7e842b15b1c9
python3 -m venv .venv && .venv/bin/pip install -r scripts/echo-client-requirements.txt
export PYTHONPATH="$PWD/OpenEnv/src:$PWD/OpenEnv/envs"
```

```python
from textarena_env import TextArenaEnv
from textarena_env.models import TextArenaAction

with TextArenaEnv(base_url="http://127.0.0.1:8000").sync() as env:
    result = env.reset(seed=7)
    print(result.observation.prompt)

    result = env.step(TextArenaAction(message="[crane]"))
    print(result.observation.messages[-1].content)  # green / yellow feedback
    print(result.reward, result.done)
```

The Echo environment (the default for `senv serve`) is an MCP tool environment:

```python
from echo_env import CallToolAction, EchoEnv

with EchoEnv(base_url="http://127.0.0.1:8000").sync() as env:
    env.reset()
    result = env.step(CallToolAction(tool_name="echo_message", arguments={"message": "Hello!"}))
    print(result.observation.result["data"])  # "Hello!"
```

## 3. Environments

| `--env` | Client | What it is |
|---|---|---|
| `echo` (default) | `EchoEnv` | The OpenEnv Echo MCP environment: `echo_message` and `echo_with_length` tools. For smoke tests and latency. |
| `wordle` | `TextArenaEnv` | Wordle-v0: 5 letters, 6 guesses, on the TextArena socket. Seeded secrets from a 15,034-word list ([`assets/wordle/words.txt`](assets/wordle/words.txt)). |
| `arith` | `TextArenaEnv` | Arith-v0, a custom multi-turn tool-use task. The agent gets a nested expression such as `((6 * 8) * 9)`. It can send `calc <int> <op> <int>` to a calculator and must finish with `answer <int>`. Reward is 1 for a correct answer. `--arith-ops 2..4` sets the difficulty. The value is never in the observation or in `state()`. |

### Server options

| Flag | Environment variable | Default |
|---|---|---|
| `--bind` | `SENV_BIND` | `127.0.0.1:8000` |
| `--env` | `SENV_ENV` | `echo` |
| `--arith-ops` | `SENV_ARITH_OPS` | `3` (2, 3, or 4) |
| `--max-sessions` | `MAX_CONCURRENT_ENVS` | `8` |
| `--max-message-bytes` | | `16777216` (16 MiB) |
| `--idle-timeout-secs` | | `120` (`0` disables the idle sweep) |

`GET /health` returns `{"status":"healthy"}`.

## 4. Train an agent

Any trainer that drives an OpenEnv client can train against senv. A rollout is one client session. To run a GRPO group, start the server with enough `--max-sessions` and open one client per rollout:

```python
envs = [TextArenaEnv(base_url="http://127.0.0.1:8000").sync() for _ in range(group_size)]
for env in envs:
    env.__enter__()
results = [env.reset(seed=problem_seed) for env in envs]  # same problem in every rollout
# ... generate one action per active env, then env.step(TextArenaAction(message=line)) ...
```

The same seed gives the same problem, so a group can share one task while its rollouts stay independent. [`scripts/arith_grpo.py`](scripts/arith_grpo.py) is a complete example: GRPO with LoRA on Arith-v0, with generation batched across sessions.

## 5. How it works

```
 OpenEnv client ──WebSocket / HTTP──▶ Axum shell ──command──▶ kernel(snapshot, cmd) ─▶ write set + reply
                                      (protocol,               pure, no clock, no RNG
                                       sessions)               one MemoryEngine per session
```

- The **shell** ([`src/server.rs`](src/server.rs), [`src/protocol.rs`](src/protocol.rs)) parses the OpenEnv protocol, owns the session table, and renders observations.
- Each **kernel** ([`src/kernel.rs`](src/kernel.rs), [`src/wordle.rs`](src/wordle.rs), [`src/arith.rs`](src/arith.rs)) is a pure transition `(snapshot, command) → (write set, reply)`. The shell passes in the seed and episode id, so a transition never reads a clock or a random source.
- Each session gets its own `h5i_app::MemoryEngine`. Sessions are isolated because no two of them share an engine. When a socket closes, its engine is dropped.

[DESIGN.md](DESIGN.md) has the full architecture. [`proofs/`](proofs/) holds the Lean 4 model of the Wordle kernel (reset, green-before-yellow marking, rewards, the one-invalid allowance), with no `sorry`. [`proofs/CORRESPONDENCE.md`](proofs/CORRESPONDENCE.md) maps the Lean names back to the Rust code. The proofs do not cover socket isolation; the integration tests check that.

## 6. Performance

One run on a 4 vCPU AMD EPYC 7R32. Both Echo servers used the same client. See [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

| Server | Mean step | p99 step | Steps/s (one socket) | RSS |
|---|---:|---:|---:|---:|
| senv Echo | 0.121 ms | 0.157 ms | 8,276 | 5.3 MB |
| Python OpenEnv Echo | 2.097 ms | 2.386 ms | 477 | 108 MB |
| senv Wordle | 0.223 ms | 0.334 ms | 4,476 | 6.0 MB |

## 7. Compatibility and limits

The compatibility scripts run the official clients against a live server: [`scripts/echo_compat.sh`](scripts/echo_compat.sh), [`scripts/wordle_compat.sh`](scripts/wordle_compat.sh), and [`scripts/arith_compat.sh`](scripts/arith_compat.sh). The behaviors most likely to surprise someone who knows upstream OpenEnv:

- A WebSocket disconnect destroys the session. Upstream keeps an HTTP-created session until `session/close`.
- There is no simulation-mode HTTP API (`POST /reset`, `/step`, `/state`), no CORS, and no Gradio UI.
- Echo never sets `done`, and a step's reward is JSON `null`.
- In Wordle, `state()` reveals the secret, as upstream does. Arith never reveals its value.

[PROTOCOL.md](PROTOCOL.md) is the full list.

## 8. Development

```bash
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
OPENENV_SRC=$PWD/OpenEnv bash scripts/echo_compat.sh   # official-client check
(cd proofs && lake build)                              # Lean proofs
```

## License

Apache License 2.0. See [LICENSE](LICENSE). OpenEnv and h5i attributions are in [NOTICE](NOTICE).
