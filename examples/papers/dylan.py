"""A Dynamic LLM-Powered Agent Network for Task-Oriented Agent
Collaboration (Liu et al. 2023, arXiv:2310.02170) — DyLAN: the team is
mutable state.

Fixed teams waste turns on agents that contribute nothing. DyLAN runs
rounds of collaboration and *prunes the roster as it goes*: after each
round a ranker scores every active agent's contribution (the agent
importance signal), the least valuable seat is deactivated, and the
exchange stops early once the survivors agree. Team membership is an
ordinary Python list — deactivation is `active.remove(...)`, which no
static workflow graph can express.

    python examples/papers/dylan.py ["<question>"]
"""

import asyncio
import sys
from collections import Counter
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_QUESTION = (
    "You flip two fair coins. Given that at least one is heads, what is "
    "the probability that both are heads?"
)
MAX_ROUNDS = 3
MIN_TEAM = 2


def parse_position(value: Any) -> dict[str, str]:
    if not isinstance(value, Mapping) or "answer" not in value:
        raise ValueError('reply must be {"reasoning": "...", "answer": "..."}')
    return {
        "answer": str(value["answer"]).strip(),
        "reasoning": str(value.get("reasoning", "")).strip(),
    }


def parse_scores(names: list[str]):
    def parse(value: Any) -> dict[str, float]:
        scores = value.get("scores") if isinstance(value, Mapping) else None
        if not isinstance(scores, Mapping) or set(scores) != set(names):
            raise ValueError(
                f'reply must be {{"scores": {{<agent>: <0-10>}}}} covering exactly {names}'
            )
        return {k: max(0.0, min(10.0, float(v))) for k, v in scores.items()}

    return parse


def canon(answer: str) -> str:
    return answer.strip().lower()


async def main(question: str) -> None:
    base = (
        f"{question}\n\nReply as JSON: "
        '{"reasoning": "<your reasoning>", "answer": "<final answer>"}'
    )
    async with Conductor(".", "dylan-demo", launcher="resident", isolation="supervised") as c:
        active = [
            await c.hire("solver0", runtime="claude", model="claude-haiku-4-5"),
            await c.hire("solver1", runtime="claude", model="claude-haiku-4-5"),
            await c.hire("solver2", runtime="codex", model="gpt-5.4-mini", effort="medium"),
            await c.hire("solver3", runtime="claude", model="claude-haiku-4-5"),
        ]
        ranker = await c.hire("ranker", runtime="claude", model="claude-haiku-4-5")
        importance: dict[str, float] = {a.id: 0.0 for a in active}

        positions: dict[str, dict[str, str]] = {}
        for round_no in range(1, MAX_ROUNDS + 1):
            # One collaboration round: active agents answer, seeing the
            # other active agents' previous positions.
            prompts = []
            for agent in active:
                heard = "\n\n".join(
                    f"[{aid}] answer: {p['answer']}\nreasoning: {p['reasoning']}"
                    for aid, p in positions.items()
                    if aid != agent.id
                )
                prompts.append(
                    (f"Peers' previous positions:\n\n{heard}\n\n" if heard else "")
                    + f"Round {round_no}.\n\n{base}"
                )
            answers = await asyncio.gather(
                *(a.ask(p, parse=parse_position) for a, p in zip(active, prompts))
            )
            positions = {a.id: pos for a, pos in zip(active, answers)}

            # Early stop: the survivors agree.
            if len({canon(p["answer"]) for p in positions.values()}) == 1:
                print(f"round {round_no}: consensus among {len(active)} active agents")
                break

            # Agent importance: rank contributions, deactivate the weakest.
            names = [a.id for a in active]
            rendered = "\n\n".join(
                f"[{aid}] answer: {p['answer']}\nreasoning: {p['reasoning']}"
                for aid, p in positions.items()
            )
            scores = await ranker.ask(
                f"Question: {question}\n\nContributions this round:\n\n"
                f"{rendered}\n\nScore each agent's contribution 0-10 for "
                "correctness and usefulness to the team. Reply as JSON: "
                '{"scores": {"<agent>": <0-10>, ...}} covering every agent.',
                parse=parse_scores(names),
            )
            for aid, s in scores.items():
                importance[aid] += s
            if len(active) > MIN_TEAM and round_no < MAX_ROUNDS:
                weakest = min(active, key=lambda a: scores[a.id])
                active.remove(weakest)
                del positions[weakest.id]
                print(
                    f"round {round_no}: deactivated {weakest.id} "
                    f"(score {scores[weakest.id]:.1f}); roster now "
                    f"{[a.id for a in active]}"
                )

        votes = Counter(canon(p["answer"]) for p in positions.values())
        winner, n = votes.most_common(1)[0]
        ranked = sorted(importance.items(), key=lambda kv: -kv[1])
        await c.note(f"dylan: '{winner}' ({n}/{len(positions)}); importance {ranked}")
        print(f"\nfinal answer: {winner} ({n}/{len(positions)} active votes)")
        print("agent importance:", ", ".join(f"{k}={v:.1f}" for k, v in ranked))


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_QUESTION))
