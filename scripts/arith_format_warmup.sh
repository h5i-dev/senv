#!/usr/bin/env bash
# Separate answer-format warmup. Operator count 2 was chosen from the validation
# pilots, where it was the only setting with a pretrained success. The test
# split is not evaluated. This is not the frozen ops=3 run.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

out="${SENV_WARMUP_OUT:-/mnt/scratch/h5i-openenv/experiment-arith-warmup}"
mkdir -p "$out"
python="${SENV_PYTHON:-python3}"
openenv="${OPENENV_SRC:?OPENENV_SRC must point at the pinned OpenEnv checkout}"
export PYTHONPATH="${root}/scripts:${openenv}/src:${openenv}/envs${PYTHONPATH:+:$PYTHONPATH}"
export PYTHONUNBUFFERED=1
export HF_HOME="${HF_HOME:-/mnt/scratch/h5i-openenv/hf}"
export HUGGINGFACE_HUB_CACHE="${HUGGINGFACE_HUB_CACHE:-/mnt/scratch/h5i-openenv/hf/hub}"
export TOKENIZERS_PARALLELISM=false
export OPENENV_CLIENT="${openenv}/envs/textarena_env/client.py"
unset SENV_EVAL_TEST
unset SENV_ARITH_FROZEN

port="${SENV_ARITH_PORT:-8786}"
export SENV_BASE_URL="http://127.0.0.1:${port}"
export SENV_MODEL_ID="Qwen/Qwen2.5-0.5B-Instruct"
export SENV_MODEL_REVISION="7ae557604adf67be50417f59c2c2f167def9a775"
bin="${CARGO_TARGET_DIR:-$root/target}/release/senv"
episodes="${SENV_WARMUP_EPISODES:-40}"
bonus="${SENV_ANSWER_BONUS:-0.5}"

cat >"$out/label.json" <<EOF
{
  "experiment": "arith-answer-format-warmup",
  "ops": 2,
  "ops_reason": "validation pilots only; ops 2 had the only pretrained success",
  "model_id": "${SENV_MODEL_ID}",
  "model_revision": "${SENV_MODEL_REVISION}",
  "train_episodes": ${episodes},
  "answer_bonus": ${bonus},
  "baseline_init": 0,
  "baseline_reason": "answer bonus is at most 0.5; a 0.25 prior made every advantage negative",
  "test_split_used": false,
  "original_frozen_ops": 3
}
EOF

"$python" "$root/scripts/arith_task.py"

server_pid=""
stop_server() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=""
  fi
}
trap stop_server EXIT

"$bin" serve --bind "127.0.0.1:${port}" --env arith --arith-ops 2 --max-sessions 2 \
  >"$out/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do
  if curl -fsS --max-time 0.5 "$SENV_BASE_URL/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "senv exited during startup" >&2
    cat "$out/server.log" >&2
    exit 1
  fi
  sleep 0.1
done

dest="$out/train-qwen-seed1.json"
if [[ ! -f "$dest" ]]; then
  "$python" "$root/scripts/arith_experiment.py" \
    --phase train --torch-seed 1 \
    --train-episodes "$episodes" --checkpoint-every 20 \
    --answer-bonus "$bonus" \
    --adapter-dir "$out/adapter-qwen-seed1" \
    --out "$dest"
fi
echo "ARITH_WARMUP_DONE"
