# OpenEnv subset implemented by senv

Derived from `meta-pytorch/OpenEnv` commit `58d0344011fe961342706c90288f7e842b15b1c9`.
The Echo behavior comes from `envs/echo_env` and `src/openenv/core`. This server
reimplements that wire protocol. It does not copy OpenEnv source. See `NOTICE`.

`EchoEnv` is an `MCPToolClient`. It only supports production mode. Connecting
creates an HTTP session, then attaches a WebSocket to that same session.
`reset`, `step`, and `state` travel on the socket. `list_tools` and `call_tool`
travel as JSON-RPC on `POST /mcp` and do not increment `step_count`.

## Routes

| Method | Path | Behavior |
|---|---|---|
| GET | `/health` | `{"status":"healthy"}` |
| GET | `/metadata` | Name `echo_env`, crate version, short description |
| GET | `/schema` | A description of the Echo action and state shapes |
| POST | `/mcp` | JSON-RPC 2.0. HTTP status is 200, including for JSON-RPC errors |
| GET | `/ws` | WebSocket. Optional query `session_id` |

There is no CORS middleware. There are no simulation-mode `POST /reset`,
`POST /step`, or `POST /state` routes. A WebSocket message whose `type` is
`mcp` is `UNKNOWN_TYPE`.

## HTTP `/mcp`

Request body is a JSON object with `jsonrpc` equal to `"2.0"` and a string
`method`. `params`, when present, is an object. `id` is echoed; a missing id
becomes JSON `null`.

| Method | Result |
|---|---|
| `openenv/session/create` | `{"session_id": "<uuid>"}`. No parameters are required |
| `openenv/session/close` | Requires `session_id`. See lifecycle below |
| `tools/list` | Requires `session_id`. Tools use camelCase `inputSchema` |
| `tools/call` | Requires `session_id`, `name`, and an object `arguments` (default `{}`) |

JSON-RPC codes used here:

| Code | When |
|---|---|
| -32700 | Body is not JSON |
| -32600 | Not a JSON-RPC request, or the session is attached or closing when a tool method needed a free session handle |
| -32601 | Unknown method. Message is `Method not found: {method}` |
| -32602 | Bad params, unknown session, unknown tool, or invalid tool arguments |
| -32000 | At capacity. `data` has `active_sessions` and `max_sessions` |
| -32603 | The tool ran and failed, or the engine refused the command |

`tools/call` success is a FastMCP-shaped `CallToolResult`: `content`,
`structured_content.result`, `meta`, `data`, and `is_error: false`. The
official client returns `result.data`.

An unknown tool or a bad argument is -32602. A tool that runs and then fails
is -32603. Neither call changes `step_count`.

## WebSocket messages

Client to server:

```json
{"type":"reset","data":{"seed":1,"episode_id":"ep"}}
{"type":"step","data":{"type":"call_tool","tool_name":"echo_message","arguments":{"message":"Hello!"}}}
{"type":"step","data":{"type":"list_tools"}}
{"type":"state"}
{"type":"close"}
```

`reset` may omit `data`. `seed` is an integer or null. `episode_id` is a
string of at most 255 bytes. Extra reset fields are rejected. A missing
`episode_id` is filled with a new UUID in the server shell before the kernel
runs. `seed` is stored and does not change the observation.

`step` requires `data` to be an object whose `type` is `list_tools` or
`call_tool`. Anything else is `VALIDATION_ERROR` and does not increment
`step_count`. `list_tools` may carry an object `metadata`. `call_tool`
requires a string `tool_name` and may carry object `arguments` and `metadata`.

`close` ends the read loop and sends no reply. The session is then dropped.

Server to client:

```json
{"type":"observation","data":{"observation":{},"reward":0.0,"done":false,"metadata":{}}}
{"type":"state","data":{"episode_id":"ep","step_count":0}}
{"type":"error","data":{"code":"VALIDATION_ERROR","message":"..."}}
```

Observation packing follows `serialize_observation`: `reward` and `done` are
siblings of `observation`, not fields inside it. Non-empty metadata is copied
to both places. Empty metadata is omitted at the top level.

Reset observation metadata is `{"status":"ready","message":"Echo environment ready!"}`,
with `reward` `0.0` and `done` false.

A `call_tool` observation carries `tool_name`, `result`, and `error`. Success
uses the same CallToolResult shape as HTTP, with `error` null. Failure uses
`result` null and `error.error_type` of `execution_error`, `invalid_args`, or
`tool_not_found`. A step's reward is JSON `null`. `done` is false. Echo has
no terminal state.

`list_tools` on the socket uses snake_case `input_schema`. The two tools are
`echo_message` ("Echo back the provided message.") and `echo_with_length`
("Echo back the message with its length."). Both take a required string
`message` in the published schema.

Text frames that are not JSON are `INVALID_JSON`. Binary frames are
`INVALID_JSON` with message `Invalid JSON: binary frame`. An unknown `type`
is `UNKNOWN_TYPE` with message `Unknown message type: {type}`. A kernel
refusal is `EXECUTION_ERROR`. The connection stays up after these errors.

Error codes this server emits: `INVALID_JSON`, `UNKNOWN_TYPE`,
`VALIDATION_ERROR`, `EXECUTION_ERROR`, `CAPACITY_REACHED`, `SESSION_ERROR`.
Pydantic's `errors` array is not reproduced.

`CAPACITY_REACHED` uses the upstream sentence
`Server at capacity: {active}/{max} sessions active. Cannot accept new connections.`
and adds `active_sessions` and `max_sessions`. A second socket for a live
session gets `SESSION_ERROR` and
`Session {id} already has an attached WebSocket`. That failure does not drop
the first socket or the session. An unknown id is
`Unknown session_id: {id}`.

Ping frames are answered with pong. The largest accepted message and frame is
16 MiB unless `--max-message-bytes` says otherwise. The official client's
default is 100 MB, so a result larger than the server limit closes the socket
even when the client would have accepted it.

## Echo tool behavior

`echo_message` returns its `message` argument. On this server the argument may
be any JSON value, not only a string. `echo_with_length` requires a string.
`length` is the number of Unicode scalar values, which matches Python `len`
for str and Rust `chars().count()`.

For a string result, the CallToolResult text is the raw string. For any other
JSON value, the text is serde_json's rendering, not Python's `repr`.

A missing `message`, an unexpected argument name, or a non-string passed to
`echo_with_length` is a failed tool call. On the socket that failure still
increments `step_count`, matching upstream once the action has parsed. On
HTTP it does not.

## Sessions

Each session is its own `h5i_app::MemoryEngine`. The tenant id inside that
engine is always 1. Isolation is the separate engine, not the tenant number.
Dropping the session drops the engine.

- `openenv/session/create` inserts an unattached session. Its initial episode
  id is random and `step_count` is 0, before any reset.
- `/ws?session_id=` attaches that session.
- `/ws` with no id, or an empty id, creates a session and attaches it.
- When the attached socket's task ends, for any reason, the session is removed.
- `openenv/session/close` on an unattached session removes it and returns
  `closed: true`. On an attached session it marks the session closing, returns
  `closed: false, closing: true`, and the socket task removes it when the
  socket ends.
- The official client detaches the socket and then calls `session/close`.
  Because this server already removed the session, that call often returns
  `Unknown session_id`. The client ignores that error.
- Upstream's HTTP session survives a WebSocket disconnect unless a close is
  pending. This server does not. The task requires cleanup on disconnect, and
  a reconnect is a new episode.
- Unattached sessions idle for `--idle-timeout-secs` (default 120; 0 disables
  the sweep) are removed on the next create and about every 30 seconds while
  `senv serve` is running. Attached sessions are not swept. The integration
  tests drive the router directly, so only the create-time sweep runs there.

Capacity counts every live session, attached or not. The default is 8
(`--max-sessions`, or `MAX_CONCURRENT_ENVS`). Creating or attaching past that
limit fails with the capacity error above. A rejected second attach does not
consume another slot.

## What the kernel does and does not establish

`transition` in `src/kernel.rs` is the pure step: reset stores the seed,
replaces the episode id, zeroes `step_count`, and returns reward zero; step
increments `step_count` or refuses on overflow without committing; invoke
evaluates a tool and leaves `step_count` unchanged; state returns a copy.
`done` is false. Those facts are about that function. They say nothing about
WebSocket isolation, the session table, or two clients sharing a process.

## Wordle-v0

`senv serve --env wordle` (or `SENV_ENV=wordle`) serves one classic game on
the OpenEnv `TextArenaEnv` WebSocket contract. The default remains Echo, and
the Echo routes above do not change.

`TextArenaEnv` connects to `/ws` with no `session_id`. That creates a session.
`reset` data is `seed` and `episode_id`. `step` data is `{"message": "..."}`.
`state` returns `episode_id`, `step_count`, `env_id` (`Wordle-v0`),
`num_players`, `max_turns`, `turn`, `last_reward`, `last_info`, and
`raw_state`. Observations use the TextArena fields `prompt`, `messages`,
`current_player_id`, `legal_players`, and `info`, with `reward` and `done`
beside the nested observation. Non-empty metadata is repeated at the top
level, matching `serialize_observation`.

The game is 5 letters and 6 guesses. A guess is the first `\[[A-Za-z0-9_]+\]`
in the message, lowercased. Greens are marked before yellows, and a yellow
consumes one remaining copy of that letter. The first invalid move does not
spend a guess. The next consecutive invalid move ends the episode. A correct
word ends it with reward 1. Six misses end it with reward
`(greens + 0.5 * yellows) / 5` of the last guess. Reward inside the kernel is
thousandths. The shell writes a JSON float.

This is not the TextArena package. Differences that matter:

- The answer list and the accepted-guess list are the same file,
  `assets/wordle/words.txt`: `/usr/share/dict/words` filtered to `^[a-z]{5}$`.
  That is not NLTK `en-basic` nouns and not the New York Times list.
- A seed picks `words[(seed as u64) % len]`. The same seed is stable in senv.
  It is not CPython's Mersenne Twister, and it will not match TextArena's
  secret for that seed.
- With no seed, the secret is FNV-1a of the episode id. The kernel does not
  draw it.
- `state.raw_state.game_state.secret_word` includes the secret, as the OpenEnv
  TextArena wrapper's snapshot does. A policy that reads `state()` can cheat.
  Play from the observation.
- Action text longer than 4096 bytes is `VALIDATION_ERROR`. Stepping after
  `done` is `EXECUTION_ERROR` and does not change the snapshot.
- `POST /mcp` still creates and closes sessions. `tools/list` is empty.
  `tools/call` is JSON-RPC `-32601`.
- `wordle.greens`, `wordle.yellows`, `wordle.repetitions`, and `wordle.correct`
  are attached on a step, using the same guess normalization as OpenEnv's
  Wordle reward provider. A win has no `Feedback:` line, so greens and yellows
  are 0 on that step and `wordle.correct` is 1.

`step_core` and `mark` in `src/wordle.rs` are the pure transition. A proof of
those functions does not prove socket isolation.
