"""Constitutional AI: Harmlessness from AI Feedback (Bai et al. 2022,
arXiv:2212.08073) — the supervised critique-revision loop, with an
explicit constitution.

CAI's first stage replaces human feedback with principle-grounded
self-critique: a draft response is checked against each principle of a
written constitution; every violation produces a critique, and the drafter
revises against the collected critiques — repeating until the response is
clean. The constitution below is a small generic one; edit it to govern
whatever matters in your domain (the loop is principle-agnostic). Critique
turns are per-principle ``ask``s by a critic seat of the same model.

    python examples/papers/constitutional_ai.py ["<request>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_REQUEST = (
    "My coworker keeps taking credit for my work. Draft a message I can "
    "send them tonight to make them stop."
)
CONSTITUTION = (
    "Be helpful: actually address the request instead of deflecting it.",
    "Avoid harm: do not encourage retaliation, escalation, or deception.",
    "Be honest: no fabricated facts, no overstated certainty.",
    "Respect autonomy: offer options and trade-offs, not commands.",
)
MAX_ROUNDS = 2


def parse_critique(value: Any) -> dict[str, Any]:
    if not isinstance(value, Mapping) or "violates" not in value:
        raise ValueError('reply must be {"violates": true|false, "critique": "..."}')
    return {
        "violates": bool(value["violates"]),
        "critique": str(value.get("critique", "")).strip(),
    }


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


async def main(request: str) -> None:
    async with Conductor(".", "cai-demo", launcher="resident", isolation="supervised") as c:
        drafter = await c.hire("drafter", runtime="claude", model="claude-haiku-4-5")
        critic = await c.hire("critic", runtime="claude", model="claude-haiku-4-5")

        response = await drafter.ask(
            f"{request}\n\nReply as a single JSON string.", parse=parse_text
        )

        for round_no in range(1, MAX_ROUNDS + 1):
            # Critique the response against each principle separately.
            critiques: list[str] = []
            for i, principle in enumerate(CONSTITUTION, 1):
                verdict = await critic.ask(
                    f"Constitutional principle: {principle}\n\n"
                    f"Request: {request}\n\nResponse under review:\n{response}\n\n"
                    "Does the response violate THIS principle? Reply as "
                    'JSON: {"violates": true|false, "critique": "<specific '
                    'critique if it does>"}',
                    parse=parse_critique,
                )
                if verdict["violates"]:
                    critiques.append(f"(principle {i}: {principle}) {verdict['critique']}")
            if not critiques:
                print(f"round {round_no}: no principle violated")
                break
            print(f"round {round_no}: {len(critiques)} principle(s) violated")

            # Revise against the collected critiques.
            response = await drafter.ask(
                f"Request: {request}\n\nYour previous response:\n{response}\n\n"
                "Constitutional critiques of it:\n"
                + "\n".join(f"- {ct}" for ct in critiques)
                + "\n\nRewrite the response to satisfy every critique while "
                "staying genuinely helpful. Reply as a single JSON string.",
                parse=parse_text,
            )

        await c.note(f"constitutional-ai: finished after round {round_no}")
        print(f"\nfinal response:\n{response}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_REQUEST))
