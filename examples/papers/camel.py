"""CAMEL: Communicative Agents for "Mind" Exploration of Large Language
Model Society (Li et al. 2023, arXiv:2303.17760) — inception-prompted role
playing.

CAMEL's recipe for autonomous cooperation without drift: a task specifier
turns a vague idea into a concrete task, then two inception-prompted seats
play fixed roles — the user gives ONE instruction at a time and never
solves; the assistant solves exactly what was instructed and never leads —
until the user declares the task done. The inception prompts below are the
paper's role constraints, generalized; the alternating turns are plain
``ask`` calls, so the whole role-play is journaled and resumable.

    python examples/papers/camel.py ["<idea>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_IDEA = "design a rate limiter for a public HTTP API"
MAX_TURNS = 4

USER_INCEPTION = (
    "Never forget you are the USER and I am the ASSISTANT. You always "
    "instruct; you never solve the task yourself and never switch roles. "
    "Give me exactly ONE concrete instruction at a time, building on my "
    "previous solutions. When the task is completely solved, set done=true."
)
ASSISTANT_INCEPTION = (
    "Never forget you are the ASSISTANT and I am the USER. You solve "
    "exactly the instruction given — completely, concretely, no deferrals "
    "— and you never give instructions back or switch roles."
)


def parse_instruction(value: Any) -> dict[str, Any]:
    if not isinstance(value, Mapping) or "instruction" not in value:
        raise ValueError('reply must be {"instruction": "...", "done": true|false}')
    return {
        "instruction": str(value["instruction"]).strip(),
        "done": bool(value.get("done", False)),
    }


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


async def main(idea: str) -> None:
    async with Conductor(".", "camel-demo", launcher="resident", isolation="supervised") as c:
        specifier = await c.hire("specifier", runtime="claude", model="claude-haiku-4-5")
        user_role = await c.hire("user-role", runtime="claude", model="claude-haiku-4-5")
        assistant_role = await c.hire(
            "assistant-role", runtime="codex", model="gpt-5.4-mini", effort="medium"
        )

        # Task specification: vague idea → concrete, bounded task.
        task = await specifier.ask(
            f"Make this idea specific enough for two agents to execute in a "
            f"few steps — one sentence, concrete deliverable: {idea}\n\n"
            "Reply as a single JSON string.",
            parse=parse_text,
        )
        print(f"specified task: {task}")

        # Inception-prompted role-play: instruct → solve, one step at a time.
        transcript = ""
        for turn in range(1, MAX_TURNS + 1):
            move = await user_role.ask(
                f"{USER_INCEPTION}\n\nTask: {task}\n\nConversation so far:\n"
                f"{transcript or '(start)'}\n\nReply as JSON: "
                '{"instruction": "<your single next instruction>", '
                '"done": true|false}',
                parse=parse_instruction,
            )
            if move["done"]:
                print(f"turn {turn}: user declares the task done")
                break
            print(f"turn {turn}: {move['instruction'][:100]}")
            solution = await assistant_role.ask(
                f"{ASSISTANT_INCEPTION}\n\nTask: {task}\n\nConversation so "
                f"far:\n{transcript or '(start)'}\n\nInstruction: "
                f"{move['instruction']}\n\nReply with your solution as a "
                "single JSON string.",
                parse=parse_text,
            )
            transcript += f"USER: {move['instruction']}\nASSISTANT: {solution}\n\n"

        await c.note(f"camel role-play finished for: {task}")
        print(f"\nrole-play transcript:\n{transcript}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_IDEA))
