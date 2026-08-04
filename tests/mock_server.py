"""An in-process mock of `h5i orchestra serve` speaking the same protocol.

Unit tests wire a real :class:`h5i.orchestra._rpc.Bridge` (and a real
``Conductor``) to this mock over in-memory pipes, so everything except the
Rust process itself is exercised: framing, multiplexing, error mapping,
server→client launcher turns.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Awaitable, Callable
from typing import Any

from h5i.orchestra import PROTOCOL_VERSION
from h5i.orchestra._conductor import Conductor
from h5i.orchestra._rpc import Bridge

Handler = Callable[[dict], Any]


class MockError(Exception):
    """Raise inside a handler to make the mock return a JSON-RPC error."""

    def __init__(self, message: str, code: int = -32000, kind: str | None = "metadata"):
        super().__init__(message)
        self.code = code
        self.kind = kind


class FeedWriter:
    """Duck-typed StreamWriter that feeds a StreamReader — an in-memory pipe."""

    def __init__(self, into: asyncio.StreamReader):
        self._into = into

    def write(self, data: bytes) -> None:
        self._into.feed_data(data)

    async def drain(self) -> None:
        pass

    def close(self) -> None:
        try:
            self._into.feed_eof()
        except AssertionError:
            pass  # eof already fed


class MockOrchestra:
    """Scripted server. Register per-method handlers with :meth:`on`;
    ``initialize``/``conductor.launch``/``shutdown`` have defaults. Every
    request is recorded in :attr:`calls` for assertions."""

    def __init__(self):
        self.calls: list[tuple[str, dict]] = []
        self.launch_result = {"run_id": "testrun", "actor": "human", "replayed_steps": 0}
        self.hello = {
            "protocol_version": PROTOCOL_VERSION,
            "h5i_version": "0.0-mock",
            "capabilities": ["conductor.core", "agent.turns", "launcher.client"],
        }
        self._handlers: dict[str, Handler] = {}
        self._writer: FeedWriter | None = None
        self._next_id = 0
        self._pending: dict[str, asyncio.Future] = {}
        self._tasks: set[asyncio.Task] = set()

    def on(self, method: str, handler: Handler) -> None:
        self._handlers[method] = handler

    def calls_to(self, method: str) -> list[dict]:
        return [p for m, p in self.calls if m == method]

    # ── the server loop ─────────────────────────────────────────────────────

    async def serve(self, reader: asyncio.StreamReader, writer: FeedWriter) -> None:
        self._writer = writer
        while True:
            line = await reader.readline()
            if not line:
                break
            message = json.loads(line)
            if "method" in message:
                task = asyncio.ensure_future(self._handle(message))
                self._tasks.add(task)
                task.add_done_callback(self._tasks.discard)
            else:
                future = self._pending.pop(message.get("id"), None)
                if future is not None and not future.done():
                    future.set_result(message)

    async def _handle(self, message: dict) -> None:
        method = message["method"]
        params = message.get("params") or {}
        request_id = message.get("id")
        self.calls.append((method, params))
        try:
            handler = self._handlers.get(method) or self._default(method)
            result = handler(params)
            if asyncio.iscoroutine(result) or isinstance(result, Awaitable):
                result = await result
            reply = {"jsonrpc": "2.0", "id": request_id, "result": result}
        except MockError as e:
            error: dict[str, Any] = {"code": e.code, "message": str(e)}
            if e.kind is not None:
                error["data"] = {"kind": e.kind}
            reply = {"jsonrpc": "2.0", "id": request_id, "error": error}
        if request_id is not None:
            self.send(reply)

    def _default(self, method: str) -> Handler:
        if method == "initialize":
            return lambda p: dict(self.hello)
        if method == "conductor.launch":
            return lambda p: dict(self.launch_result)
        if method == "shutdown":
            return lambda p: None
        raise MockError(f"unknown method '{method}'", code=-32601, kind=None)

    # ── server→client requests (launcher.on_turn) ───────────────────────────

    def send(self, obj: dict) -> None:
        assert self._writer is not None
        self._writer.write((json.dumps(obj) + "\n").encode())

    async def request(self, method: str, params: dict) -> dict:
        """Issue a server→client request and await the raw reply message."""
        self._next_id += 1
        request_id = f"srv:{self._next_id}"
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        self._pending[request_id] = future
        self.send(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        )
        return await future


def connect(mock: MockOrchestra, *, on_request=None) -> tuple[Bridge, asyncio.Task]:
    """Wire a Bridge to the mock over in-memory pipes and start both."""
    client_to_server = asyncio.StreamReader()
    server_to_client = asyncio.StreamReader()
    bridge = Bridge(
        server_to_client, FeedWriter(client_to_server), on_request=on_request
    )
    bridge.start()
    server_task = asyncio.get_running_loop().create_task(
        mock.serve(client_to_server, FeedWriter(server_to_client))
    )
    return bridge, server_task


async def launch_conductor(mock: MockOrchestra, **kwargs) -> Conductor:
    """A real Conductor speaking to the mock (transport seam overridden)."""
    kwargs.setdefault("score_digest", None)
    run = kwargs.pop("run", "testrun")
    conductor = Conductor(".", run, **kwargs)

    async def factory() -> Bridge:
        bridge, task = connect(mock, on_request=conductor._serve_request)
        conductor._mock_server_task = task  # keep a handle for teardown
        return bridge

    conductor._spawn_bridge = factory  # type: ignore[method-assign]
    await conductor.launch()
    return conductor
