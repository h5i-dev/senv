"""Meta-Prompting: Enhancing Language Models with Task-Agnostic Scaffolding
(Suzgun & Kalai 2024, arXiv:2401.12954) — a conductor invents and consults
fresh experts.

One model wears two hats: as the *meta* model it breaks the problem down,
decides which expert would help next, writes that expert's persona and
instructions from scratch, and integrates the replies; each *expert* is a
fresh instance that sees only the excerpt the conductor chose to pass — no
history, no task statement. Here the conductor is one seat and experts
draw from a small pool of clean seats (rotated per consultation), so the
paper's fresh-eyes property holds while sessions stay resident.

    python examples/papers/meta_prompting.py ["<task>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_TASK = (
    "Write a correct regular expression that matches valid IPv4 addresses "
    "(reject 999.1.1.1 and leading zeros like 01.2.3.4), and explain it."
)
MAX_CONSULTATIONS = 4


def parse_move(value: Any) -> dict[str, str]:
    if not isinstance(value, Mapping) or "action" not in value:
        raise ValueError(
            'reply must be {"action": "consult"|"final", "persona": "...", '
            '"instructions": "...", "final_answer": "..."}'
        )
    action = str(value["action"]).strip().lower()
    if action not in {"consult", "final"}:
        raise ValueError('action must be "consult" or "final"')
    return {
        "action": action,
        "persona": str(value.get("persona", "")).strip(),
        "instructions": str(value.get("instructions", "")).strip(),
        "final_answer": str(value.get("final_answer", "")).strip(),
    }


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


async def main(task: str) -> None:
    async with Conductor(".", "metaprompt-demo", launcher="resident", isolation="supervised") as c:
        conductor_seat = await c.hire("conductor", runtime="claude", model="claude-haiku-4-5")
        expert_pool = [
            await c.hire(f"expert{i}", runtime="claude", model="claude-haiku-4-5")
            for i in range(2)
        ]

        history = ""
        final = ""
        for consult_no in range(1, MAX_CONSULTATIONS + 1):
            move = await conductor_seat.ask(
                f"You are the meta model orchestrating experts.\nTask: {task}\n\n"
                f"Consultations so far:\n{history or '(none)'}\n\n"
                "Either consult one more expert — invent the most useful "
                "persona and write its instructions, INCLUDING every detail "
                "it needs (experts see nothing but your instructions) — or, "
                "if you can already answer, finish. Reply as JSON: "
                '{"action": "consult"|"final", "persona": "...", '
                '"instructions": "...", "final_answer": "..."}',
                parse=parse_move,
            )
            if move["action"] == "final":
                final = move["final_answer"]
                print(f"consultation {consult_no}: conductor finishes")
                break

            # A fresh expert: sees only the conductor's instructions.
            expert = expert_pool[(consult_no - 1) % len(expert_pool)]
            reply = await expert.ask(
                f"You are: {move['persona']}\n\n{move['instructions']}\n\n"
                "Reply as a single JSON string.",
                parse=parse_text,
            )
            history += (
                f"--- consultation {consult_no}: {move['persona']} ---\n"
                f"instructions: {move['instructions']}\nreply: {reply}\n\n"
            )
            print(f"consultation {consult_no}: {move['persona']}")

        if not final:  # budget spent — force the conductor to conclude
            final = await conductor_seat.ask(
                f"Task: {task}\n\nConsultations:\n{history}\nGive your final "
                "answer now. Reply as a single JSON string.",
                parse=parse_text,
            )
        await c.note("meta-prompting: finished")
        print(f"\nfinal answer:\n{final}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
