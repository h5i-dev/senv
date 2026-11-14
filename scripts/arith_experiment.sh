#!/usr/bin/env bash
# Pilot, freeze, and train custom Arith-v0 through the live Rust server.
#
# Large outputs go to SENV_EXPERIMENT_OUT (default /mnt/scratch/h5i-openenv/experiment-arith).
# SENV_SMOKE=1 runs one model, two validation episodes, and two training episodes.
# The test split is evaluated only after the operator count is frozen.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

out="${SENV_EXPERIMENT_OUT:-/mnt/scratch/h5i-openenv/experiment-arith}"
mkdir -p "$out"
python="${SENV_PYTHON:-python3}"
openenv="${OPENENV_SRC:?OPENENV_SRC must point at the pinned OpenEnv checkout}"
export PYTHONPATH="${root}/scripts:${openenv}/src:${openenv}/envs${PYTHONPATH:+:$PYTHONPATH}"
export PYTHONUNBUFFERED=1
export HF_HOME="${HF_HOME:-/mnt/scratch/h5i-openenv/hf}"
export HUGGINGFACE_HUB_CACHE="${HUGGINGFACE_HUB_CACHE:-/mnt/scratch/h5i-openenv/hf/hub}"
export TMPDIR="${TMPDIR:-/mnt/scratch/h5i-openenv/tmp}"
export OPENENV_CLIENT="${openenv}/envs/textarena_env/client.py"
export TOKENIZERS_PARALLELISM=false
mkdir -p "$TMPDIR"

port="${SENV_ARITH_PORT:-8783}"
export SENV_BASE_URL="http://127.0.0.1:${port}"

if [[ "${SENV_SMOKE:-0}" == "1" ]]; then
  limit=2
  train_episodes=2
  checkpoint_every=1
  ops_list=(3)
  models=("smol|HuggingFaceTB/SmolLM2-135M-Instruct|12fd25f77366fa6b3b4b768ec3050bf629380bac")
  torch_seeds=(1)
  do_test=0
else
  limit=0
  train_episodes=200
  checkpoint_every=50
  ops_list=(2 3 4)
  models=(
    "smol|HuggingFaceTB/SmolLM2-135M-Instruct|12fd25f77366fa6b3b4b768ec3050bf629380bac"
    "qwen|Qwen/Qwen2.5-0.5B-Instruct|7ae557604adf67be50417f59c2c2f167def9a775"
  )
  torch_seeds=(1 2)
  do_test=1
fi

cargo build --locked --release --bin senv
bin="${CARGO_TARGET_DIR:-$root/target}/release/senv"
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
  "$bin" serve --bind "127.0.0.1:${port}" --env arith --arith-ops "$ops" --max-sessions 4 \
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
  cat "$out/server-ops${ops}.log" >&2
  exit 1
}

run_py() {
  "$python" "$root/scripts/arith_experiment.py" "$@"
}

"$python" "$root/scripts/arith_task.py"

for spec in "${models[@]}"; do
  IFS='|' read -r name model_id revision <<<"$spec"
  export SENV_MODEL_ID="$model_id"
  export SENV_MODEL_REVISION="$revision"
  mark="$out/validate-${name}.json"
  if [[ ! -f "$mark" ]]; then
    run_py --phase validate --out "$mark"
  fi
done

for ops in "${ops_list[@]}"; do
  start_server "$ops"
  for spec in "${models[@]}"; do
    IFS='|' read -r name model_id revision <<<"$spec"
    export SENV_MODEL_ID="$model_id"
    export SENV_MODEL_REVISION="$revision"
    dest="$out/pilot-ops${ops}-${name}.json"
    if [[ ! -f "$dest" ]]; then
      run_py --phase pretrained --split val --decode sample --limit "$limit" --out "$dest"
    fi
  done
done

if [[ "$do_test" == "1" ]]; then
  if [[ ! -f "$out/choice.json" ]]; then
    run_py --phase choose --pilot-dir "$out" --out "$out/choice.json"
  fi
  frozen="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["ops"])' "$out/choice.json")"
else
  frozen=3
  printf '%s\n' '{"ops":3,"rule":"smoke"}' >"$out/choice.json"
fi
echo "frozen arith ops: $frozen"
export SENV_ARITH_FROZEN=1
start_server "$frozen"

if [[ ! -f "$out/oracle.json" ]]; then
  run_py --phase oracle --limit "${limit:-5}" --out "$out/oracle.json"
fi

if [[ "$do_test" == "1" ]]; then
  for policy in answer0 random; do
    dest="$out/baseline-${policy}.json"
    if [[ ! -f "$dest" ]]; then
      run_py --phase baseline --policy "$policy" --split test --out "$dest"
    fi
  done
  for spec in "${models[@]}"; do
    IFS='|' read -r name model_id revision <<<"$spec"
    export SENV_MODEL_ID="$model_id"
    export SENV_MODEL_REVISION="$revision"
    for decode in greedy sample; do
      dest="$out/pretrained-${name}-${decode}.json"
      if [[ ! -f "$dest" ]]; then
        run_py --phase pretrained --split test --decode "$decode" --out "$dest"
      fi
    done
    for torch_seed in "${torch_seeds[@]}"; do
      dest="$out/train-${name}-seed${torch_seed}.json"
      adapter="$out/adapter-${name}-seed${torch_seed}"
      if [[ ! -f "$dest" ]]; then
        SENV_EVAL_TEST=1 run_py --phase train --torch-seed "$torch_seed" \
          --train-episodes "$train_episodes" --checkpoint-every "$checkpoint_every" \
          --adapter-dir "$adapter" --out "$dest"
      fi
    done
  done
else
  IFS='|' read -r name model_id revision <<<"${models[0]}"
  export SENV_MODEL_ID="$model_id"
  export SENV_MODEL_REVISION="$revision"
  dest="$out/smoke-train-${name}.json"
  if [[ ! -f "$dest" ]]; then
    run_py --phase train --torch-seed 1 --limit "$limit" \
      --train-episodes "$train_episodes" --checkpoint-every "$checkpoint_every" \
      --out "$dest"
  fi
fi

echo "ARITH_EXPERIMENT_DONE"
