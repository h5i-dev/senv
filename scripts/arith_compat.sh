#!/usr/bin/env bash
# Build senv, serve custom Arith-v0, and run the unmodified TextArenaEnv client.
#
# Required:
#   OPENENV_SRC  checkout of meta-pytorch/OpenEnv at the revision in src/lib.rs
# Optional:
#   SENV_PYTHON  interpreter that can import textarena_env (default: python3)
#   SENV_BIND    listen address (default: 127.0.0.1:8784)

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

python="${SENV_PYTHON:-python3}"
bind="${SENV_BIND:-127.0.0.1:8784}"
base_url="http://${bind}"

cargo build --locked --bin senv
bin="${CARGO_TARGET_DIR:-$root/target}/debug/senv"

log="$(mktemp)"
trap 'kill "$server_pid" 2>/dev/null || true; rm -f "$log"' EXIT
"$bin" serve --bind "$bind" --env arith --arith-ops 3 --max-sessions 8 >"$log" 2>&1 &
server_pid=$!

for _ in $(seq 1 50); do
  if curl -fsS --max-time 0.5 "${base_url}/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "senv exited before /health" >&2
    cat "$log" >&2
    exit 1
  fi
  sleep 0.1
done

if ! curl -fsS --max-time 1 "${base_url}/health" >/dev/null 2>&1; then
  echo "senv did not answer ${base_url}/health" >&2
  cat "$log" >&2
  exit 1
fi

PYTHONPATH="${root}/scripts:${OPENENV_SRC}/src:${OPENENV_SRC}/envs${PYTHONPATH:+:$PYTHONPATH}" \
  SENV_BASE_URL="$base_url" \
  OPENENV_CLIENT="${OPENENV_SRC}/envs/textarena_env/client.py" \
  "$python" "$root/scripts/arith_client_compat.py"
