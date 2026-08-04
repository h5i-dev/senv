"""Verdict policies.

A policy is either a **built-in** (named, evaluated inside the h5i binary —
byte-for-byte the same rule the CLI's ``team finalize`` applies) or **any
Python callable** ``(Run) -> Verdict`` (sync or async). Custom policies run
in your process with the folded run snapshot — an LLM judge is just a policy
that ``ask``s inside — and the verdict they return is recorded through the
same journaled, auditable path as the built-ins.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from dataclasses import dataclass

from ._types import Run, Verdict

__all__ = ["BuiltinPolicy", "Policy", "tests_then_smallest_diff"]


@dataclass(frozen=True)
class BuiltinPolicy:
    """A policy evaluated server-side, referenced by name."""

    name: str


#: Today's ``h5i team finalize`` rule: keep candidates whose latest
#: verification applies cleanly and passes tests, refuse divergent verifier
#: commands, pick the smallest diff.
tests_then_smallest_diff = BuiltinPolicy("tests_then_smallest_diff")

Policy = BuiltinPolicy | Callable[[Run], Verdict | dict | Awaitable[Verdict | dict]]
