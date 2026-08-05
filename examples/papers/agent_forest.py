"""More Agents Is All You Need (Li et al. 2024, arXiv:2402.05120) —
sampling-and-voting, with its scaling curve.

The paper's finding: plain sampling-and-voting — spawn N agents on the same
input, take the majority — scales performance with ensemble size, no
scaffolding needed, and stacks on top of any other method. Here N seats
answer in parallel and the vote is host-side Python; the scaling flavor
comes free by re-voting over prefixes of the same samples (1, 3, …, N)
instead of paying for separate runs. Exact-match voting suits closed-form
answers; the paper uses pairwise similarity for open-ended ones — swap
``canon`` accordingly.

    python examples/papers/agent_forest.py ["<question>"]
"""

import asyncio
import sys
from collections import Counter
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_QUESTION = (
    "A farmer has a fox, a goose, and a bag of grain, and a boat that "
    "carries the farmer plus one item. The fox eats the goose, and the "
    "goose eats the grain, when left alone together. What is the minimum "
    "number of river crossings to get everything across?"
)
N_AGENTS = 5


def parse_answer(value: Any) -> str:
    if not isinstance(value, Mapping) or "answer" not in value:
        raise ValueError('reply must be {"reasoning": "...", "answer": "..."}')
    return str(value["answer"]).strip()


def canon(answer: str) -> str:
    return answer.strip().lower()


def majority(samples: list[str]) -> tuple[str, int]:
    winner, n = Counter(canon(s) for s in samples).most_common(1)[0]
    return winner, n


async def main(question: str) -> None:
    prompt = (
        f"{question}\n\nReason step by step, then reply as JSON: "
        '{"reasoning": "<your reasoning>", '
        '"answer": "<final answer, as short as possible>"}'
    )
    async with Conductor(".", "agent-forest-demo", launcher="resident", isolation="supervised") as c:
        forest = [
            await c.hire(f"tree{i}", runtime="claude", model="claude-haiku-4-5")
            for i in range(N_AGENTS)
        ]

        # The whole method: N independent samples, then vote.
        samples = list(
            await asyncio.gather(*(seat.ask(prompt, parse=parse_answer) for seat in forest))
        )

        # The scaling curve, from prefixes of the same sample set.
        for n in range(1, N_AGENTS + 1, 2):
            winner, votes = majority(samples[:n])
            print(f"ensemble size {n}: '{winner}' ({votes}/{n} votes)")

        winner, votes = majority(samples)
        await c.note(f"agent-forest: '{winner}' won {votes}/{N_AGENTS} votes")
        print(f"\nfinal answer: {winner}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_QUESTION))
