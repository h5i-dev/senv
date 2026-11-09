# senv

OpenEnv-compatible Echo environment, executed on [h5i-app](https://github.com/h5i-dev/h5i).

`senv serve` speaks the production Echo protocol from
[meta-pytorch/OpenEnv](https://github.com/meta-pytorch/OpenEnv) at commit
`58d0344011fe961342706c90288f7e842b15b1c9`. Each WebSocket session owns its own
`h5i_app::MemoryEngine`. Reset, step, and state are kernel transitions. The
Axum process only parses the wire protocol and drops the engine when the
socket ends.

This repository previously shipped a sandboxed Python environment manager.
That command set is gone. The name, the git history, and the Apache-2.0
license remain. `DESIGN.md` describes the current architecture. `PROTOCOL.md`
is the supported wire subset and its limits. `NOTICE` attributes OpenEnv and h5i.

## Run

Rust 1.88 or newer.

```bash
cargo run --locked --bin senv -- serve
```

The default listen address is `127.0.0.1:8000`.

| Flag | Environment | Default |
|---|---|---|
| `--bind` | `SENV_BIND` | `127.0.0.1:8000` |
| `--env` | `SENV_ENV` | `echo` (`wordle` serves Wordle-v0) |
| `--max-sessions` | `MAX_CONCURRENT_ENVS` | 8 |
| `--max-message-bytes` | | 16777216 (16 MiB) |
| `--idle-timeout-secs` | | 120 (0 disables the idle sweep) |

`GET /health` returns `{"status":"healthy"}`.

A Docker image is defined in `Dockerfile`. It listens on `0.0.0.0:8000`:

```bash
docker build -t senv .
docker run --rm -p 8000:8000 senv
```

That image was not built on the host that ran the compatibility test. The
Docker data root there is on a nearly full disk.

`install.sh` installs a published release binary. `pyproject.toml` still
packages that same binary as the `h5i-senv` wheel. Neither path installs a
Python library.

## Python client

Use the unmodified `EchoEnv` from the pinned OpenEnv tree. Do not vendor it
into this repository.

```bash
git clone https://github.com/meta-pytorch/OpenEnv.git
git -C OpenEnv checkout 58d0344011fe961342706c90288f7e842b15b1c9
python3 -m venv .venv
.venv/bin/pip install -r scripts/echo-client-requirements.txt
```

`scripts/echo-client-requirements.txt` lists the modules that tree imports
while loading `EchoEnv`. It is not `pip install openenv`, which also installs
Gradio.

With `senv serve` already listening:

```bash
export OPENENV_SRC="$PWD/OpenEnv"
export SENV_BASE_URL=http://127.0.0.1:8000
export PYTHONPATH="$OPENENV_SRC/src:$OPENENV_SRC/envs"
.venv/bin/python - <<'PY'
from echo_env import CallToolAction, EchoEnv

with EchoEnv(base_url="http://127.0.0.1:8000").sync() as env:
    reset = env.reset()
    print(reset.reward, reset.observation.metadata["message"])
    step = env.step(CallToolAction(
        tool_name="echo_message",
        arguments={"message": "Hello!"},
    ))
    print(step.observation.result["data"])
    print(env.state().step_count)
PY
```

`reset()` returns reward `0.0`, `done` false, and metadata message
`Echo environment ready!`. `step(CallToolAction(...))` increments
`step_count`. `call_tool` and `list_tools` use HTTP `/mcp` and do not
increment it. A string tool result is the raw string in `result["data"]`.
A non-string result's text is JSON, not Python's `repr`.

`scripts/echo_compat.sh` builds the binary, starts it, and runs
`scripts/echo_client_compat.py` against that client. Set `OPENENV_SRC` to the
checkout. Set `SENV_PYTHON` if `python3` is older than 3.10. Put
`CARGO_TARGET_DIR` on a large disk before a cold build.

## Validation on 2026-11-09

OpenEnv `58d0344011fe961342706c90288f7e842b15b1c9`. h5i-app
`5e0f8a929c57bb12a321090d98a14b3a2714b849`. Host Python 3.12.15. The interpreter
that imported the client had pydantic 2.14.3, websockets 17.4, httpx 0.28.1,
fastapi 0.162.2, fastmcp 3.4.11, openai 3.48.0, and requests 2.34.2.

```text
cargo test --locked --offline
17 passed (kernel, protocol, and Wordle unit tests)
3 passed (tests/protocol.rs: Echo reset/step/state, isolation, malformed frames,
         capacity, second socket, idle reap)
2 passed (tests/wordle.rs: feedback, win, invalid moves, seed, isolation)
0 failed

cargo clippy --all-targets --locked --offline -- -D warnings
clean

OPENENV_SRC=<pinned OpenEnv> SENV_PYTHON=<venv python> bash scripts/echo_compat.sh
client: <OpenEnv>/envs/echo_env/client.py
PASS official EchoEnv reset/step/state, isolation, reconnect
```

The Python script calls the unmodified `EchoEnv.reset`, `EchoEnv.step`
(`CallToolAction` for `echo_message` and `echo_with_length` with `héllo`,
length 5), `EchoEnv.state`, `list_tools`, and `call_tool` on one session; a
second `EchoEnv` whose reset stays at `step_count` 0; then a new `EchoEnv`
after close, which receives a new episode. The client file path is checked
against `OPENENV_CLIENT` when that variable is set. `echo_compat.sh` sets it
to `envs/echo_env/client.py` inside `OPENENV_SRC`.

`scripts/wordle_compat.sh` is the same kind of check for Wordle. It starts
`senv serve --env wordle` and imports the unmodified `TextArenaEnv` from
`envs/textarena_env/client.py` in that OpenEnv tree. On 2026-11-09 that script
printed:

```text
PASS official TextArenaEnv Wordle reset/step/state, feedback, win, invalid, isolation
```

`cargo test --locked --offline` then also passed the two Wordle WebSocket
tests in `tests/wordle.rs` (17 library tests, 3 Echo protocol tests, 2 Wordle
tests). Wordle is a 5-letter, 6-guess game on that client's socket. The word
list is `assets/wordle/words.txt` (15,034 words from a system spelling list).
It is not NLTK, not the New York Times list, and not CPython's RNG. `PROTOCOL.md`
lists the other differences, including that `state()` reveals the secret.

`proofs/SenvWordle.lean` (Lean 4.31, `lake build` in `proofs/`) proves the
pure Wordle reset, the green-before-yellow marker, win and six-miss rewards,
and the one-invalid allowance. It has no `sorry` or `admit`. It does not
prove WebSocket isolation or that sessions have separate engines.
`proofs/CORRESPONDENCE.md` maps the Lean names to `mark` and `step_core`.

`docs/BENCHMARKS.md` records one 2026-11-09 run of `scripts/bench.sh` on a 4 vCPU host. The client was `scripts/bench.py`. On one WebSocket, 400 Echo steps averaged 0.121 ms on senv and 2.097 ms on the pinned Python Echo server. Rust Wordle steps averaged 0.223 ms (n=240). The GPU was idle. That page is the whole claim.

## Limits

The full list is `PROTOCOL.md`. The ones that surprise people who know
upstream OpenEnv:

- A WebSocket disconnect destroys the session. Upstream keeps an HTTP-created
  session until `session/close`. The official client still succeeds, because
  it ignores a close error after detach.
- The server accepts WebSocket messages up to 16 MiB. The client's default
  ceiling is 100 MB.
- Echo never sets `done`. A step's reward is JSON null. Reset's reward is 0.0.
- `seed` is stored and ignored.
- There is no `/reset`, `/step`, or `/state` HTTP simulation API, no CORS, and
  no Gradio UI.
- Kernel proofs, if they are added later, will not establish socket isolation.
  Separate engines and the session table are what isolate clients, and the
  tests are what check that.
