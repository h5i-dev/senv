"""ReConcile: Round-Table Conference Improves Reasoning via Consensus among
Diverse LLMs (Chen et al. 2024, arXiv:2309.13007) — confidence-weighted
consensus across heterogeneous models.

Three properties distinguish ReConcile from plain debate: the table is
*model-diverse* (here: a Claude seat, a Codex seat, and a third seat — swap
in a third vendor's runtime if you have one installed), every position
carries a *confidence estimate*, and each discussion round shares everyone's
answer + confidence + explanation so agents can be convinced by a better
explanation rather than by repetition. The final answer is a
confidence-weighted vote, not a head count.

    python examples/papers/reconcile.py ["<question>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_QUESTION = (
    "You have two ropes; each takes exactly 60 minutes to burn but burns "
    "unevenly. How do you measure exactly 45 minutes?"
)
ROUNDS = 2

POSITION_SHAPE = (
    'Reply as JSON: {"answer": "<final answer>", "confidence": <0.0-1.0>, '
    '"explanation": "<the argument that would convince a skeptic>"}'
)


def parse_position(value: Any) -> dict[str, Any]:
    if not isinstance(value, Mapping) or "answer" not in value:
        raise ValueError(POSITION_SHAPE)
    try:
        confidence = float(value.get("confidence", 0.5))
    except (TypeError, ValueError) as e:
        raise ValueError(f"confidence must be a number 0.0-1.0 ({e})")
    return {
        "answer": str(value["answer"]).strip(),
        "confidence": min(1.0, max(0.0, confidence)),
        "explanation": str(value.get("explanation", "")).strip(),
    }


def canon(answer: str) -> str:
    return answer.strip().lower()


def table_view(names: list[str], positions: list[dict[str, Any]]) -> str:
    return "\n\n".join(
        f"[{name}] answer: {p['answer']} (confidence {p['confidence']:.2f})\n"
        f"explanation: {p['explanation']}"
        for name, p in zip(names, positions)
    )


async def main(question: str) -> None:
    base = f"Question: {question}\n\n{POSITION_SHAPE}"
    async with Conductor(".", "reconcile-demo", launcher="resident", isolation="supervised") as c:
        table = [
            await c.hire("haiku-seat", runtime="claude", model="claude-haiku-4-5"),
            await c.hire("mini-seat", runtime="codex", model="gpt-5.4-mini", effort="medium"),
            await c.hire("third-seat", runtime="claude", model="claude-haiku-4-5"),
        ]
        names = [seat.id for seat in table]

        # Initial phase: independent positions with confidence.
        positions = list(
            await asyncio.gather(*(seat.ask(base, parse=parse_position) for seat in table))
        )

        # Discussion rounds: everyone sees the whole table (answers,
        # confidences, explanations) and may be convinced.
        for round_no in range(1, ROUNDS + 1):
            if len({canon(p["answer"]) for p in positions}) == 1:
                print(f"consensus before round {round_no}")
                break
            grouped = table_view(names, positions)
            positions = list(
                await asyncio.gather(
                    *(
                        seat.ask(
                            f"Round-table, round {round_no}. Every "
                            f"participant's current position:\n\n{grouped}\n\n"
                            "Weigh the explanations — change your answer only "
                            f"if another is more convincing.\n\n{base}",
                            parse=parse_position,
                        )
                        for seat in table
                    )
                )
            )
            print(f"round {round_no}: {len({canon(p['answer']) for p in positions})} distinct answer(s)")

        # Confidence-weighted vote.
        weight: dict[str, float] = {}
        for p in positions:
            weight[canon(p["answer"])] = weight.get(canon(p["answer"]), 0.0) + p["confidence"]
        winner = max(weight, key=lambda k: weight[k])
        for name, p in zip(names, positions):
            print(f"[{name}] {p['answer']} (confidence {p['confidence']:.2f})")
        await c.note(f"reconcile: '{winner}' won with weight {weight[winner]:.2f}")
        print(f"\nweighted consensus: {winner} (weight {weight[winner]:.2f})")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_QUESTION))
