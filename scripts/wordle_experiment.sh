#!/usr/bin/env bash
# Serve Wordle and run the small-model experiment through TextArenaEnv.
#
# Required:
#   OPENENV_SRC       pinned OpenEnv checkout
#   EXPERIMENT_OUT    directory for results.json and the server log
# Optional:
#   SENV_PYTHON       interpreter with torch, transformers, and OpenEnv deps
#   SENV_BIN          senv binary (default: $CARGO_TARGET_DIR/release/senv)
#   SENV_BIND         listen address (default: 127.0.0.1:8780)
#   HF_HOME           Hugging Face cache (set this to a large disk)
#
# The script does not download Rust crates and does not start Docker.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if [[ -z "${OPENENV_SRC:-}" ]]; then
  echo "OPENENV_SRC must point at the pinned meta-pytorch/OpenEnv checkout" >&2
  exit 2
fi
if [[ ! -f "${OPENENV_SRC}/envs/textarena_env/client.py" ]]; then
  echo "OPENENV_SRC does not contain envs/textarena_env/client.py: ${OPENENV_SRC}" >&2
  exit 2
fi
if [[ -z "${EXPERIMENT_OUT:-}" ]]; then
  echo "EXPERIMENT_OUT is required" >&2
  exit 2
fi

python="${SENV_PYTHON:-python3}"
bind="${SENV_BIND:-127.0.0.1:8780}"
base_url="http://${bind}"
bin="${SENV_BIN:-${CARGO_TARGET_DIR:-$root/target}/release/senv}"
if [[ ! -x "$bin" ]]; then
  echo "senv binary is missing at $bin" >&2
  exit 2
fi

mkdir -p "$EXPERIMENT_OUT"
export PYTHONPATH="${OPENENV_SRC}/src:${OPENENV_SRC}/envs${PYTHONPATH:+:$PYTHONPATH}"
export SENV_BASE_URL="$base_url"
export SENV_EXPERIMENT_OUT="$EXPERIMENT_OUT"
export TOKENIZERS_PARALLELISM=false

cleanup() {
  if [[ -n "${server_pid:-}" ]]; then
    kill "$server_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

"$bin" serve --bind "$bind" --env wordle --max-sessions 4 >"$EXPERIMENT_OUT/server.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 100); do
  if curl -fsS --max-time 0.5 "${base_url}/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "senv exited before ${base_url}/health" >&2
    exit 1
  fi
  sleep 0.2
done

"$python" "$root/scripts/wordle_experiment.py"
echo "experiment results in $EXPERIMENT_OUT/results.json"
