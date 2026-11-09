#!/usr/bin/env python3
"""Measure one OpenEnv server. The server is already listening.

Environment:
  SENV_BASE_URL   required, for example http://127.0.0.1:8771
  SENV_BENCH      echo or wordle (default echo)
  SENV_PID        optional server pid; VmRSS and VmHWM are read after the run
  BENCH_OUT       optional directory for results.json

The client is this process. It uses the same WebSocket shapes as the official
clients and does not import OpenEnv, so the numbers are the server plus this
small client. They are one run on one machine.
"""

from __future__ import annotations

import json
import os
import statistics
import time
import urllib.request
from pathlib import Path

import websockets.sync.client as ws_client


def main() -> int:
    base = os.environ["SENV_BASE_URL"].rstrip("/")
    kind = os.environ.get("SENV_BENCH", "echo")
    steps = int(os.environ.get("SENV_BENCH_STEPS", "400"))
    episodes = int(os.environ.get("SENV_BENCH_EPISODES", "40"))
    sessions = int(os.environ.get("SENV_BENCH_SESSIONS", "16"))
    parallel = int(os.environ.get("SENV_BENCH_PARALLEL", "8"))
    ws_base = "ws" + base[len("http") :] if base.startswith("http") else base

    create_ms = []
    ids = []
    for _ in range(sessions):
        started = time.perf_counter()
        session_id = create_session(base)
        create_ms.append((time.perf_counter() - started) * 1000.0)
        ids.append(session_id)
    for session_id in ids:
        close_session(base, session_id)

    socket = connect(ws_base)
    try:
        if kind == "echo":
            reset_ms, step_ms = echo_run(socket, steps)
        elif kind == "wordle":
            reset_ms, step_ms = wordle_run(socket, episodes)
        else:
            raise SystemExit(f"unknown SENV_BENCH {kind}")
    finally:
        socket.close()

    parallel_ms = parallel_steps(ws_base, kind, parallel)

    result = {
        "base_url": base,
        "kind": kind,
        "steps_timed": len(step_ms),
        "resets_timed": len(reset_ms),
        "session_creates": len(create_ms),
        "parallel_clients": parallel,
        "create_ms": summary(create_ms),
        "reset_ms": summary(reset_ms),
        "step_ms": summary(step_ms),
        "step_per_s": (1000.0 * len(step_ms) / sum(step_ms)) if step_ms else None,
        "parallel_wall_ms": parallel_ms,
        "memory": read_memory(os.environ.get("SENV_PID")),
    }
    text = json.dumps(result, indent=2, sort_keys=True)
    print(text)
    out = os.environ.get("BENCH_OUT")
    if out:
        path = Path(out)
        path.mkdir(parents=True, exist_ok=True)
        (path / f"{kind}.json").write_text(text + "\n", encoding="utf-8")
    return 0


def echo_run(socket, steps: int) -> tuple[list[float], list[float]]:
    reset_ms = [call(socket, {"type": "reset", "data": {"episode_id": "bench", "seed": 1}})[0]]
    # One untimed step so the measured loop is not the first message.
    call(socket, echo_step("warmup"))
    step_ms = [call(socket, echo_step(f"m{i}"))[0] for i in range(steps)]
    return reset_ms, step_ms


def wordle_run(socket, episodes: int) -> tuple[list[float], list[float]]:
    reset_ms: list[float] = []
    step_ms: list[float] = []
    for episode in range(episodes):
        reset_ms.append(
            call(
                socket,
                {"type": "reset", "data": {"episode_id": f"w{episode}", "seed": episode}},
            )[0]
        )
        # Stop if the episode ends. A later guess would be EXECUTION_ERROR.
        for guess in ("crane", "stone", "house", "plant", "query", "bread"):
            elapsed, reply = call(socket, {"type": "step", "data": {"message": f"[{guess}]"}})
            step_ms.append(elapsed)
            if reply["data"].get("done") is True:
                break
    return reset_ms, step_ms


def parallel_steps(ws_base: str, kind: str, clients: int) -> float:
    import concurrent.futures

    def one(index: int) -> None:
        socket = connect(ws_base)
        try:
            call(socket, {"type": "reset", "data": {"episode_id": f"p{index}", "seed": index}})
            if kind == "echo":
                for i in range(20):
                    call(socket, echo_step(f"{index}-{i}"))
            else:
                call(socket, {"type": "step", "data": {"message": "[crane]"}})
        finally:
            socket.close()

    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=clients) as pool:
        list(pool.map(one, range(clients)))
    return (time.perf_counter() - started) * 1000.0


def echo_step(message: str) -> dict:
    return {
        "type": "step",
        "data": {
            "type": "call_tool",
            "tool_name": "echo_message",
            "arguments": {"message": message},
        },
    }


def connect(ws_base: str):
    session_id = create_session(ws_base.replace("ws://", "http://", 1).replace("wss://", "https://", 1))
    # create_session needs the http base. Callers pass ws_base; recover http above
    # only when this helper is used from parallel_steps. The sequential path
    # connects with a fresh session too.
    return ws_client.connect(
        f"{ws_base}/ws?session_id={session_id}",
        open_timeout=10,
        max_size=2**20,
        proxy=None,
    )


def call(socket, payload: dict) -> tuple[float, dict]:
    raw = json.dumps(payload)
    started = time.perf_counter()
    socket.send(raw)
    reply = json.loads(socket.recv(timeout=10))
    elapsed = (time.perf_counter() - started) * 1000.0
    if reply.get("type") == "error":
        raise RuntimeError(reply)
    return elapsed, reply


def create_session(base: str) -> str:
    body = rpc(base, "openenv/session/create", {})
    return body["result"]["session_id"]


def close_session(base: str, session_id: str) -> None:
    rpc(base, "openenv/session/close", {"session_id": session_id})


def rpc(base: str, method: str, params: dict) -> dict:
    request = urllib.request.Request(
        f"{base}/mcp",
        data=json.dumps(
            {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
        ).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        body = json.load(response)
    if "error" in body:
        raise RuntimeError(body["error"])
    return body


def summary(samples: list[float]) -> dict:
    ordered = sorted(samples)
    return {
        "n": len(samples),
        "mean": statistics.fmean(samples) if samples else None,
        "p50": pick(ordered, 0.50),
        "p99": pick(ordered, 0.99),
        "min": ordered[0] if ordered else None,
        "max": ordered[-1] if ordered else None,
    }


def pick(ordered: list[float], fraction: float) -> float | None:
    if not ordered:
        return None
    index = min(len(ordered) - 1, max(0, round(fraction * (len(ordered) - 1))))
    return ordered[index]


def read_memory(pid: str | None) -> dict | None:
    if not pid:
        return None
    fields = {}
    try:
        text = Path(f"/proc/{pid}/status").read_text(encoding="utf-8")
    except OSError:
        return None
    for line in text.splitlines():
        if line.startswith(("VmRSS:", "VmHWM:", "VmSize:")):
            key, value = line.split(":", 1)
            fields[key] = value.strip()
    return fields


if __name__ == "__main__":
    raise SystemExit(main())
