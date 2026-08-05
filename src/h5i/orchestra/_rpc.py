"""The bridge transport: a child `h5i orchestra serve` spoken to over stdio.

One JSON object per line, JSON-RPC 2.0 shaped. Requests multiplex by id —
two `await`s in flight are two in-flight requests, which is what lets
``asyncio.gather(claude.work(…), codex.work(…))`` run both turns
concurrently. The one server→client request is ``launcher.on_turn``; it is
dispatched to the ``on_request`` callback and answered with the callback's
outcome.

Not a daemon: no socket, no port, no auth. The child's only I/O is this
pipe pair; its stderr is inherited so ``H5I_LOG`` diagnostics land in the
score's own terminal.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
from collections.abc import Awaitable, Callable, Sequence
from typing import Any

from ._errors import (
    BridgeClosedError,
    OrchestraError,
    ProtocolError,
    error_from_payload,
)

__all__ = ["Bridge", "resolve_h5i_bin"]

OnRequest = Callable[[str, dict], Awaitable[Any]]


def resolve_h5i_bin(explicit: str | os.PathLike[str] | None = None) -> str:
    """Locate the ``h5i`` binary: explicit argument > ``$H5I`` > ``PATH``."""
    if explicit:
        return os.fspath(explicit)
    env = os.environ.get("H5I")
    if env:
        return env
    found = shutil.which("h5i")
    if found:
        return found
    raise OrchestraError(
        "h5i binary not found — install h5i (cargo install --path <h5i repo>), "
        "put it on PATH, set $H5I, or pass h5i_bin=..."
    )


class Bridge:
    """One JSON-RPC session over a reader/writer pair.

    Use :meth:`spawn` for the real subprocess; tests hand in in-memory
    streams directly.
    """

    def __init__(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        *,
        on_request: OnRequest | None = None,
        process: asyncio.subprocess.Process | None = None,
    ):
        self._reader = reader
        self._writer = writer
        self._on_request = on_request
        self._process = process
        self._pending: dict[int, asyncio.Future[Any]] = {}
        self._next_id = 0
        self._write_lock = asyncio.Lock()
        self._closed: BaseException | None = None
        self._reader_task: asyncio.Task[None] | None = None
        self._request_tasks: set[asyncio.Task[None]] = set()

    @classmethod
    async def spawn(
        cls,
        argv: Sequence[str],
        *,
        cwd: str | None = None,
        on_request: OnRequest | None = None,
    ) -> Bridge:
        try:
            process = await asyncio.create_subprocess_exec(
                *argv,
                cwd=cwd,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=None,  # inherit: server logs belong in the score's terminal
            )
        except OSError as e:
            raise BridgeClosedError(f"failed to spawn {argv[0]!r}: {e}") from e
        assert process.stdout is not None and process.stdin is not None
        bridge = cls(
            process.stdout, process.stdin, on_request=on_request, process=process
        )
        bridge.start()
        return bridge

    def start(self) -> None:
        if self._reader_task is None:
            self._reader_task = asyncio.get_running_loop().create_task(
                self._read_loop(), name="h5i-orchestra-bridge-reader"
            )

    # ── requests ────────────────────────────────────────────────────────────

    async def request(self, method: str, params: dict | None = None) -> Any:
        """Send one request and await its response.

        Raises the mapped server error, or :class:`BridgeClosedError` if the
        bridge dies while the request is in flight.
        """
        if self._closed is not None:
            raise self._closed_error()
        self._next_id += 1
        request_id = self._next_id
        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._pending[request_id] = future
        try:
            await self._write(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params or {},
                }
            )
            return await future
        finally:
            self._pending.pop(request_id, None)

    async def notify_close(self, *, graceful_timeout: float = 5.0) -> None:
        """Shut the bridge down: polite ``shutdown``, then EOF, then SIGKILL."""
        if self._closed is None:
            try:
                await asyncio.wait_for(
                    self.request("shutdown", {}), timeout=graceful_timeout
                )
            except (OrchestraError, asyncio.TimeoutError):
                pass
        self._closed = self._closed or BridgeClosedError("bridge closed")
        try:
            self._writer.close()
        except Exception:  # noqa: BLE001, S110
            pass
        if self._process is not None:
            try:
                await asyncio.wait_for(self._process.wait(), timeout=graceful_timeout)
            except asyncio.TimeoutError:
                self._process.kill()
                await self._process.wait()
        if self._reader_task is not None:
            self._reader_task.cancel()
            try:
                await self._reader_task
            except asyncio.CancelledError:
                pass
        self._fail_pending()

    # ── internals ───────────────────────────────────────────────────────────

    async def _write(self, message: dict) -> None:
        line = json.dumps(message, separators=(",", ":")) + "\n"
        async with self._write_lock:
            try:
                self._writer.write(line.encode("utf-8"))
                await self._writer.drain()
            except (ConnectionError, RuntimeError, OSError) as e:
                self._closed = self._closed or BridgeClosedError(
                    f"bridge pipe closed while writing: {e}"
                )
                raise self._closed_error() from e

    async def _read_loop(self) -> None:
        try:
            while True:
                line = await self._reader.readline()
                if not line:
                    break
                line = line.strip()
                if not line:
                    continue
                try:
                    message = json.loads(line)
                except json.JSONDecodeError as e:
                    self._closed = ProtocolError(
                        f"non-JSON line on the bridge's stdout (is something "
                        f"printing to stdout?): {line[:200]!r} ({e})"
                    )
                    break
                self._route(message)
        except (asyncio.CancelledError, ConnectionError, OSError):
            pass
        finally:
            if self._closed is None:
                returncode = self._process.returncode if self._process else None
                self._closed = BridgeClosedError(
                    "h5i orchestra serve exited"
                    + (f" with code {returncode}" if returncode is not None else "")
                    + " — check stderr above; the run's journal is durable, "
                    "re-running the score resumes it"
                )
            self._fail_pending()

    def _route(self, message: dict) -> None:
        if not isinstance(message, dict):
            return
        method = message.get("method")
        if isinstance(method, str):
            task = asyncio.get_running_loop().create_task(
                self._serve_request(message), name="h5i-orchestra-server-request"
            )
            self._request_tasks.add(task)
            task.add_done_callback(self._request_tasks.discard)
            return
        request_id = message.get("id")
        future = self._pending.get(request_id) if request_id is not None else None
        if future is None or future.done():
            return
        if "error" in message and message["error"] is not None:
            future.set_exception(error_from_payload(message["error"]))
        else:
            future.set_result(message.get("result"))

    async def _serve_request(self, message: dict) -> None:
        """Answer a server→client request (``launcher.on_turn``)."""
        request_id = message.get("id")
        method = message["method"]
        params = message.get("params") or {}
        try:
            if self._on_request is None:
                raise OrchestraError(
                    f"server requested {method!r} but no handler is installed "
                    "(pass on_turn=... to Conductor)"
                )
            result = await self._on_request(method, params)
            reply: dict[str, Any] = {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": result if result is not None else {},
            }
        except BaseException as e:  # noqa: BLE001 — must answer, whatever happened
            reply = {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32000, "message": f"{type(e).__name__}: {e}"},
            }
        if request_id is not None and self._closed is None:
            try:
                await self._write(reply)
            except OrchestraError:
                pass

    def _fail_pending(self) -> None:
        error = self._closed_error()
        for future in list(self._pending.values()):
            if not future.done():
                future.set_exception(error)
        self._pending.clear()

    def _closed_error(self) -> BaseException:
        return self._closed or BridgeClosedError("bridge closed")
