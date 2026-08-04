"""PRD: Peer Rank and Discussion Improve Large Language Model based
Evaluations (Li et al. 2023, arXiv:2307.02762) — the contestants are also
the jury, weighted by how much the jury trusts them.

Single-judge evaluation suffers self-enhancement and position bias. PRD's
peer rank: every contestant judges every answer pair (both presentation
orders), and each reviewer's vote is weighted by how much it agrees with
the aggregate — a reviewer the panel disagrees with loses influence. Peer
discussion then lets two reviewers argue the closest call to a mutual
verdict. All of it is ``ask`` turns over a model-diverse pool plus a
little matrix arithmetic in host code.

    python examples/papers/prd_peer_rank.py ["<instruction>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from itertools import combinations
from typing import Any

from h5i.orchestra import Agent, Conductor

DEMO_TASK = (
    "Explain what eventual consistency means in distributed systems, with "
    "one concrete example of where it is acceptable and one where it is not."
)


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


def parse_winner(value: Any) -> str:
    label = value.get("winner") if isinstance(value, Mapping) else value
    if not isinstance(label, str) or label.strip().upper() not in {"A", "B"}:
        raise ValueError('reply must be {"winner": "A"} or {"winner": "B"}')
    return label.strip().upper()


async def main(task: str) -> None:
    async with Conductor(".", "prd-demo", launcher="resident", isolation="supervised") as c:
        contestants = [
            await c.hire("peer0", runtime="claude", model="claude-haiku-4-5"),
            await c.hire("peer1", runtime="codex", model="gpt-5.4-mini", effort="medium"),
            await c.hire("peer2", runtime="claude", model="claude-haiku-4-5"),
        ]

        # Every peer answers the instruction.
        answers = list(
            await asyncio.gather(
                *(
                    p.ask(f"{task}\n\nReply as a single JSON string.", parse=parse_text)
                    for p in contestants
                )
            )
        )
        pairs = list(combinations(range(len(contestants)), 2))

        # Peer rank: every peer judges every pair, both orders (position
        # bias washes out); sequential within a reviewer, parallel across.
        async def review_all(reviewer: Agent) -> dict[tuple[int, int], int]:
            prefs: dict[tuple[int, int], int] = {}
            for i, j in pairs:
                votes = []
                for first, second in ((i, j), (j, i)):
                    label = await reviewer.ask(
                        f"Instruction: {task}\n\nAnswer A:\n{answers[first]}\n\n"
                        f"Answer B:\n{answers[second]}\n\nWhich answer is "
                        'better? Reply as JSON: {"winner": "A"} or {"winner": "B"}',
                        parse=parse_winner,
                    )
                    votes.append(first if label == "A" else second)
                prefs[(i, j)] = votes[0] if votes[0] == votes[1] else -1  # -1 = tie
            return prefs

        all_prefs = await asyncio.gather(*(review_all(r) for r in contestants))

        def scores(weights: list[float]) -> list[float]:
            out = [0.0] * len(contestants)
            for reviewer_idx, prefs in enumerate(all_prefs):
                for (i, j), winner in prefs.items():
                    if winner == -1:
                        out[i] += weights[reviewer_idx] / 2
                        out[j] += weights[reviewer_idx] / 2
                    else:
                        out[winner] += weights[reviewer_idx]
            return out

        # Round 1: uniform weights. Round 2: a reviewer's weight is its
        # agreement with the aggregate preference — the peer-trust update.
        base = scores([1.0] * len(contestants))
        consensus = {(i, j): (i if base[i] >= base[j] else j) for i, j in pairs}
        weights = []
        for prefs in all_prefs:
            agreed = sum(1 for pair, winner in prefs.items() if winner == consensus[pair])
            weights.append(agreed / len(pairs))
        weighted = scores(weights)

        for idx, p in enumerate(contestants):
            print(
                f"{p.id}: raw {base[idx]:.1f}, weighted {weighted[idx]:.2f} "
                f"(reviewer weight {weights[idx]:.2f})"
            )

        # Peer discussion on the closest call between the top two.
        top = sorted(range(len(contestants)), key=lambda k: -weighted[k])[:2]
        r1, r2 = contestants[top[1]], contestants[top[0]]
        opinion = await r1.ask(
            f"Instruction: {task}\n\nAnswer A:\n{answers[top[0]]}\n\n"
            f"Answer B:\n{answers[top[1]]}\n\nGive your preference and the "
            "reasons. Reply as a single JSON string.",
            parse=parse_text,
        )
        verdict = await r2.ask(
            f"Instruction: {task}\n\nAnswer A:\n{answers[top[0]]}\n\n"
            f"Answer B:\n{answers[top[1]]}\n\nA fellow reviewer argued:\n"
            f"{opinion}\n\nDiscuss and settle it: which answer wins? Reply "
            'as JSON: {"winner": "A"} or {"winner": "B"}',
            parse=parse_winner,
        )
        final = top[0] if verdict == "A" else top[1]
        await c.note(f"prd: {contestants[final].id} won after peer rank + discussion")
        print(f"\nfinal winner after discussion: {contestants[final].id}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
