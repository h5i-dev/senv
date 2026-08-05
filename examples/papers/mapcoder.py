"""MapCoder: Multi-Agent Code Generation for Competitive Problem Solving
(Islam et al. 2024, arXiv:2405.11403) — retrieval → planning → coding →
plan-wise debugging.

Four stages mirror the human contest workflow. The retrieval agent
*self-generates* exemplars (similar problems it knows, each with a plan) —
no external database. The planner turns exemplars into several candidate
plans, each with a confidence score. The coder attempts plans in confidence
order; each attempt is neutrally executed, and failures get a bounded
debugging loop *within the current plan* before falling back to the next
one — MapCoder's key move: don't debug a doomed plan forever, switch plans.

    python examples/papers/mapcoder.py ["<task>"]   # default: implement quicksort with pytest
"""

import asyncio
import json
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor, Review, Verification

DEMO_TASK = "implement quicksort with pytest"
VERIFY = ["pytest", "-q"]
K_EXEMPLARS = 2
K_PLANS = 2
MAX_DEBUG = 2  # debugging attempts per plan


def parse_exemplars(value: Any) -> list[dict[str, str]]:
    items = value.get("exemplars") if isinstance(value, Mapping) else None
    if not isinstance(items, list) or not items:
        raise ValueError(
            'reply must be {"exemplars": [{"problem": "...", "plan": "..."}]}'
        )
    return [
        {"problem": str(e.get("problem", "")), "plan": str(e.get("plan", ""))}
        for e in items
    ][:K_EXEMPLARS]


def parse_plans(value: Any) -> list[dict[str, Any]]:
    items = value.get("plans") if isinstance(value, Mapping) else None
    if not isinstance(items, list) or not items:
        raise ValueError(
            'reply must be {"plans": [{"plan": "...", "confidence": <0-100>}]}'
        )
    plans = [
        {"plan": str(p.get("plan", "")), "confidence": float(p.get("confidence", 0))}
        for p in items
    ][:K_PLANS]
    return sorted(plans, key=lambda p: -p["confidence"])


def describe(v: Verification) -> str:
    line = (
        f"`{' '.join(v.command)}`: "
        + ("applied cleanly" if v.applies_cleanly else "failed to apply")
        + ", "
        + ("tests passed" if v.tests_passed else "tests FAILED")
    )
    if v.failure:
        line += f"\nfailure: {v.failure}"
    return line


async def main(task: str) -> None:
    async with Conductor(".", "mapcoder-demo", launcher="resident", isolation="supervised") as c:
        retrieval = await c.hire("retrieval", runtime="claude", model="claude-haiku-4-5")
        planner = await c.hire("planner", runtime="claude", model="claude-haiku-4-5")
        coder = await c.hire("coder", runtime="codex", model="gpt-5.4-mini", effort="medium")

        # 1. Retrieval: self-generated exemplars, no external database.
        exemplars = await retrieval.ask(
            f"Recall {K_EXEMPLARS} problems you know that are algorithmically "
            f"similar to: {task}\nFor each, state the problem and a "
            "step-by-step solution plan. Reply as JSON: "
            '{"exemplars": [{"problem": "...", "plan": "..."}]}',
            parse=parse_exemplars,
        )

        # 2. Planning: candidate plans, ranked by the planner's confidence.
        plans = await planner.ask(
            "Similar solved problems:\n"
            + json.dumps(exemplars, indent=2)
            + f"\n\nUsing them as guidance, write {K_PLANS} DISTINCT "
            f"step-by-step plans for: {task}\nRate each plan's confidence "
            '0-100. Reply as JSON: {"plans": [{"plan": "...", '
            '"confidence": <0-100>}]}',
            parse=parse_plans,
        )

        # 3-4. Coding + plan-wise debugging: attempt plans in confidence
        # order; debug within a plan only a bounded number of times.
        frozen = False
        green = False
        for rank, plan in enumerate(plans, 1):
            print(f"plan {rank} (confidence {plan['confidence']:.0f})")
            attempt = await coder.work(
                f"{task}\n\nFollow this plan strictly:\n{plan['plan']}",
                expect_independent=not frozen,
            )
            if not frozen:
                await c.freeze()
                frozen = True
            for debug in range(MAX_DEBUG + 1):
                verification = await c.verify(attempt, VERIFY)
                if verification.applies_cleanly and verification.tests_passed:
                    green = True
                    print(f"  green after {debug} debug turn(s)")
                    break
                if debug == MAX_DEBUG:
                    print(f"  plan {rank} exhausted its debug budget — switching plans")
                    break
                attempt = await coder.revise(
                    attempt,
                    Review(
                        reviewer="plan-debugger",
                        target=coder.id,
                        round=attempt.round,
                        body=(
                            "Verdict: REVISE\n\nStay on the current plan:\n"
                            f"{plan['plan']}\n\nNeutral execution failed:\n"
                            f"{describe(verification)}"
                        ),
                        referenced_artifacts=(attempt.id,),
                    ),
                )
            if green:
                break

        verdict = await c.judge()
        print("verdict:", verdict.selected_submission or "none", "—", *verdict.reasons)


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
