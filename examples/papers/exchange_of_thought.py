"""Exchange-of-Thought: Enhancing Large Language Model Capabilities through
Cross-Model Communication (Yin et al. 2023, arXiv:2312.01823) — four
communication topologies over the same seats.

EoT's contribution is treating the *communication network* as the design
variable: Memory (bus — everyone sees the full shared history), Report
(star — spokes exchange only with a hub), Relay (ring — each agent hears
only its predecessor), and Debate (tree — pairwise exchange feeding a
parent). Each paradigm here is a small function that decides who-sees-what
before the same round of ``ask`` turns; termination is confidence-based —
an agent that keeps its answer across rounds gains confidence, and the
exchange stops when everyone is confident. Topology is just Python.

    python examples/papers/exchange_of_thought.py ["<question>"] [memory|report|relay|debate]
"""

import asyncio
import sys
from collections import Counter
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_QUESTION = (
    "A snail climbs 3 meters up a 10-meter wall each day and slips back 2 "
    "meters each night. On which day does it reach the top?"
)
N_AGENTS = 3
MAX_ROUNDS = 3
CONFIDENT_AFTER = 2  # identical answers in a row → confident


def parse_position(value: Any) -> dict[str, str]:
    if not isinstance(value, Mapping) or "answer" not in value:
        raise ValueError('reply must be {"reasoning": "...", "answer": "..."}')
    return {
        "answer": str(value["answer"]).strip(),
        "reasoning": str(value.get("reasoning", "")).strip(),
    }


def canon(answer: str) -> str:
    return answer.strip().lower()


def visible_peers(topology: str, me: int, n: int) -> list[int]:
    """Who agent `me` hears from — the entire difference between paradigms."""
    if topology == "memory":  # bus: everyone
        return [j for j in range(n) if j != me]
    if topology == "report":  # star: spokes hear the hub (agent 0), hub hears all
        return [j for j in range(n) if j != me] if me == 0 else [0]
    if topology == "relay":  # ring: only the predecessor
        return [(me - 1) % n]
    if topology == "debate":  # tree: siblings pair up; the root (0) hears both
        return [j for j in range(n) if j != me] if me == 0 else [me % 2 + 1]
    raise ValueError(f"unknown topology '{topology}'")


async def main(question: str, topology: str) -> None:
    base = (
        f"{question}\n\nReply as JSON: "
        '{"reasoning": "<your reasoning>", "answer": "<final answer>"}'
    )
    async with Conductor(".", f"eot-{topology}-demo", launcher="resident", isolation="supervised") as c:
        agents = [
            await c.hire("hub" if i == 0 else f"node{i}", runtime="claude", model="claude-haiku-4-5")
            for i in range(N_AGENTS)
        ]

        # Round 0: independent positions.
        positions = list(
            await asyncio.gather(*(a.ask(base, parse=parse_position) for a in agents))
        )
        streak = [1] * N_AGENTS  # rounds each agent has kept its answer

        for round_no in range(1, MAX_ROUNDS + 1):
            if all(s >= CONFIDENT_AFTER for s in streak):
                print(f"all agents confident before round {round_no}")
                break
            prompts = []
            for i in range(N_AGENTS):
                heard = "\n\n".join(
                    f"[{agents[j].id}] answer: {positions[j]['answer']}\n"
                    f"reasoning: {positions[j]['reasoning']}"
                    for j in visible_peers(topology, i, N_AGENTS)
                )
                prompts.append(
                    f"Communication round {round_no} (topology: {topology}). "
                    f"You received these positions:\n\n{heard}\n\n"
                    f"Reconsider and answer again.\n\n{base}"
                )
            updated = list(
                await asyncio.gather(
                    *(a.ask(p, parse=parse_position) for a, p in zip(agents, prompts))
                )
            )
            for i in range(N_AGENTS):
                same = canon(updated[i]["answer"]) == canon(positions[i]["answer"])
                streak[i] = streak[i] + 1 if same else 1
            positions = updated
            print(
                f"round {round_no}: "
                f"{len({canon(p['answer']) for p in positions})} distinct answer(s), "
                f"confidence streaks {streak}"
            )

        votes = Counter(canon(p["answer"]) for p in positions)
        winner, n = votes.most_common(1)[0]
        await c.note(f"exchange-of-thought[{topology}]: '{winner}' with {n}/{N_AGENTS}")
        print(f"\nfinal answer ({topology}): {winner} ({n}/{N_AGENTS})")


if __name__ == "__main__":
    question = sys.argv[1] if len(sys.argv) > 1 else DEMO_QUESTION
    chosen = sys.argv[2] if len(sys.argv) > 2 else "relay"
    asyncio.run(main(question, chosen))
