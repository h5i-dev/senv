"""Self-Consistency Improves Chain of Thought Reasoning in Language Models
(Wang et al. 2023, arXiv:2203.11171) — sample diverse reasoning paths,
marginalize by majority vote.

One greedy chain of thought is brittle; many independently sampled chains
converge on the right answer more often than any single one. Diversity here
comes from N independent seats — each its own session, asked the same
question in parallel with no cross-influence — and the vote is ordinary
Python over the journaled replies. Pure ``ask`` data turns: no artifacts,
no freeze. Swap ``canon`` for a similarity kernel to vote over open-ended
answers.

    python examples/papers/self_consistency.py ["<question>"]
"""

import asyncio
import sys
from collections import Counter
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_QUESTION = (
    "A bat and a ball cost $1.10 in total. The bat costs $1.00 more than "
    "the ball. How much does the ball cost, in cents?"
)
N_SAMPLES = 5


def parse_answer(value: Any) -> str:
    if not isinstance(value, Mapping) or "answer" not in value:
        raise ValueError('reply must be {"reasoning": "...", "answer": "..."}')
    return str(value["answer"]).strip()


def canon(answer: str) -> str:
    return answer.strip().lower()


async def main(question: str) -> None:
    prompt = (
        f"{question}\n\nReason step by step, then reply as JSON: "
        '{"reasoning": "<your chain of thought>", '
        '"answer": "<final answer, as short as possible>"}'
    )
    async with Conductor(".", "self-consistency-demo", launcher="resident", isolation="supervised") as c:
        seats = [
            await c.hire(f"sampler{i}", runtime="claude", model="claude-haiku-4-5")
            for i in range(N_SAMPLES)
        ]

        # N independent reasoning paths, in parallel (one ask per seat).
        answers = await asyncio.gather(
            *(seat.ask(prompt, parse=parse_answer) for seat in seats)
        )

        # Marginalize out the reasoning: majority over final answers.
        votes = Counter(canon(a) for a in answers)
        for answer, n in votes.most_common():
            print(f"{n}/{len(answers)}  {answer}")
        winner, n = votes.most_common(1)[0]
        await c.note(f"self-consistency: '{winner}' won {n}/{len(answers)} votes")
        print(f"\nconsensus answer: {winner}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_QUESTION))
