"""Typed exceptions — errors surface in the score's own stack frame.

The bridge maps JSON-RPC errors back to a small hierarchy so a score can
``except`` precisely, and so tracebacks point at the awaiting line of the
user's own code (the define-by-run debuggability bargain).
"""

from __future__ import annotations

__all__ = [
    "AskParseError",
    "BridgeClosedError",
    "H5iError",
    "OrchestraError",
    "ProtocolError",
    "RpcError",
]


class OrchestraError(Exception):
    """Base class for every error this SDK raises."""


class BridgeClosedError(OrchestraError):
    """The ``h5i orchestra serve`` process is gone (EOF, crash, or close).

    The run itself is durable: journaled steps live on the git-backed team
    event log, so re-running the same score resumes without re-executing
    completed agent turns.
    """


class ProtocolError(OrchestraError):
    """Malformed traffic or a handshake the two sides could not agree on."""


class RpcError(OrchestraError):
    """An error the server returned for one request."""

    def __init__(self, message: str, code: int, kind: str | None = None):
        super().__init__(message)
        self.code = code
        self.kind = kind


class H5iError(RpcError):
    """An ``H5iError`` from the Rust core (``code == -32000``).

    ``kind`` carries the coarse variant ("git", "metadata", "io", …).
    """


class AskParseError(OrchestraError):
    """``Agent.ask(parse=…)`` exhausted its attempts without a parseable reply."""

    def __init__(self, message: str, last_value: object = None):
        super().__init__(message)
        self.last_value = last_value


def error_from_payload(payload: dict) -> RpcError:
    """Map a JSON-RPC ``error`` object to the matching exception."""
    code = payload.get("code", -32603)
    message = payload.get("message", "unknown server error")
    data = payload.get("data") or {}
    kind = data.get("kind") if isinstance(data, dict) else None
    if code == -32000:
        return H5iError(message, code, kind)
    return RpcError(message, code, kind)
