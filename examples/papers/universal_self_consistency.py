"""Universal Self-Consistency for Large Language Model Generation (Chen et
al. 2023, arXiv:2311.17311) — self-consistency for free-form answers.

Classic self-consistency needs answers that can be exact-matched for a
vote; USC removes that constraint: sample N responses, then ask one
selector to read them all and pick *the most consistent one* — the answer
that best agrees with the sample population. This is the free-form sibling
of ``self_consistency.py``: the same parallel independent seats, with the
Counter swapped for a selector ``ask`` whose choice is validated against
the sample indices.

    python examples/papers/universal_self_consistency.py ["<question>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_QUESTION = (
    "What were the main causes of the fall of the Western Roman Empire? "
    "Answer in one short paragraph."
)
N_SAMPLES = 4


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


def parse_choice(n: int):
    def parse(value: Any) -> dict[str, Any]:
        if not isinstance(value, Mapping) or "choice" not in value:
            raise ValueError('reply must be {"choice": <1-based index>, "reason": "..."}')
        choice = int(value["choice"])
        if not 1 <= choice <= n:
            raise ValueError(f"choice must be between 1 and {n}")
        return {"choice": choice, "reason": str(value.get("reason", "")).strip()}

    return parse


async def main(question: str) -> None:
    async with Conductor(".", "usc-demo", launcher="resident", isolation="supervised") as c:
        samplers = [
            await c.hire(f"sampler{i}", runtime="claude", model="claude-haiku-4-5")
            for i in range(N_SAMPLES)
        ]
        selector = await c.hire("selector", runtime="claude", model="claude-haiku-4-5")

        # N independent free-form responses, in parallel.
        responses = list(
            await asyncio.gather(
                *(
                    s.ask(f"{question}\n\nReply as a single JSON string.", parse=parse_text)
                    for s in samplers
                )
            )
        )

        # The universal vote: pick the response most consistent with the
        # whole sample population.
        numbered = "\n\n".join(
            f"--- response {i + 1} ---\n{r}" for i, r in enumerate(responses)
        )
        picked = await selector.ask(
            f"Question: {question}\n\nIndependent responses:\n\n{numbered}\n\n"
            "Select the single response that is MOST CONSISTENT with the "
            "majority of the responses — the one whose claims the others "
            'agree with most. Reply as JSON: {"choice": <1-based index>, '
            '"reason": "<why>"}',
            parse=parse_choice(len(responses)),
        )
        await c.note(
            f"universal self-consistency: response {picked['choice']}/{N_SAMPLES} "
            f"— {picked['reason']}"
        )
        print(f"selected response {picked['choice']} ({picked['reason']}):\n")
        print(responses[picked["choice"] - 1])


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_QUESTION))
