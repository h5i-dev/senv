"""h5i.orchestra — define-by-run agent orchestration for Python.

A score is an ordinary async Python program: ``if``, ``for`` and
``asyncio.gather`` are the orchestration language, every effectful step is
journaled on the git-backed team event log, and a killed score resumes by
simply running the same file again — completed agent turns are never
re-executed (and never re-paid).

    import asyncio
    from h5i.orchestra import Conductor

    async def main():
        async with Conductor(".", "fix-auth") as c:
            claude = await c.hire("claude", runtime="claude")
            codex = await c.hire("codex", runtime="codex")
            task = "implement `h5i pull` mirroring `h5i push`"
            a, b = await asyncio.gather(claude.work(task), codex.work(task))
            await c.freeze()
            await asyncio.gather(codex.review(a), claude.review(b))
            await c.verify(a, ["cargo", "test", "--quiet"])
            await c.verify(b, ["cargo", "test", "--quiet"])
            verdict = await c.judge()
            print("winner:", verdict.selected_submission)

    asyncio.run(main())

The SDK drives a child ``h5i orchestra serve`` process over stdio JSON-RPC —
no daemon, no socket, no native extension. It is stdlib-only; the heavy
lifting (sandboxed envs, the journal, turn dispatch, neutral verification)
lives in the ``h5i`` binary.
"""

from . import patterns, policy
from ._conductor import PROTOCOL_VERSION, Agent, Conductor, Scope
from ._errors import (
    AskParseError,
    BridgeClosedError,
    H5iError,
    OrchestraError,
    ProtocolError,
    RpcError,
)
from ._herdr import HerdrLauncher
from ._types import (
    ApplyResult,
    Artifact,
    CompareRow,
    GateAnswer,
    Review,
    Run,
    RunAgent,
    TurnContext,
    Verdict,
    Verification,
)
from .patterns import approves

__version__ = "0.1.0"

__all__ = [
    "PROTOCOL_VERSION",
    "Agent",
    "ApplyResult",
    "Artifact",
    "AskParseError",
    "BridgeClosedError",
    "CompareRow",
    "Conductor",
    "GateAnswer",
    "H5iError",
    "HerdrLauncher",
    "OrchestraError",
    "ProtocolError",
    "Review",
    "RpcError",
    "Run",
    "RunAgent",
    "Scope",
    "TurnContext",
    "Verdict",
    "Verification",
    "__version__",
    "approves",
    "patterns",
    "policy",
]
