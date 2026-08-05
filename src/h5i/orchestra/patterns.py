"""Prebuilt orchestrations, implemented in the public SDK — readable,
forkable, no privileged API. Each is a faithful port of the Rust
``h5i_orchestra::patterns`` module, built only from the public primitives
in one shared shape: attempt → ``freeze`` → interact (review / revise /
ask) → ``verify`` → ``judge``.

Three ways to make one your own, cheapest first:

- **Parameterize** — the conventions are injectable where that is cheap:
  ``ensemble(..., approve=…)`` swaps the approval predicate,
  ``judge_panel(..., aggregate=…)`` swaps the ballot aggregation, and every
  ``judge=`` kwarg accepts any :data:`~h5i.orchestra.policy.Policy` callable.
- **Compose** — the pieces the patterns are assembled from are public:
  `merge_reviews`, `review_cycle`, `verify_and_judge`,
  `ask_with_valid_citations`, `render_evidence`, `mean_score_verdict`,
  `smaller_diff`. A custom pattern is an ordinary async function over these
  (``examples/tutorial/quorum_ensemble.py`` builds one from scratch).
- **Fork** — if the control flow itself doesn't fit, copy the pattern's ~40
  lines into your score and edit. There is deliberately no plugin API to
  learn; patterns are user-space code.

Roster note: every agent a pattern uses must be hired before the round is
sealed — hire integrators/moderators up front, alongside the workers.
"""

from __future__ import annotations

import asyncio
import sys
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any

from ._conductor import Agent, Conductor
from ._errors import AskParseError, OrchestraError
from ._types import (
    Artifact,
    CompareRow,
    Review,
    Run,
    Verdict,
    Verification,
    approves_text,
)
from .policy import Policy, tests_then_smallest_diff

__all__ = [
    "ArenaOutcome",
    "Ballot",
    "DebateConclusion",
    "DebateOutcome",
    "EnsembleOutcome",
    "IntegrateOutcome",
    "JudgePanelOutcome",
    "MapReduceOutcome",
    "ReviewCycleOutcome",
    "approves",
    "arena",
    "ask_with_valid_citations",
    "debate",
    "ensemble",
    "integrate",
    "judge_panel",
    "map_reduce",
    "mean_score_verdict",
    "merge_reviews",
    "pipeline",
    "render_evidence",
    "review_cycle",
    "smaller_diff",
    "verify_and_judge",
]


def approves(review: Review) -> bool:
    """Approval convention applied to a review (``APPROVE``/``LGTM``/…,
    optionally behind a ``Verdict:`` label, leading the first line)."""
    return approves_text(review.body)


# ── composition helpers ───────────────────────────────────────────────────────
#
# The pieces the prebuilt patterns are assembled from — public, so a custom
# pattern composes them instead of forking a whole pattern.


def merge_reviews(received: Sequence[Review], target: Artifact) -> Review:
    """Fold several reviews of one artifact into a single review, so feedback
    from many reviewers costs the author one revise turn. Each reviewer's
    feedback is tagged ``[reviewer]`` in the merged body; the merged review
    references the reviewed artifact."""
    if not received:
        raise ValueError("merge_reviews needs at least one review")
    return Review(
        reviewer="+".join(r.reviewer for r in received),
        target=target.owner_agent,
        round=received[0].round,
        body="\n\n".join(f"[{r.reviewer}]\n{r.body}" for r in received),
        referenced_artifacts=(target.id,),
    )


@dataclass
class ReviewCycleOutcome:
    #: every review posted this cycle, in (reviewer, target) pair order
    reviews: list[Review]
    #: latest artifact per agent id after the cycle (revisions applied)
    artifacts: dict[str, Artifact]
    #: ids of the agents that revised this cycle, in roster order
    revised: tuple[str, ...]


async def review_cycle(
    agents: Sequence[Agent],
    latest: Mapping[str, Artifact],
    *,
    approve: Callable[[Review], bool] = approves,
) -> ReviewCycleOutcome:
    """One mutual review → revise cycle over ``latest`` (artifact per agent
    id): every ordered (reviewer, target) pair reviews in parallel, then
    every author whose received reviews are not unanimously approved — per
    ``approve`` — revises once against the merged feedback. Returns updated
    artifacts without mutating ``latest``. For a quorum other than unanimity,
    write the loop yourself on `merge_reviews` — it is a few lines (see
    ``examples/tutorial/quorum_ensemble.py``)."""
    pairs = [
        (reviewer, target)
        for reviewer in agents
        for target in agents
        if reviewer.id != target.id
    ]
    cycle = await asyncio.gather(
        *(reviewer.review(latest[target.id]) for reviewer, target in pairs)
    )
    revising: list[tuple[Agent, Review]] = []
    for agent in agents:
        received = [r for r in cycle if r.target == agent.id]
        if all(approve(r) for r in received):
            continue
        revising.append((agent, merge_reviews(received, latest[agent.id])))
    revised = await asyncio.gather(
        *(agent.revise(latest[agent.id], merged) for agent, merged in revising)
    )
    artifacts = dict(latest)
    for (agent, _), artifact in zip(revising, revised):
        artifacts[agent.id] = artifact
    return ReviewCycleOutcome(
        reviews=list(cycle),
        artifacts=artifacts,
        revised=tuple(agent.id for agent, _ in revising),
    )


async def verify_and_judge(
    c: Conductor,
    artifacts: Sequence[Artifact],
    *,
    verify: Sequence[str] | None = None,
    isolation: str | None = None,
    sealed_from: Artifact | str | None = None,
    judge: Policy | None = None,
) -> Verdict | None:
    """The shared pattern tail: neutrally verify each artifact — one at a
    time, verify worktrees share on-disk state and parallel creation is racy
    — then record a verdict. An explicit ``judge`` always wins; otherwise,
    when a verifier ran, the CLI's finalize rule (`tests_then_smallest_diff`)
    applies; with neither, nothing is recorded and ``None`` comes back.
    ``sealed_from`` seals part of every verified tree to one submission's
    content (see ``Conductor.verify``) — typically an independent test set;
    its owner must not be one of the candidates."""
    if verify is not None:
        for artifact in artifacts:
            await c.verify(
                artifact, verify, isolation=isolation, sealed_from=sealed_from
            )
    if judge is not None:
        return await c.judge(judge)
    if verify is not None:
        return await c.judge(tests_then_smallest_diff)
    return None


# ── ensemble ──────────────────────────────────────────────────────────────────


@dataclass
class EnsembleOutcome:
    #: each agent's latest artifact after the review cycles, ordered by agent id
    artifacts: list[Artifact]
    #: every review posted across all cycles
    reviews: list[Review]
    #: the recorded verdict, when a verifier command or policy was configured
    verdict: Verdict | None
    #: review/revise cycles actually run (early exit on full approval)
    rounds_run: int


async def ensemble(
    c: Conductor,
    task: str,
    agents: Sequence[Agent],
    *,
    rounds: int = 1,
    verify: Sequence[str] | None = None,
    isolation: str | None = None,
    sealed_from: Artifact | str | None = None,
    judge: Policy | None = None,
    approve: Callable[[Review], bool] = approves,
) -> EnsembleOutcome:
    """The classic ensemble: every agent attempts ``task`` independently, the
    round is sealed, agents mutually review and revise for up to ``rounds``
    cycles, then (optionally) a neutral verifier runs and a policy decides.
    Apply is never automatic — inspect the outcome and apply yourself.
    ``approve`` swaps the per-review approval convention (default:
    `approves`, the ``APPROVE``/``LGTM`` first-line rule)."""
    if len(agents) < 2:
        raise ValueError("ensemble needs at least two agents")

    # 1. Independent first attempts, in parallel.
    attempts = await asyncio.gather(
        *(a.work(task, expect_independent=True) for a in agents)
    )
    latest: dict[str, Artifact] = {
        agent.id: artifact for agent, artifact in zip(agents, attempts)
    }

    # 2. Seal the round: no cross-agent influence before every first attempt
    #    is frozen (the independence invariant).
    await c.freeze()

    # 3. Mutual review → revise cycles, host-language loop.
    all_reviews: list[Review] = []
    rounds_run = 0
    for _ in range(rounds):
        rounds_run += 1
        cycle = await review_cycle(agents, latest, approve=approve)
        all_reviews.extend(cycle.reviews)
        latest = cycle.artifacts
        if not cycle.revised:
            break

    # 4-5. Neutral verification + verdict, the shared tail.
    verdict = await verify_and_judge(
        c,
        list(latest.values()),
        verify=verify,
        isolation=isolation,
        sealed_from=sealed_from,
        judge=judge,
    )

    return EnsembleOutcome(
        artifacts=[latest[k] for k in sorted(latest)],
        reviews=all_reviews,
        verdict=verdict,
        rounds_run=rounds_run,
    )


# ── integrate ─────────────────────────────────────────────────────────────────


@dataclass
class IntegrateOutcome:
    merged: Artifact
    verification: Verification | None


async def integrate(
    c: Conductor,
    task: str,
    parts: Sequence[Artifact],
    integrator: Agent,
    *,
    verify: Sequence[str] | None = None,
    isolation: str | None = None,
    sealed_from: Artifact | str | None = None,
) -> IntegrateOutcome:
    """The multi-implementer merge seat: seal the round, then one integrator
    fuses ``parts`` in its own env — granted their diffs as materials,
    honestly stamped non-independent — and optionally the merged artifact is
    neutrally verified."""
    if not parts:
        raise ValueError("integrate needs at least one part")
    # Materials ride the discuss channel, which is sealed-phase-only.
    await c.freeze()
    merged = await integrator.work(
        f"{task}\n\nMerge the granted teammate artifacts into one coherent "
        "candidate: apply their patches in this worktree, resolve conflicts "
        "(prefer a mechanical `git merge`/`git apply` first; use judgment only "
        "where the changes genuinely collide), and make the result build.",
        materials=parts,
    )
    verification = None
    if verify is not None:
        verification = await c.verify(
            merged, verify, isolation=isolation, sealed_from=sealed_from
        )
    return IntegrateOutcome(merged=merged, verification=verification)


# ── pipeline ──────────────────────────────────────────────────────────────────


async def pipeline(
    c: Conductor, stages: Sequence[tuple[Agent, str]]
) -> list[Artifact]:
    """Role-specialized stages in sequence (architect → implementer →
    reviewer …): stage 1 works independently; the round is sealed; every
    later stage gets the previous stage's artifact as material. Returns one
    artifact per stage, in order."""
    if not stages:
        raise ValueError("pipeline needs at least one stage")
    artifacts: list[Artifact] = []
    for i, (agent, task) in enumerate(stages):
        if i == 0:
            first = await agent.work(task)
            await c.freeze()
            artifacts.append(first)
        else:
            artifacts.append(await agent.work(task, materials=[artifacts[-1]]))
    return artifacts


# ── arena ─────────────────────────────────────────────────────────────────────


@dataclass
class ArenaOutcome:
    artifacts: list[Artifact]
    rows: list[CompareRow]
    verdict: Verdict | None


async def arena(
    c: Conductor,
    task: str,
    agents: Sequence[Agent],
    *,
    verify: Sequence[str] | None = None,
    isolation: str | None = None,
    sealed_from: Artifact | str | None = None,
    judge: Policy | None = None,
) -> ArenaOutcome:
    """Independent attempts, ranked: N agents try the same task with no
    cross-influence, the round seals, every candidate is (optionally)
    neutrally verified with one command, a policy decides, and the roster
    comparison rows come back alongside the verdict."""
    if len(agents) < 2:
        raise ValueError("arena needs at least two agents")
    artifacts = list(
        await asyncio.gather(*(a.work(task, expect_independent=True) for a in agents))
    )
    await c.freeze()
    verdict = await verify_and_judge(
        c,
        artifacts,
        verify=verify,
        isolation=isolation,
        sealed_from=sealed_from,
        judge=judge,
    )
    rows = await c.compare()
    return ArenaOutcome(artifacts=artifacts, rows=rows, verdict=verdict)


# ── map_reduce ────────────────────────────────────────────────────────────────


@dataclass
class MapReduceOutcome:
    parts: list[Artifact]
    merged: Artifact | None


async def map_reduce(
    c: Conductor,
    assignments: Sequence[tuple[Agent, str]],
    *,
    reduce: tuple[Agent, str] | None = None,
) -> MapReduceOutcome:
    """Fan a work list out and merge: each ``(agent, task)`` assignment runs
    as its own work turn — assignments to the *same* agent run sequentially
    (one resident session, and one journal label, per agent) — then the round
    seals and the reducer fuses every part with materials."""
    if not assignments:
        raise ValueError("map_reduce needs at least one assignment")

    # Group by agent: cross-agent parallel, same-agent sequential.
    by_agent: dict[str, tuple[Agent, list[str]]] = {}
    for agent, task in assignments:
        by_agent.setdefault(agent.id, (agent, []))[1].append(task)

    async def run_agent(agent: Agent, tasks: list[str]) -> list[Artifact]:
        return [await agent.work(task) for task in tasks]

    grouped = await asyncio.gather(
        *(run_agent(agent, tasks) for agent, tasks in by_agent.values())
    )
    parts = [artifact for group in grouped for artifact in group]

    merged: Artifact | None = None
    if reduce is not None:
        integrator, reduce_task = reduce
        merged = (await integrate(c, reduce_task, parts, integrator)).merged
    else:
        await c.freeze()
    return MapReduceOutcome(parts=parts, merged=merged)


# ── judge_panel ───────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Ballot:
    """One judge's scored ballot for a candidate, grounded in cited evidence."""

    artifact_id: str
    score: int
    rationale: str
    cited_ids: tuple[str, ...] = ()

    @classmethod
    def from_value(cls, value: Mapping[str, Any]) -> Ballot:
        if not isinstance(value, Mapping):
            raise TypeError(f"ballot must be an object, got {value!r}")
        return cls(
            artifact_id=str(value.get("artifact_id", "")),
            score=int(value.get("score", 0)),
            rationale=str(value.get("rationale", "")),
            cited_ids=tuple(value.get("cited_ids") or ()),
        )


@dataclass
class JudgePanelOutcome:
    #: every validated ballot, by judge id
    ballots: list[tuple[str, list[Ballot]]]
    #: the recorded verdict (default: highest mean score, ties broken by
    #: smallest diff — see ``aggregate``)
    verdict: Verdict


async def judge_panel(
    c: Conductor,
    rubric: str,
    judges: Sequence[Agent],
    *,
    aggregate: Callable[[Sequence[Ballot], int, Run], Verdict] | None = None,
) -> JudgePanelOutcome:
    """A panel of judge agents scores the sealed candidates over the run's
    recorded evidence; citations are validated against real ids (bounded
    re-ask on a hallucinated citation) and the aggregated winner is recorded
    as the verdict. Judges are read-only seats — they never submit, and the
    verdict is advisory (never auto-applicable). ``aggregate`` swaps the
    aggregation rule ``(ballots, n_judges, run) -> Verdict`` (median, quorum,
    veto, …); the default is `mean_score_verdict`."""
    if not judges:
        raise ValueError("judge_panel needs at least one judge")
    status = await c.status()
    candidates = list(status.submissions)
    if not candidates:
        raise OrchestraError("judge_panel: no submissions to judge (freeze/collect first)")

    valid_ids = {s.id for s in status.submissions} | {v.id for v in status.verifications}
    candidate_ids = [s.id for s in candidates]
    evidence = render_evidence(status)

    prompt = (
        f"You are a neutral judge on a review panel. Rubric: {rubric}\n\n"
        "Score EACH candidate 0-10, grounding every rationale in the recorded "
        "evidence below (cite the exact ids you used). Do not run the code; "
        "judge from the evidence.\n\n"
        f"Candidates: {', '.join(candidate_ids)}\n\nEvidence:\n{evidence}\n\n"
        'Reply as JSON: {"ballots": [{"artifact_id": "<id>", "score": <0-10>, '
        '"rationale": "<why, citing ids>", "cited_ids": ["<id>", …]}]}.'
    )

    ballots: list[tuple[str, list[Ballot]]] = []
    for judge in judges:
        card = await ask_with_valid_citations(
            judge, prompt, valid_ids, candidate_ids
        )
        ballots.append((judge.id, card))

    # Aggregation is a policy — the panel's contribution is eliciting
    # evidence-cited ballots; ``aggregate`` decides over them.
    flat = [b for _, judge_ballots in ballots for b in judge_ballots]
    n_judges = len(judges)
    rule = aggregate if aggregate is not None else mean_score_verdict

    def aggregate_policy(run: Run) -> Verdict:
        return rule(flat, n_judges, run)

    verdict = await c.judge(aggregate_policy)
    return JudgePanelOutcome(ballots=ballots, verdict=verdict)


async def ask_with_valid_citations(
    judge: Agent,
    base_prompt: str,
    valid_ids: set[str],
    candidate_ids: Sequence[str],
) -> list[Ballot]:
    """Re-ask (bounded) if the judge cites ids not in the run or scores a
    non-candidate — what makes the panel evidence-grounded rather than
    free-associating."""
    prompt = base_prompt
    for attempt in range(3):
        value = await judge.ask(prompt)
        try:
            raw_ballots = value["ballots"] if isinstance(value, Mapping) else None
            if not isinstance(raw_ballots, list):
                raise TypeError('reply must be {"ballots": [...]}')
            card = [Ballot.from_value(b) for b in raw_ballots]
        except (ValueError, TypeError, KeyError) as e:
            problems = [f"unparseable ballots ({e})"]
        else:
            problems = []
            for ballot in card:
                if ballot.artifact_id not in candidate_ids:
                    problems.append(f"scored non-candidate '{ballot.artifact_id}'")
                for cited in ballot.cited_ids:
                    if cited not in valid_ids:
                        problems.append(f"cited unknown evidence id '{cited}'")
            if not problems:
                return card
        if attempt == 2:
            raise AskParseError(
                f"judge_panel: judge '{judge.id}' kept citing invalid evidence: "
                + "; ".join(problems)
            )
        prompt = (
            f"{base_prompt}\n\nYour previous reply had problems: "
            + "; ".join(problems)
            + ". Score ONLY the listed candidates and cite ONLY ids that appear "
            "in the evidence."
        )
    raise AssertionError("unreachable")


def mean_score_verdict(ballots: Sequence[Ballot], n_judges: int, run: Run) -> Verdict:
    """Mean-score aggregation: highest mean wins, ties broken by smallest
    diff. A panel is advisory over evidence (not a neutral re-execution), so
    the verdict is never auto-applicable. This is `judge_panel`'s default
    ``aggregate`` rule — a template for writing your own."""
    method = f"panel:mean-score({n_judges} judges)"
    epsilon = sys.float_info.epsilon
    best: tuple[str, float] | None = None
    for candidate in run.submissions:
        scores = [min(b.score, 10) for b in ballots if b.artifact_id == candidate.id]
        if not scores:
            continue
        mean = sum(scores) / len(scores)
        if best is None:
            better = True
        else:
            _, current_mean = best
            better = mean > current_mean + epsilon or (
                abs(mean - current_mean) <= epsilon
                and smaller_diff(candidate, best[0], run.submissions)
            )
        if better:
            best = (candidate.id, mean)
    if best is None:
        return Verdict(
            selected_submission=None,
            method=method,
            decided_by="judge-panel",
            can_auto_apply=False,
            reasons=("no candidate received a ballot",),
        )
    winner, mean = best
    return Verdict(
        selected_submission=winner,
        method=method,
        decided_by="judge-panel",
        can_auto_apply=False,
        reasons=(f"{winner} won the panel with mean score {mean:.1f}/10",),
    )


def smaller_diff(
    candidate: Artifact, other_id: str, candidates: Sequence[Artifact]
) -> bool:
    """The smallest-diff tie-break: fewer files changed, then fewer
    insertions, then lexicographic id — usable in any custom aggregation."""
    other = next((a for a in candidates if a.id == other_id), None)
    if other is None:
        return False
    return (candidate.files_changed, candidate.insertions, candidate.id) < (
        other.files_changed,
        other.insertions,
        other.id,
    )


def render_evidence(run: Run) -> str:
    """Render a folded run's submissions and verifications as the compact,
    id-citable evidence block `judge_panel` grounds its judges in."""
    lines = ["Submissions:"]
    for sub in run.submissions:
        lines.append(
            f"- {sub.id} by {sub.owner_agent} (round {sub.round}, "
            f"+{sub.insertions}/-{sub.deletions} over {sub.files_changed} files, "
            f"independent={str(sub.independent).lower()})"
        )
    if run.verifications:
        lines.append("Verifications:")
        for v in run.verifications:
            lines.append(
                f"- {v.id} for {v.submission_id} "
                f"(applies_cleanly={str(v.applies_cleanly).lower()}, "
                f"tests_passed={str(v.tests_passed).lower()}, "
                f"cmd `{' '.join(v.command)}`)"
            )
    return "\n".join(lines) + "\n"


# ── debate ────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class DebateConclusion:
    """The moderator's structured conclusion of a debate."""

    winner: str
    rationale: str

    @classmethod
    def from_value(cls, value: Any) -> DebateConclusion:
        if not isinstance(value, Mapping) or "winner" not in value:
            raise ValueError('reply must be {"winner": "<agent-id>", "rationale": "…"}')
        return cls(winner=str(value["winner"]), rationale=str(value.get("rationale", "")))


@dataclass
class DebateOutcome:
    #: ``(agent_id, argument)`` in speaking order
    transcript: list[tuple[str, str]]
    conclusion: DebateConclusion | None


async def debate(
    c: Conductor,
    question: str,
    sides: Sequence[Agent],
    *,
    moderator: Agent | None = None,
    rounds: int = 1,
) -> DebateOutcome:
    """Argue a question through data turns: each side speaks in alternating
    order for ``rounds`` rounds (seeing the transcript so far), then an
    optional moderator concludes. Pure ``ask`` — no artifacts, no freeze."""
    if len(sides) < 2:
        raise ValueError("debate needs at least two sides")
    rounds = max(1, rounds)
    transcript: list[tuple[str, str]] = []
    for round_no in range(1, rounds + 1):
        for side in sides:
            if not transcript:
                context = "You open the debate."
            else:
                context = "Transcript so far:\n" + "".join(
                    f"- {who}: {what}\n" for who, what in transcript
                )
            argument = await side.ask(
                f"Debate (round {round_no}/{rounds}): {question}\n\n{context}\n\n"
                "Make your strongest argument for your side, as a single JSON "
                "string.",
                parse=lambda v: v if isinstance(v, str) else str(v),
            )
            transcript.append((side.id, argument))
    conclusion: DebateConclusion | None = None
    if moderator is not None:
        rendered = "".join(f"- {who}: {what}\n" for who, what in transcript)
        conclusion = await moderator.ask(
            f"You moderate this debate: {question}\n\nTranscript:\n{rendered}\n"
            'Decide which side prevailed. Reply as JSON: {"winner": '
            '"<agent-id>", "rationale": "<why>"}.',
            parse=DebateConclusion.from_value,
        )
    return DebateOutcome(transcript=transcript, conclusion=conclusion)
