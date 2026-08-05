"""Typed views over the run's recorded objects.

Every dataclass keeps the server's full JSON payload in ``raw`` and sends it
back verbatim when the object crosses the bridge again (``verify(artifact)``,
``review(artifact)``, …) — so fields this SDK version doesn't know about
survive the round trip, and the SDK never lags the binary on data shape.

Objects a *score* constructs itself (a custom ``Verdict``, a merged
``Review``) are plain constructible dataclasses with a ``to_payload()``.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import Any

__all__ = [
    "ApplyResult",
    "Artifact",
    "CompareRow",
    "GateAnswer",
    "Review",
    "Run",
    "RunAgent",
    "TurnContext",
    "Verdict",
    "Verification",
    "approves_text",
]


def _s(raw: Mapping[str, Any], key: str, default: str = "") -> str:
    v = raw.get(key)
    return v if isinstance(v, str) else default


def _i(raw: Mapping[str, Any], key: str, default: int = 0) -> int:
    v = raw.get(key)
    return v if isinstance(v, int) else default


_APPROVAL_TOKENS = frozenset({"APPROVE", "APPROVED", "LGTM", "YES", "OK"})
_APPROVAL_LABELS = frozenset({"verdict", "decision", "result", "review", "status"})


def approves_text(body: str) -> bool:
    """The approval convention, ported verbatim from the Rust eDSL.

    Look at the first non-empty line, strip one leading
    ``Verdict:``/``Decision:``/… label, and check the first remaining word
    against the approval set. Conservative: an approval token must *lead* the
    (delabeled) first line, so "I can't approve this" does not count.
    """
    line = next((ln.strip() for ln in body.splitlines() if ln.strip()), None)
    if line is None:
        return False
    rest = line
    if ":" in line:
        label, after = line.split(":", 1)
        if label.strip().lower() in _APPROVAL_LABELS:
            rest = after.strip()
    first = next(iter(rest.split()), None)
    if first is None:
        return False

    def _alnum(ch: str) -> bool:
        return ch.isascii() and ch.isalnum()

    # Trim non-alphanumerics at the token edges only (matching the Rust
    # `trim_matches`) — "**APPROVE**" counts, "AP-PROVE" does not.
    start = 0
    end = len(first)
    while start < end and not _alnum(first[start]):
        start += 1
    while end > start and not _alnum(first[end - 1]):
        end -= 1
    return first[start:end].upper() in _APPROVAL_TOKENS


@dataclass(frozen=True)
class Artifact:
    """One submitted candidate (``TeamArtifact``)."""

    id: str
    owner_agent: str
    round: int
    env_id: str
    commit_oid: str
    tree_oid: str
    files_changed: int
    insertions: int
    deletions: int
    submitted_at: str
    summary: str | None
    independent: bool
    influence_artifact_ids: tuple[str, ...]
    raw: Mapping[str, Any] = field(repr=False)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> Artifact:
        return cls(
            id=_s(raw, "id"),
            owner_agent=_s(raw, "owner_agent"),
            round=_i(raw, "round"),
            env_id=_s(raw, "env_id"),
            commit_oid=_s(raw, "commit_oid"),
            tree_oid=_s(raw, "tree_oid"),
            files_changed=_i(raw, "files_changed"),
            insertions=_i(raw, "insertions"),
            deletions=_i(raw, "deletions"),
            submitted_at=_s(raw, "submitted_at"),
            summary=raw.get("summary"),
            independent=bool(raw.get("independent", False)),
            influence_artifact_ids=tuple(raw.get("influence_artifact_ids") or ()),
            raw=dict(raw),
        )

    def to_payload(self) -> Mapping[str, Any]:
        return self.raw


@dataclass(frozen=True)
class Review:
    """A posted review. Constructible — patterns merge several into one."""

    reviewer: str
    target: str
    round: int
    body: str
    referenced_artifacts: tuple[str, ...] = ()
    raw: Mapping[str, Any] | None = field(default=None, repr=False)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> Review:
        return cls(
            reviewer=_s(raw, "reviewer"),
            target=_s(raw, "target"),
            round=_i(raw, "round", 1),
            body=_s(raw, "body"),
            referenced_artifacts=tuple(raw.get("referenced_artifacts") or ()),
            raw=dict(raw),
        )

    def to_payload(self) -> Mapping[str, Any]:
        if self.raw is not None:
            return self.raw
        return {
            "reviewer": self.reviewer,
            "target": self.target,
            "round": self.round,
            "body": self.body,
            "referenced_artifacts": list(self.referenced_artifacts),
        }

    @property
    def approved(self) -> bool:
        return approves_text(self.body)


@dataclass(frozen=True)
class Verification:
    """A neutral re-execution of a candidate (``TeamVerification``)."""

    id: str
    submission_id: str
    owner_agent: str
    round: int
    command: tuple[str, ...]
    applies_cleanly: bool
    tests_passed: bool
    isolation: str
    capture_id: str | None
    failure: str | None
    #: sealed-overlay mode (non-None = sealed): the submission id whose
    #: base..commit diff was overlaid over the candidate before the command
    #: ran — the candidate's edits to those paths could not weaken the check
    sealed_from: str | None
    #: sealed mode only: tree OID of the sealing submission (the content
    #: digest the verdict compares across candidates)
    sealed_tree_oid: str | None
    #: sealed mode only: the paths pinned by the overlay
    sealed_paths: tuple[str, ...]
    #: sealed paths where a candidate edit was discarded by the overlay —
    #: tamper evidence, surfaced rather than silently dropped
    sealed_overridden: tuple[str, ...]
    raw: Mapping[str, Any] = field(repr=False)

    @property
    def sealed(self) -> bool:
        return self.sealed_from is not None

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> Verification:
        return cls(
            id=_s(raw, "id"),
            submission_id=_s(raw, "submission_id"),
            owner_agent=_s(raw, "owner_agent"),
            round=_i(raw, "round"),
            command=tuple(raw.get("command") or ()),
            applies_cleanly=bool(raw.get("applies_cleanly", False)),
            tests_passed=bool(raw.get("tests_passed", False)),
            isolation=_s(raw, "isolation", "unknown"),
            capture_id=raw.get("capture_id"),
            failure=raw.get("failure"),
            sealed_from=raw.get("sealed_from"),
            sealed_tree_oid=raw.get("sealed_tree_oid"),
            sealed_paths=tuple(raw.get("sealed_paths") or ()),
            sealed_overridden=tuple(raw.get("sealed_overridden") or ()),
            raw=dict(raw),
        )


@dataclass(frozen=True)
class Verdict:
    """A recorded decision. Constructible — custom policies build one."""

    method: str
    decided_by: str
    selected_submission: str | None = None
    can_auto_apply: bool = False
    reasons: tuple[str, ...] = ()
    raw: Mapping[str, Any] | None = field(default=None, repr=False)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> Verdict:
        return cls(
            method=_s(raw, "method"),
            decided_by=_s(raw, "decided_by"),
            selected_submission=raw.get("selected_submission"),
            can_auto_apply=bool(raw.get("can_auto_apply", False)),
            reasons=tuple(raw.get("reasons") or ()),
            raw=dict(raw),
        )

    def to_payload(self) -> Mapping[str, Any]:
        if self.raw is not None:
            return self.raw
        return {
            "method": self.method,
            "decided_by": self.decided_by,
            "selected_submission": self.selected_submission,
            "can_auto_apply": self.can_auto_apply,
            "reasons": list(self.reasons),
        }


@dataclass(frozen=True)
class ApplyResult:
    submission_id: str
    source_commit_oid: str
    target_commit_oid: str
    raw: Mapping[str, Any] = field(repr=False)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> ApplyResult:
        return cls(
            submission_id=_s(raw, "submission_id"),
            source_commit_oid=_s(raw, "source_commit_oid"),
            target_commit_oid=_s(raw, "target_commit_oid"),
            raw=dict(raw),
        )


@dataclass(frozen=True)
class RunAgent:
    """One roster seat (``TeamAgent``)."""

    agent_id: str
    env_id: str
    runtime: str | None
    model: str | None
    isolation_claim: str
    state: str
    latest_submission_id: str | None
    raw: Mapping[str, Any] = field(repr=False)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> RunAgent:
        return cls(
            agent_id=_s(raw, "agent_id"),
            env_id=_s(raw, "env_id"),
            runtime=raw.get("runtime"),
            model=raw.get("model"),
            isolation_claim=_s(raw, "isolation_claim"),
            state=_s(raw, "state"),
            latest_submission_id=raw.get("latest_submission_id"),
            raw=dict(raw),
        )


@dataclass(frozen=True)
class Run:
    """The folded run state (``TeamRun``) — what a policy decides over."""

    id: str
    name: str
    base_oid: str
    created_by: str
    created_at: str
    phase: str
    current_round: int
    max_rounds: int
    agents: tuple[RunAgent, ...]
    submissions: tuple[Artifact, ...]
    verifications: tuple[Verification, ...]
    verdict: Verdict | None
    raw: Mapping[str, Any] = field(repr=False)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> Run:
        verdict = raw.get("verdict")
        return cls(
            id=_s(raw, "id"),
            name=_s(raw, "name"),
            base_oid=_s(raw, "base_oid"),
            created_by=_s(raw, "created_by"),
            created_at=_s(raw, "created_at"),
            phase=_s(raw, "phase"),
            current_round=_i(raw, "current_round"),
            max_rounds=_i(raw, "max_rounds"),
            agents=tuple(RunAgent.from_raw(a) for a in raw.get("agents") or ()),
            submissions=tuple(Artifact.from_raw(s) for s in raw.get("submissions") or ()),
            verifications=tuple(
                Verification.from_raw(v) for v in raw.get("verifications") or ()
            ),
            verdict=Verdict.from_raw(verdict) if verdict else None,
            raw=dict(raw),
        )


@dataclass(frozen=True)
class CompareRow:
    """One row of the arena view (``TeamCompareRow``)."""

    agent_id: str
    env_id: str
    submitted: bool
    submission_id: str | None
    status: str
    files_changed: int
    insertions: int
    deletions: int
    last_exit: int | None
    last_tool: str | None
    last_result: str | None
    raw: Mapping[str, Any] = field(repr=False)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> CompareRow:
        return cls(
            agent_id=_s(raw, "agent_id"),
            env_id=_s(raw, "env_id"),
            submitted=bool(raw.get("submitted", False)),
            submission_id=raw.get("submission_id"),
            status=_s(raw, "status"),
            files_changed=_i(raw, "files_changed"),
            insertions=_i(raw, "insertions"),
            deletions=_i(raw, "deletions"),
            last_exit=raw.get("last_exit"),
            last_tool=raw.get("last_tool"),
            last_result=raw.get("last_result"),
            raw=dict(raw),
        )


@dataclass(frozen=True)
class GateAnswer:
    """A human's reply to a durable gate."""

    sender: str
    body: str
    raw: Mapping[str, Any] = field(repr=False)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> GateAnswer:
        return cls(sender=_s(raw, "from"), body=_s(raw, "body"), raw=dict(raw))

    @property
    def approved(self) -> bool:
        return approves_text(self.body)


@dataclass(frozen=True)
class TurnContext:
    """Everything a client-side launcher gets for one agent turn."""

    run_id: str
    agent_id: str
    env_id: str
    kind: str  # "work" | "review" | "revise" | "ask" | "reflect"
    target: str | None
    instruction: str
    repo_workdir: str
    h5i_root: str
    work_dir: str | None
    runtime: str | None
    model: str | None
    raw: Mapping[str, Any] = field(repr=False)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> TurnContext:
        return cls(
            run_id=_s(raw, "run_id"),
            agent_id=_s(raw, "agent_id"),
            env_id=_s(raw, "env_id"),
            kind=_s(raw, "kind"),
            target=raw.get("target"),
            instruction=_s(raw, "instruction"),
            repo_workdir=_s(raw, "repo_workdir"),
            h5i_root=_s(raw, "h5i_root"),
            work_dir=raw.get("work_dir"),
            runtime=raw.get("runtime"),
            model=raw.get("model"),
            raw=dict(raw),
        )
