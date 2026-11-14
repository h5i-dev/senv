# senv design

senv is a library that serves h5i-app kernels as OpenEnv environments. An
environment author writes two things: a kernel, which is an ordinary h5i-app
`Kernel`, and an `Environment`, which is the OpenEnv shell around it. senv
supplies the wire protocol, the session table, and the server.

This repository used to be a sandboxed Python environment manager, and after
that a single binary with three built-in environments. Both are gone from the
tree; the git history has them.

## Pieces

```
OpenEnv client ─ WebSocket / HTTP ─▶ senv::Server ─▶ Session ─▶ Environment::reset/step/state ─▶ Kernel command
                                     (routes,         (one        (trusted shell: parse request,   │
                                      session table)   engine)     render snapshot)                 ▼
                                                                                         MemoryEngine::execute
                                                                                         transition + apply
```

The kernel is a pure function from `(principal, snapshot, command)` to a write
set and a reply, or a refusal that commits nothing. It is the h5i-app
`Kernel` trait, unchanged. It decides every reward, every `done`, and every
step count. Nothing in senv reads a clock or a random source on its behalf.
In h5i's layout the kernel lives in its own crate with no web or database
dependencies, so `h5i app extract` can translate it to Lean with Aeneas.

`Environment` (`src/env.rs`) is the trusted shell for one kernel. It turns an
OpenEnv `reset` into a kernel command (deriving the task from the seed is
allowed here, as long as it is deterministic), parses a `step` payload into a
command, and renders the committed snapshot and reply into OpenEnv JSON. It
also maps kernel refusals to wire errors and, optionally, exposes MCP tools.
It must not compute outcomes; it reads them from what the kernel committed.
`&self` is configuration shared by every session, such as a difficulty level.

`Session` (`src/session.rs`) is one episode stream: an `Arc<Environment>` and
a private `h5i_app::MemoryEngine`. Each command runs `transition` and `apply`
under the engine lock, then the session reads back the committed snapshot and
hands it to `observe`. A small mutex keeps that execute-then-read pair atomic
when an HTTP tool call races a socket step. A `Session` can be driven
in-process with no socket, which is how kernel properties are checked from
Rust tests and how a trainer can run rollouts without the network.

`Server` (`src/server.rs`) is the Axum shell: `/health`, `/metadata`,
`/schema`, `/mcp`, and `/ws`, plus the session table. `src/wire.rs` parses
the envelope (`reset`, `step`, `state`, `close`) and writes frames. Neither
knows what an environment's actions or observations contain.

`senv::run` (`src/cli.rs`) parses the common flags (`--bind`,
`--max-sessions`, `--max-message-bytes`, `--idle-timeout-secs`), initializes
logging, and serves. An environment binary with its own flags flattens
`ServeArgs` into its parser.

## Isolation

Every OpenEnv session gets its own `MemoryEngine`. The principal and tenant id
inside it are whatever the environment returns from `principal()`; they are
not the isolation boundary. The boundary is that no two sessions share an
engine, which is a property of `SessionTable`, checked by the integration
tests, and not of any kernel.

## Session lifecycle

`POST /mcp` method `openenv/session/create` allocates a session and leaves it
unattached. `GET /ws?session_id=` attaches the socket. A socket with no
session id allocates and attaches one. The socket task owns the attachment.
When that task ends, the session and its engine are dropped.

A second socket for the same id is refused and does not drop the first
session. An HTTP `session/close` while a socket is attached only marks the
session closing; the socket task performs the drop. Unattached sessions that
stay idle past `idle_timeout` are removed by a sweep on the next create and by
a 30-second task inside `Server::serve`.

The live-session count is capped by `max_sessions` (default 8, also read
from `MAX_CONCURRENT_ENVS` by `ServeArgs`).

Upstream OpenEnv keeps an HTTP-created session after the WebSocket
disconnects, unless a close is already pending. senv drops it on disconnect.
That is a deliberate deviation: the environment must not outlive the socket
that was using it. The official client then sends `session/close`, which may
answer `Unknown session_id`. The client ignores that error.

`PROTOCOL.md` is the wire subset.

## What a kernel proof covers

A proof about `transition` can talk about rewards, termination, step counts,
and what the snapshot hides. It cannot talk about two WebSockets, the mutex
around the session table, the rendering in `observe`, or the process that
hosts them. Those are shell properties, covered by tests.

The examples show the range:

- `examples/wordle/proofs` is a Lean 4 model of the Wordle kernel (marking,
  rewards, the invalid-move allowance) with no `sorry`. It is hand-written and
  matched to the Rust by `CORRESPONDENCE.md`, not extracted by Aeneas: the
  kernel uses `String` and iterators, which are outside the subset Aeneas
  translates today.
- The Echo and Arith kernels have Rust tests but no Lean model.

Writing a kernel in the Aeneas subset (no `String`, `while` loops over
indices, no `?`) and running `h5i app extract` / `h5i app prove` is the path
to a machine-checked correspondence. h5i's `examples/app/calculator` shows the
layout.

## Pins

| Component | Revision |
|---|---|
| OpenEnv wire protocol and clients | `58d0344011fe961342706c90288f7e842b15b1c9` |
| h5i-app (`Kernel`, `MemoryEngine`) | crates.io `0.5.5` |

`OPENENV_GIT_REVISION` lives in `src/lib.rs`. CI checks out that OpenEnv commit
before running the compatibility scripts. senv depends on h5i-app with no
features; environments add the h5i-app features they use themselves.

## Out of scope

Rubrics, simulation-mode HTTP step routes, Gradio, and a PostgreSQL-backed
engine are not implemented. Sessions always run on `MemoryEngine`.
