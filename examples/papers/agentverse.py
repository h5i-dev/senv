"""AgentVerse: Facilitating Multi-Agent Collaboration and Exploring
Emergent Behaviors (Chen et al. 2023, arXiv:2308.10848) — recruit,
collaborate, evaluate, re-recruit.

AgentVerse treats the *team composition itself* as part of the search: a
recruiter drafts the expert roles this specific task deserves, the recruits
collaborate, an evaluator scores the result — and if it is not satisfied,
the team is dissolved and a better-shaped one is recruited with the
feedback in hand. This score exploits an engine property: enrollment is
open-round-only, but a pure-``ask`` score never freezes, so it can *hire
new seats mid-run* — the recruit loop uses genuinely fresh agents each
attempt, not repainted ones.

    python examples/papers/agentverse.py ["<task>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_TASK = (
    "Propose a realistic one-week plan to cut a web service's cloud bill "
    "by 30% without hurting reliability."
)
MAX_ATTEMPTS = 2
MAX_EXPERTS = 3


def parse_roles(value: Any) -> list[str]:
    roles = value.get("roles") if isinstance(value, Mapping) else value
    if not isinstance(roles, list) or not roles:
        raise ValueError('reply must be {"roles": ["<expert role description>", ...]}')
    return [str(r).strip() for r in roles][:MAX_EXPERTS]


def parse_evaluation(value: Any) -> dict[str, Any]:
    if not isinstance(value, Mapping) or "satisfied" not in value:
        raise ValueError('reply must be {"satisfied": true|false, "feedback": "..."}')
    return {
        "satisfied": bool(value["satisfied"]),
        "feedback": str(value.get("feedback", "")).strip(),
    }


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


async def main(task: str) -> None:
    async with Conductor(".", "agentverse-demo", launcher="resident", isolation="supervised") as c:
        recruiter = await c.hire("recruiter", runtime="claude", model="claude-haiku-4-5")
        evaluator = await c.hire("evaluator", runtime="claude", model="claude-haiku-4-5")

        feedback = ""
        solution = ""
        for attempt in range(1, MAX_ATTEMPTS + 1):
            # Recruit: what team does THIS task (and this feedback) deserve?
            roles = await recruiter.ask(
                f"Task: {task}\n\n"
                + (
                    f"The previous team's result was rejected with feedback:\n"
                    f"{feedback}\n\nRecruit a better-shaped team.\n\n"
                    if feedback
                    else ""
                )
                + f"Name at most {MAX_EXPERTS} expert roles whose combination "
                'would best solve this task. Reply as JSON: {"roles": ["..."]}',
                parse=parse_roles,
            )
            print(f"attempt {attempt} team: {'; '.join(roles)}")

            # The run never freezes (pure ask), so the round is still open —
            # genuinely fresh seats can be hired for this attempt's team.
            experts = [
                await c.hire(
                    f"a{attempt}-expert{i}", runtime="claude", model="claude-haiku-4-5"
                )
                for i in range(len(roles))
            ]

            # Collaborate: role-played contributions, then synthesis.
            contributions = await asyncio.gather(
                *(
                    expert.ask(
                        f"You are: {role}\nTask: {task}\n"
                        + (f"Prior feedback to address: {feedback}\n" if feedback else "")
                        + "Contribute your role's part of the solution. "
                        "Reply as a single JSON string.",
                        parse=parse_text,
                    )
                    for expert, role in zip(experts, roles)
                )
            )
            solution = await recruiter.ask(
                f"Task: {task}\n\nYour recruits contributed:\n\n"
                + "\n\n".join(
                    f"[{role}]\n{text}" for role, text in zip(roles, contributions)
                )
                + "\n\nSynthesize the team's final solution. Reply as a "
                "single JSON string.",
                parse=parse_text,
            )

            # Evaluate: keep the team's work, or dissolve and re-recruit.
            evaluation = await evaluator.ask(
                f"Task: {task}\n\nProposed solution:\n{solution}\n\nIs this "
                "solution complete, concrete, and actionable? Reply as JSON: "
                '{"satisfied": true|false, "feedback": "<what is missing>"}',
                parse=parse_evaluation,
            )
            if evaluation["satisfied"]:
                print(f"attempt {attempt}: evaluator satisfied")
                break
            feedback = evaluation["feedback"]
            print(f"attempt {attempt}: rejected — {feedback}")

        await c.note(f"agentverse: finished after recruiting {attempt} team(s)")
        print(f"\nfinal solution:\n{solution}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
