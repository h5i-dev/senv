# Lean correspondence

`proofs/SenvWordle.lean` is checked by Lean 4.31 (`proofs/lean-toolchain`).
The library builds with `lake build` from `proofs/`. There is no `sorry` and
no `admit`.

The proofs are about the pure game. They do not establish WebSocket
isolation, session cleanup, capacity limits, or the fact that each socket has
its own `MemoryEngine`. Those are properties of `src/server.rs`, and the Rust
tests are what check them.

## Names

| Lean | Rust | What it decides |
|---|---|---|
| `mark` / `markPairs` | `mark` in `src/wordle.rs` | Greens consume a secret letter, then yellows consume a remaining copy |
| `completionMilli` | `completion_milli` | `(greens + 0.5 * yellows) / 5` in thousandths |
| `fresh` | `fresh` | `done = false`, reward 0, turn 0, no guesses |
| `step` | `step_core` | Win, six-miss loss, invalid allowance, repeat-before-dictionary |
| `Move.play` | `Move::Candidate` | The shell has already parsed five letters and set `in_dictionary` |
| `Move.malformed` | `Move::Malformed` | Format or length failure from the shell |
| `latestCompletion` | `latest_completion` | Reward of a second consecutive invalid move |

Rust stores guesses in a fixed array of six and recomputes the last reward
from that array. Lean stores the same guesses in a list and recomputes with
`mark`. The shell, not these functions, picks the secret and renders JSON.

## Checked against the Rust tests

`cigar` on `crane` is `[green, wrong, wrong, yellow, yellow]` and 400
thousandths. `llama` on `alley` is `[yellow, green, yellow, wrong, wrong]`.
Both are `native_decide` theorems and the same vectors as
`greens_consume_before_yellows` in `src/wordle.rs`.

`mark_green_iff` is proved for every secret and guess: a mark is green
exactly where the letters are equal. That is the first pass. The yellow pass
is the same recursion as the Rust second loop.

## Not proved

The dictionary file, FNV episode secrets, reward-signal text, and the Axum
session table are outside this file on purpose.
