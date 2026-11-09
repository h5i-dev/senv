# senv

senv is an OpenEnv-compatible Echo environment. The process is an Axum
server. Each session's reset, step, and state run as an h5i-app kernel on a
private `MemoryEngine`.

This repository used to be a sandboxed Python environment manager on top of
uv and h5i-sandbox. That product is gone from the tree. The git history still
has it. The crate name, the binary name, and the Apache-2.0 license stayed.

## Pieces

The shell (`src/server.rs`, `src/protocol.rs`, `src/main.rs`) accepts TCP
connections, parses OpenEnv JSON, and decides session identity. It generates
episode ids when the client does not supply one. It does not decide what a
transition means.

The kernel (`src/kernel.rs`) is a pure function from `(principal, snapshot,
command)` to a write set and a reply, or a refusal. `apply` replaces the
snapshot with the write set. The shell passes the episode id and the seed in,
so the function does not read a clock or a random source.

The adapter (`src/store.rs`, `src/env.rs`) implements `h5i_app::Kernel` for
that function and builds one `H5iApp<EchoKernel, EchoStore, MemoryEngine<_>>`
per session. `H5iApp::with_executor` still requires a `Store` type.
`EchoStore` refuses every PostgreSQL `load` and `write`. Sessions call the
synchronous `MemoryEngine::execute` through `H5iApp::engine()`. They do not
open a database, and they do not use the h5i-app HTTP router: OpenEnv rewards
are floats, and `h5i-app-json` only encodes `u64` numbers. Reward on the wire
is `serde_json`. Inside the kernel it is an optional number of thousandths
(`Some(0)` is `0.0`, `None` is JSON null).

`AnonAuth` exists because `H5iApp` is generic over an authenticator. OpenEnv
clients do not send h5i bearer tokens. The principal is always tenant 1.
That number is not the isolation boundary. The boundary is that no two
sessions share an engine.

## Session lifecycle

`POST /mcp` method `openenv/session/create` allocates a session and leaves it
unattached. `GET /ws?session_id=` attaches the socket. A socket with no
session id allocates and attaches one. The socket task owns the attachment.
When that task ends, the session and its engine are dropped.

A second socket for the same id is refused and does not drop the first
session. An HTTP `session/close` while a socket is attached only marks the
session closing; the socket task performs the drop. Unattached sessions that
stay idle past `--idle-timeout-secs` are removed by a sweep on the next
create and by a 30-second task inside `serve`.

The live-session count is capped by `--max-sessions` (default 8, also read
from `MAX_CONCURRENT_ENVS`).

Upstream OpenEnv keeps an HTTP-created session after the WebSocket
disconnects, unless a close is already pending. senv drops it on disconnect.
That is a deliberate deviation: the environment must not outlive the socket
that was using it. The official client then sends `session/close`, which may
answer `Unknown session_id`. The client ignores that error.

`PROTOCOL.md` is the wire subset, including which calls increment
`step_count`.

## What a proof of the kernel would mean

A proof of `transition` can talk about reset, the step counter, reward, and
`done`. It cannot talk about two WebSockets, the mutex around the session
table, or the process that hosts them. Those are shell properties and are
covered by the Axum tests, not by a proof of the pure function. The kernel
still carries `serde_json::Value` for tool payloads, so it is not in the
fragment Aeneas extracts today. No Lean proof is claimed.

## Pins

| Component | Revision |
|---|---|
| OpenEnv wire and Echo client | `58d0344011fe961342706c90288f7e842b15b1c9` |
| h5i-app (`Kernel`, `MemoryEngine`, `H5iApp`) | `5e0f8a929c57bb12a321090d98a14b3a2714b849` |

Both constants live in `src/lib.rs`. The h5i revision is also the `rev` in
`Cargo.toml`. CI checks out that OpenEnv commit before running
`scripts/echo_compat.sh`.

## Out of scope for this server

Rubrics, the generic MCP product surface, h5i box integration, simulation-mode
HTTP step routes, and a textarena Wordle port are not implemented. Echo does
not terminate, so there is no reward schedule beyond reset's zero and a
step's null.
