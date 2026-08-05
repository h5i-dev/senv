"""Improving Factuality and Reasoning in Language Models through Multiagent
Debate (Du et al. 2023, arXiv:2305.14325) — the "society of minds" loop.

Each agent answers independently; then, for a fixed number of rounds, every
agent reads the *other* agents' latest answers and reasoning and updates its
own. Answers provably converge or expose genuine disagreement; the final
call is a majority vote. Pure ``ask`` data turns — each round is one
parallel gather across distinct seats, and the debate ends early once
everyone agrees.

    python examples/papers/multiagent_debate.py ["<question>"]
"""

import asyncio
import sys
from collections import Counter
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_QUESTION = (
    "Three people check into a hotel room that costs $30 and pay $10 each. "
    "The clerk realizes the room costs $25 and hands the bellboy $5 to "
    "return; the bellboy keeps $2 and gives $1 back to each guest. Each "
    "guest paid $9 (totalling $27) and the bellboy has $2 — where is the "
    "missing dollar?"
)
N_DEBATERS = 3
ROUNDS = 2


def parse_position(value: Any) -> dict[str, str]:
    if not isinstance(value, Mapping) or "answer" not in value:
        raise ValueError('reply must be {"reasoning": "...", "answer": "..."}')
    return {
        "answer": str(value["answer"]).strip(),
        "reasoning": str(value.get("reasoning", "")).strip(),
    }


def canon(answer: str) -> str:
    return answer.strip().lower()


async def main(question: str) -> None:
    base = (
        f"{question}\n\nReply as JSON: "
        '{"reasoning": "<your reasoning>", "answer": "<your final answer>"}'
    )
    async with Conductor(".", "ma-debate-demo", launcher="resident", isolation="supervised") as c:
        debaters = [
            await c.hire(f"debater{i}", runtime="claude", model="claude-haiku-4-5")
            for i in range(N_DEBATERS)
        ]

        # Round 0: independent answers, in parallel.
        positions = list(
            await asyncio.gather(*(d.ask(base, parse=parse_position) for d in debaters))
        )

        # Debate rounds: each agent sees every OTHER agent's latest position
        # and updates its own. Stop early on unanimity.
        for round_no in range(1, ROUNDS + 1):
            if len({canon(p["answer"]) for p in positions}) == 1:
                print(f"converged before round {round_no}")
                break
            prompts = []
            for i, debater in enumerate(debaters):
                others = "\n\n".join(
                    f"[{debaters[j].id}] answer: {p['answer']}\nreasoning: {p['reasoning']}"
                    for j, p in enumerate(positions)
                    if j != i
                )
                prompts.append(
                    f"Other agents answered the same question:\n\n{others}\n\n"
                    "Consider their reasoning as additional evidence. Keep or "
                    f"update your answer.\n\n{base}"
                )
            positions = list(
                await asyncio.gather(
                    *(d.ask(p, parse=parse_position) for d, p in zip(debaters, prompts))
                )
            )
            answers = {canon(p["answer"]) for p in positions}
            print(f"round {round_no}: {len(answers)} distinct answer(s)")

        # Majority over the final positions.
        votes = Counter(canon(p["answer"]) for p in positions)
        winner, n = votes.most_common(1)[0]
        await c.note(f"multiagent debate: '{winner}' won {n}/{N_DEBATERS} votes")
        print(f"\nfinal answer: {winner} ({n}/{N_DEBATERS} votes)")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_QUESTION))
