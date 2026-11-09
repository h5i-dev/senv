#!/usr/bin/env bash
# Build senv, serve Echo, and run the unmodified official Python EchoEnv client.
#
# Required:
#   OPENENV_SRC  checkout of meta-pytorch/OpenEnv at the revision in src/lib.rs
# Optional:
#   SENV_PYTHON  interpreter with pydantic, websockets, and httpx (default: python3)
#   SENV_BIND    listen address (default: 127.0.0.1:8765)
#
# Crate sources stay in this repository. Point CARGO_HOME and CARGO_TARGET_DIR
# at a large disk before invoking this on a small root filesystem.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if [[ -z "${OPENENV_SRC:-}" ]]; then
  echo "OPENENV_SRC must point at the pinned meta-pytorch/OpenEnv checkout" >&2
  exit 2
fi
if [[ ! -f "${OPENENV_SRC}/envs/echo_env/client.py" ]]; then
  echo "OPENENV_SRC does not contain envs/echo_env/client.py: ${OPENENV_SRC}" >&2
  exit 2
fi

python="${SENV_PYTHON:-python3}"
bind="${SENV_BIND:-127.0.0.1:8765}"
base_url="http://${bind}"

cargo build --locked --bin senv
bin="${CARGO_TARGET_DIR:-$root/target}/debug/senv"

log="$(mktemp)"
trap 'kill "$server_pid" 2>/dev/null || true; rm -f "$log"' EXIT
"$bin" serve --bind "$bind" --max-sessions 8 >"$log" 2>&1 &
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

PYTHONPATH="${OPENENV_SRC}/src:${OPENENV_SRC}/envs${PYTHONPATH:+:$PYTHONPATH}" \
  SENV_BASE_URL="$base_url" \
  OPENENV_CLIENT="${OPENENV_SRC}/envs/echo_env/client.py" \
  "$python" "$root/scripts/echo_client_compat.py"
