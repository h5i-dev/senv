# Measured server timings

One run on 2026-11-09. The client is `scripts/bench.py` (websockets 17.4). It is not the official `EchoEnv` or `TextArenaEnv` client. Both Echo servers were measured with that same client, so the Echo comparison includes this client's overhead on both sides. The GPU (NVIDIA A10G) was idle. These numbers are not a multi-host result and not a claim about other machines.

## Host

- Kernel: Linux 6.12.113-138.209.amzn2023.x86_64
- CPU: AMD EPYC 7R32, 4 vCPU
- RAM: 15 GiB
- rustc 1.99.1, `cargo build --locked --release`
- Python 3.12.15, uvicorn 0.54.0
- OpenEnv checkout `58d0344011fe961342706c90288f7e842b15b1c9`

## Command

```sh
export CARGO_HOME=/mnt/scratch/h5i-openenv/cargo-home
export CARGO_TARGET_DIR=/mnt/scratch/h5i-openenv/target
export PATH="/root/.cargo/bin:$PATH"
export SENV_PYTHON=/mnt/scratch/h5i-openenv/venv/bin/python
export OPENENV_SRC=/mnt/scratch/h5i-openenv/src/OpenEnv
export BENCH_OUT=/mnt/scratch/h5i-openenv/bench
bash scripts/bench.sh
```

`senv serve` used `--max-sessions 32`. The Python Echo server was `uvicorn echo_env.server.app:app` with `MAX_CONCURRENT_ENVS=32`. Ports were 127.0.0.1:8771 (Rust Echo), 8772 (Rust Wordle), and 8773 (Python Echo). There is no Python Wordle number: TextArena and NLTK were not installed.

websockets 17.4 printed a deprecation warning that `connect()` should be a context manager. Every timed call still returned a normal reply; the run exits on an error frame.

## Echo, one WebSocket

400 `echo_message` steps after one warmup step. `step_per_s` is `1000 * n / sum(step_ms)` on that single socket, not a parallel throughput.

| server | mean ms | p50 ms | p99 ms | steps/s | RSS |
| --- | ---: | ---: | ---: | ---: | ---: |
| senv Echo | 0.121 | 0.117 | 0.157 | 8276 | 5280 kB |
| Python Echo | 2.097 | 2.083 | 2.386 | 477 | 108284 kB |

On this run the Python mean step time was 17.4 times the senv mean (2.097 / 0.121). Full samples are in `docs/bench/rust-echo.json` and `docs/bench/python-echo.json`. Percentiles use nearest rank: `round(fraction * (n - 1))`.

Reset was timed once per Echo process (0.353 ms senv, 0.907 ms Python). Session create was 16 sequential HTTP calls. senv mean 1.069 ms, p50 0.385 ms, max 11.258 ms. Python mean 11.555 ms, p50 4.795 ms, max 111.875 ms. At n=16 the reported p99 is the max. The max is a single cold call, so the mean is not a steady-state create time.

Eight parallel clients each reset and took 20 Echo steps. Wall time was 37.9 ms for senv and 363 ms for the Python server. That wall includes connect and reset. It is not the single-socket steps/s above.

VmHWM was 5668 kB (senv Echo) and 108284 kB (Python). VmSize in the JSON is virtual address space, not resident memory.

## Wordle, one WebSocket

Absolute senv numbers only. Forty episodes, seeds 0..39, guesses `crane stone house plant query bread`, stopping early if `done` is true. All 240 guesses were played (6 per episode).

| | mean ms | p50 ms | p99 ms | n |
| --- | ---: | ---: | ---: | ---: |
| step | 0.223 | 0.221 | 0.334 | 240 |
| reset | 0.156 | 0.145 | 0.375 | 40 |

Single-socket rate: 4476 steps/s. RSS 5996 kB, VmHWM 6080 kB. Eight parallel clients each reset and took one `[crane]` step in 16.9 ms wall. Source: `docs/bench/rust-wordle.json`.
