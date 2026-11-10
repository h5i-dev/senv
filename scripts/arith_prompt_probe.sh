#!/usr/bin/env bash
# Validation-only prompt probe. Does not touch the test split or the original
# experiment directory. Large JSON goes under SENV_PROBE_OUT.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

out="${SENV_PROBE_OUT:-/mnt/scratch/h5i-openenv/experiment-arith-prompt}"
mkdir -p "$out"
python="${SENV_PYTHON:-python3}"
openenv="${OPENENV_SRC:?OPENENV_SRC must point at the pinned OpenEnv checkout}"
export PYTHONPATH="${root}/scripts:${openenv}/src:${openenv}/envs${PYTHONPATH:+:$PYTHONPATH}"
export PYTHONUNBUFFERED=1
export HF_HOME="${HF_HOME:-/mnt/scratch/h5i-openenv/hf}"
export HUGGINGFACE_HUB_CACHE="${HUGGINGFACE_HUB_CACHE:-/mnt/scratch/h5i-openenv/hf/hub}"
export TOKENIZERS_PARALLELISM=false
export OPENENV_CLIENT="${openenv}/envs/textarena_env/client.py"

port="${SENV_ARITH_PORT:-8785}"
export SENV_BASE_URL="http://127.0.0.1:${port}"
bin="${CARGO_TARGET_DIR:-$root/target}/release/senv"
limit="${SENV_PROBE_LIMIT:-8}"

"$python" "$root/scripts/arith_prompt_probe.py" --self-test

server_pid=""
stop_server() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=""
  fi
}
trap stop_server EXIT

start_server() {
  local ops="$1"
  stop_server
  "$bin" serve --bind "127.0.0.1:${port}" --env arith --arith-ops "$ops" --max-sessions 2 \
    >"$out/server-ops${ops}.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 1 100); do
    if curl -fsS --max-time 0.5 "$SENV_BASE_URL/health" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$server_pid" 2>/dev/null; then
      echo "senv exited during startup" >&2
      cat "$out/server-ops${ops}.log" >&2
      exit 1
    fi
    sleep 0.1
  done
  echo "senv did not answer health" >&2
  exit 1
}

run_one() {
  local ops="$1" name="$2" model="$3" revision="$4" variant="$5" decode="$6"
  local dest="$out/ops${ops}-${name}-${variant}-${decode}.json"
  if [[ -f "$dest" ]]; then
    echo "skip $dest"
    return 0
  fi
  export SENV_MODEL_ID="$model"
  export SENV_MODEL_REVISION="$revision"
  "$python" "$root/scripts/arith_prompt_probe.py" \
    --variant "$variant" --decode "$decode" --limit "$limit" --out "$dest"
}

# Qwen first: the open question is whether it can follow a cleaner instruction.
start_server 2
run_one 2 qwen Qwen/Qwen2.5-0.5B-Instruct 7ae557604adf67be50417f59c2c2f167def9a775 server_prompt greedy
run_one 2 qwen Qwen/Qwen2.5-0.5B-Instruct 7ae557604adf67be50417f59c2c2f167def9a775 format_first greedy
run_one 2 qwen Qwen/Qwen2.5-0.5B-Instruct 7ae557604adf67be50417f59c2c2f167def9a775 format_example greedy
start_server 3
run_one 3 qwen Qwen/Qwen2.5-0.5B-Instruct 7ae557604adf67be50417f59c2c2f167def9a775 server_prompt greedy
run_one 3 qwen Qwen/Qwen2.5-0.5B-Instruct 7ae557604adf67be50417f59c2c2f167def9a775 format_first greedy
run_one 3 qwen Qwen/Qwen2.5-0.5B-Instruct 7ae557604adf67be50417f59c2c2f167def9a775 format_example greedy

# SmolLM2 greedy is the echo failure. One operator count is enough to see if the wrapper changes it.
start_server 2
run_one 2 smol HuggingFaceTB/SmolLM2-135M-Instruct 12fd25f77366fa6b3b4b768ec3050bf629380bac server_prompt greedy
run_one 2 smol HuggingFaceTB/SmolLM2-135M-Instruct 12fd25f77366fa6b3b4b768ec3050bf629380bac format_first greedy

echo "ARITH_PROMPT_PROBE_DONE"
