"""Multi-Agent Verification: Scaling Test-Time Compute with Multiple
Verifiers (Lifshitz et al. 2025, arXiv:2502.20379) — BoN-MAV: best-of-n
candidates × m aspect verifiers.

Instead of one reward model, MAV scales the *number of verifiers*: many
simple aspect verifiers each give a binary pass/fail on one dimension, and
the candidate with the most approvals wins. The h5i mapping: n independent
work attempts are the best-of-n pool; each candidate is neutrally executed
first (``conductor.verify``) so the aspect verifiers ground their binary
votes in recorded evidence, not vibes; approval counting and the tie-break
(smaller diff) are an ordinary verdict policy. Add aspects to scale m.

    python examples/papers/mav_bon.py ["<task>"]   # default: implement quicksort with pytest
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor, Verdict, patterns

DEMO_TASK = "implement quicksort with pytest"
VERIFY = ["pytest", "-q"]
ASPECTS = (
    ("verifier-functional", "functional correctness: does the evidence show it works?"),
    ("verifier-edgecases", "edge-case coverage: empty, single-element, duplicate, adversarial inputs"),
    ("verifier-simplicity", "simplicity: smallest change that honestly does the job"),
)


def parse_approvals(candidate_ids: list[str]):
    def parse(value: Any) -> dict[str, bool]:
        if not isinstance(value, Mapping) or not isinstance(value.get("approvals"), list):
            raise TypeError(
                'reply must be {"approvals": [{"artifact_id": "...", "pass": true|false, "reason": "..."}]}'
            )
        votes: dict[str, bool] = {}
        for entry in value["approvals"]:
            if not isinstance(entry, Mapping) or entry.get("artifact_id") not in candidate_ids:
                raise ValueError(f"approvals must cover only the candidates {candidate_ids}")
            votes[str(entry["artifact_id"])] = bool(entry.get("pass", False))
        missing = [cid for cid in candidate_ids if cid not in votes]
        if missing:
            raise ValueError(f"missing approvals for {missing}")
        return votes

    return parse


async def main(task: str) -> None:
    async with Conductor(".", "mav-demo", launcher="resident", isolation="supervised") as c:
        pool = [
            await c.hire("cand0", runtime="claude", model="claude-haiku-4-5"),
            await c.hire("cand1", runtime="claude", model="claude-haiku-4-5"),
            await c.hire("cand2", runtime="codex", model="gpt-5.4-mini", effort="medium"),
        ]
        verifiers = [
            await c.hire(name, runtime="claude", model="claude-haiku-4-5")
            for name, _ in ASPECTS
        ]

        # Best-of-n: independent candidates, sealed, then neutrally executed
        # so every aspect vote is grounded in recorded evidence.
        candidates = list(
            await asyncio.gather(*(a.work(task, expect_independent=True) for a in pool))
        )
        await c.freeze()
        for artifact in candidates:
            await c.verify(artifact, VERIFY)

        status = await c.status()
        candidate_ids = [s.id for s in status.submissions]
        evidence = patterns.render_evidence(status)

        # m aspect verifiers, each a binary vote per candidate, in parallel.
        cards = await asyncio.gather(
            *(
                verifier.ask(
                    f"You verify ONE aspect only: {aspect}\n"
                    f"Task: {task}\n\nRecorded evidence:\n{evidence}\n"
                    "For EACH candidate give a binary pass/fail on your aspect "
                    "alone. Reply as JSON: "
                    '{"approvals": [{"artifact_id": "<id>", "pass": true|false, '
                    '"reason": "<one line>"}]}',
                    parse=parse_approvals(candidate_ids),
                )
                for verifier, (_, aspect) in zip(verifiers, ASPECTS)
            )
        )
        approvals = {
            cid: sum(1 for card in cards if card.get(cid)) for cid in candidate_ids
        }
        for cid in candidate_ids:
            print(f"{cid}: {approvals[cid]}/{len(ASPECTS)} aspect approvals")

        # The BoN-MAV selection rule as a verdict policy: most approvals
        # wins; ties go to the smaller diff.
        def most_approvals(run) -> Verdict:
            ranked = sorted(
                run.submissions,
                key=lambda s: (
                    -approvals.get(s.id, 0),
                    s.files_changed,
                    s.insertions,
                    s.id,
                ),
            )
            winner = ranked[0]
            return Verdict(
                method=f"mav:bon({len(ASPECTS)} aspect verifiers)",
                decided_by="mav-demo score",
                selected_submission=winner.id,
                can_auto_apply=False,
                reasons=(
                    f"{winner.id} won {approvals.get(winner.id, 0)}/{len(ASPECTS)} approvals",
                ),
            )

        verdict = await c.judge(most_approvals)
        print("\nverdict:", verdict.selected_submission, "—", *verdict.reasons)


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
