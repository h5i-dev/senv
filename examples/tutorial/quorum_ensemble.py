"""A pattern written from scratch: the majority-quorum ensemble.

``patterns.ensemble`` revises whenever *any* reviewer withholds approval
(unanimity). Say you want a different rule — an author stands pat unless a
strict majority of its reviewers reject, and only the rejecting feedback is
forwarded. That quorum is deliberately not an ``ensemble`` kwarg: a pattern
is ordinary SDK code, so you write the loop yourself. This file is the
anatomy of doing that — every prebuilt pattern has the same skeleton:

1. attempt — independent first attempts (``expect_independent=True``)
2. freeze  — seal the round before any cross-agent influence
3. interact — your control flow: reviews, the custom quorum, merged
   feedback via ``patterns.merge_reviews``, revise turns
4. verify + judge — the shared tail, one ``patterns.verify_and_judge`` call

Everything used here is public API — no privileged hooks. Hire every seat
before the freeze (enrollment is open-round-only), and remember each turn is
journaled: a killed run resumes without re-paying completed turns.

    python examples/tutorial/quorum_ensemble.py ["<task>"]   # default: implement quicksort with pytest
"""

import asyncio
import sys
from collections.abc import Sequence
from dataclasses import dataclass

from h5i.orchestra import Agent, Artifact, Conductor, Review, Verdict, patterns

DEMO_TASK = "implement quicksort with pytest"


@dataclass
class QuorumOutcome:
    #: each agent's latest artifact after the cycles, ordered by agent id
    artifacts: list[Artifact]
    #: every review posted across all cycles
    reviews: list[Review]
    verdict: Verdict | None
    rounds_run: int


async def quorum_ensemble(
    c: Conductor,
    task: str,
    agents: Sequence[Agent],
    *,
    rounds: int = 2,
    verify: Sequence[str] | None = None,
) -> QuorumOutcome:
    """Like ``patterns.ensemble``, but an author revises only when a strict
    majority of its reviewers reject — and sees only the rejecting feedback."""
    if len(agents) < 3:
        raise ValueError("a majority quorum needs at least three agents")

    # 1. Independent first attempts, in parallel.
    attempts = await asyncio.gather(
        *(a.work(task, expect_independent=True) for a in agents)
    )
    latest = {agent.id: artifact for agent, artifact in zip(agents, attempts)}

    # 2. Seal the round: no cross-agent influence before every first attempt
    #    is frozen.
    await c.freeze()

    # 3. Review cycles under the majority quorum — plain Python over
    #    journaled turns, the part no kwarg could express.
    all_reviews: list[Review] = []
    rounds_run = 0
    for _ in range(rounds):
        rounds_run += 1
        pairs = [
            (reviewer, target)
            for reviewer in agents
            for target in agents
            if reviewer.id != target.id
        ]
        cycle = await asyncio.gather(
            *(reviewer.review(latest[target.id]) for reviewer, target in pairs)
        )
        all_reviews.extend(cycle)

        revising: list[tuple[Agent, Review]] = []
        for agent in agents:
            received = [r for r in cycle if r.target == agent.id]
            rejections = [r for r in received if not r.approved]
            if len(rejections) * 2 <= len(received):  # majority approves → stand pat
                continue
            revising.append(
                (agent, patterns.merge_reviews(rejections, latest[agent.id]))
            )
        if not revising:
            break
        revised = await asyncio.gather(
            *(agent.revise(latest[agent.id], merged) for agent, merged in revising)
        )
        for (agent, _), artifact in zip(revising, revised):
            latest[agent.id] = artifact

    # 4. The shared tail: neutral verification, then the CLI finalize rule
    #    (pass judge=… for a custom policy).
    verdict = await patterns.verify_and_judge(
        c, list(latest.values()), verify=verify
    )
    return QuorumOutcome(
        artifacts=[latest[k] for k in sorted(latest)],
        reviews=all_reviews,
        verdict=verdict,
        rounds_run=rounds_run,
    )


async def main(task: str) -> None:
    async with Conductor(".", "quorum-demo", isolation="supervised") as c:
        crew = [
            await c.hire(f"worker{i}", runtime="claude", model="claude-haiku-4-5")
            for i in range(3)
        ]
        outcome = await quorum_ensemble(
            c, task, crew, rounds=2, verify=["pytest", "-q"]
        )
        print(f"{outcome.rounds_run} cycle(s), {len(outcome.reviews)} reviews")
        if outcome.verdict and outcome.verdict.selected_submission:
            print("winner:", outcome.verdict.selected_submission)
        else:
            print("no winner:", *(outcome.verdict.reasons if outcome.verdict else ()))


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
