#!/usr/bin/env bash
# Measure senv and, for Echo, the pinned OpenEnv Python server.
#
# Optional:
#   OPENENV_SRC   OpenEnv checkout. Required for the Python Echo server.
#   SENV_PYTHON   interpreter with websockets (default: python3)
#   BENCH_OUT     directory for json results (default: ./bench-out)
#
# Point CARGO_HOME and CARGO_TARGET_DIR at a large disk before a cold build.
# This script does not start Docker.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
python="${SENV_PYTHON:-python3}"
out="${BENCH_OUT:-$root/bench-out}"
mkdir -p "$out"

{
  date -u +"%Y-%m-%dT%H:%M:%SZ"
  uname -a
  nproc
  grep -m1 "model name" /proc/cpuinfo || true
  free -h
  rustc --version
  "$python" -V
  nvidia-smi -L || true
} >"$out/machine.txt"

cargo build --locked --release --bin senv
bin="${CARGO_TARGET_DIR:-$root/target}/release/senv"

cleanup() {
  if [[ -n "${rust_pid:-}" ]]; then kill "$rust_pid" 2>/dev/null || true; fi
  if [[ -n "${py_pid:-}" ]]; then kill "$py_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT

wait_health() {
  local url="$1" pid="$2"
  for _ in $(seq 1 200); do
    if curl -fsS --max-time 0.5 "$url/health" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "server $pid exited before $url/health" >&2
      return 1
    fi
    sleep 0.25
  done
  echo "timed out waiting for $url/health" >&2
  return 1
}

run_one() {
  local kind="$1" base="$2" pid="$3" copy_as="$4"
  SENV_BASE_URL="$base" SENV_BENCH="$kind" SENV_PID="$pid" BENCH_OUT="$out" \
    "$python" "$root/scripts/bench.py" >"$out/${copy_as%.json}-run.log"
  cp "$out/${kind}.json" "$out/${copy_as}"
}

echo "rust echo" | tee "$out/progress.txt"
"$bin" serve --bind 127.0.0.1:8771 --env echo --max-sessions 32 >"$out/rust-echo.log" 2>&1 &
rust_pid=$!
wait_health http://127.0.0.1:8771 "$rust_pid"
run_one echo http://127.0.0.1:8771 "$rust_pid" rust-echo.json
kill "$rust_pid" || true
wait "$rust_pid" 2>/dev/null || true
unset rust_pid

echo "rust wordle" | tee -a "$out/progress.txt"
"$bin" serve --bind 127.0.0.1:8772 --env wordle --max-sessions 32 >"$out/rust-wordle.log" 2>&1 &
rust_pid=$!
wait_health http://127.0.0.1:8772 "$rust_pid"
run_one wordle http://127.0.0.1:8772 "$rust_pid" rust-wordle.json
kill "$rust_pid" || true
wait "$rust_pid" 2>/dev/null || true
unset rust_pid

if [[ -n "${OPENENV_SRC:-}" && -f "${OPENENV_SRC}/envs/echo_env/server/app.py" ]]; then
  echo "python echo" | tee -a "$out/progress.txt"
  PYTHONPATH="${OPENENV_SRC}/src:${OPENENV_SRC}/envs${PYTHONPATH:+:$PYTHONPATH}" \
    MAX_CONCURRENT_ENVS=32 \
    "$python" -m uvicorn echo_env.server.app:app --host 127.0.0.1 --port 8773 \
    >"$out/python-echo.log" 2>&1 &
  py_pid=$!
  wait_health http://127.0.0.1:8773 "$py_pid"
  # The Python server is Echo. Write its json beside the Rust echo json.
  SENV_BASE_URL=http://127.0.0.1:8773 SENV_BENCH=echo SENV_PID="$py_pid" BENCH_OUT="$out" \
    "$python" "$root/scripts/bench.py" >"$out/python-echo-run.log"
  mv "$out/echo.json" "$out/python-echo.json"
  kill "$py_pid" || true
  wait "$py_pid" 2>/dev/null || true
  unset py_pid
else
  echo "OPENENV_SRC unset; skipped the Python Echo server" | tee -a "$out/progress.txt"
fi

echo "bench outputs in $out"
