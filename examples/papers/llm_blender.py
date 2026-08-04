"""LLM-Blender: Ensembling Large Language Models with Pairwise Ranking and
Generative Fusion (Jiang et al. 2023, arXiv:2306.02561) — rank pairwise,
then fuse the top-k.

Two stages: a PairRanker compares candidates two at a time (pairwise
comparison is far more reliable than absolute scoring), and a GenFuser
generates a *new* answer from the top-ranked candidates instead of just
picking one. Here the candidate pool is model-diverse seats, the ranker
sees every unordered pair in BOTH orders — a pair only counts as a win if
the same candidate wins both presentations, washing out position bias —
and the fuser gets the top-k by win count.

    python examples/papers/llm_blender.py ["<instruction>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from itertools import combinations
from typing import Any

from h5i.orchestra import Conductor

DEMO_TASK = (
    "Explain to a junior engineer when to prefer optimistic locking over "
    "pessimistic locking, with one concrete example of each."
)
TOP_K = 2


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


def parse_winner(value: Any) -> str:
    label = value.get("winner") if isinstance(value, Mapping) else value
    if not isinstance(label, str) or label.strip().upper() not in {"A", "B"}:
        raise ValueError('reply must be {"winner": "A"} or {"winner": "B"}')
    return label.strip().upper()


async def main(task: str) -> None:
    async with Conductor(".", "llm-blender-demo", launcher="resident", isolation="supervised") as c:
        pool = [
            await c.hire("gen0", runtime="claude", model="claude-haiku-4-5"),
            await c.hire("gen1", runtime="codex", model="gpt-5.4-mini", effort="medium"),
            await c.hire("gen2", runtime="claude", model="claude-haiku-4-5"),
        ]
        ranker = await c.hire("ranker", runtime="claude", model="claude-haiku-4-5")
        fuser = await c.hire("fuser", runtime="claude", model="claude-haiku-4-5")

        # Candidate pool: N independent generations.
        candidates = list(
            await asyncio.gather(
                *(
                    seat.ask(f"{task}\n\nReply as a single JSON string.", parse=parse_text)
                    for seat in pool
                )
            )
        )

        # PairRanker: every unordered pair, presented in both orders; a win
        # counts only if it survives the position swap.
        wins = [0.0] * len(candidates)
        for i, j in combinations(range(len(candidates)), 2):
            verdicts = []
            for first, second in ((i, j), (j, i)):
                label = await ranker.ask(
                    f"Instruction: {task}\n\n"
                    f"Candidate A:\n{candidates[first]}\n\n"
                    f"Candidate B:\n{candidates[second]}\n\n"
                    "Which candidate answers the instruction better? Reply as "
                    'JSON: {"winner": "A"} or {"winner": "B"}',
                    parse=parse_winner,
                )
                verdicts.append(first if label == "A" else second)
            if verdicts[0] == verdicts[1]:
                wins[verdicts[0]] += 1.0
            else:  # position-dependent — a tie, half a win each
                wins[i] += 0.5
                wins[j] += 0.5

        ranking = sorted(range(len(candidates)), key=lambda k: -wins[k])
        for rank, k in enumerate(ranking, 1):
            print(f"#{rank} {pool[k].id} ({wins[k]:.1f} pairwise wins)")

        # GenFuser: generate a better answer FROM the top-k, don't just pick.
        top = "\n\n".join(
            f"Candidate {rank}:\n{candidates[k]}"
            for rank, k in enumerate(ranking[:TOP_K], 1)
        )
        fused = await fuser.ask(
            f"Instruction: {task}\n\nThe top-ranked candidate answers:\n\n{top}\n\n"
            "Fuse them: keep each one's strengths, drop the weaknesses, and "
            "produce a single better answer. Reply as a single JSON string.",
            parse=parse_text,
        )
        await c.note(f"llm-blender: fused top-{TOP_K} of {len(candidates)} candidates")
        print(f"\nfused answer:\n{fused}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
