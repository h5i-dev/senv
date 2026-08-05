"""ChatDev: Communicative Agents for Software Development (Qian et al.
2024, arXiv:2307.07924) — a chat chain: phases, each a two-role dialogue.

ChatDev decomposes the waterfall into phases (design → coding → review →
testing → documentation) and staffs every phase with exactly two roles: an
instructor who steers and an assistant who produces, talking until the
phase's deliverable is agreed. Here the design and documentation phases are
alternating ``ask`` turns; the coding phase is a real work turn; review and
testing are ChatDev's "thought instructions" mapped onto h5i's native
review/revise turns and neutral verification.

    python examples/papers/chatdev.py ["<task>"]   # default: implement quicksort with pytest
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor, Review, Verification

DEMO_TASK = "implement quicksort with pytest"
VERIFY = ["pytest", "-q"]
DESIGN_TURNS = 2
REVIEW_ROUNDS = 2
TEST_ROUNDS = 2


def parse_move(value: Any) -> dict[str, Any]:
    if not isinstance(value, Mapping) or "message" not in value:
        raise ValueError('reply must be {"message": "...", "settled": true|false}')
    return {"message": str(value["message"]).strip(), "settled": bool(value.get("settled", False))}


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


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
    async with Conductor(".", "chatdev-demo", launcher="resident", isolation="supervised") as c:
        instructor = await c.hire("instructor", runtime="claude", model="claude-haiku-4-5")
        programmer = await c.hire("programmer", runtime="codex", model="gpt-5.4-mini", effort="medium")
        reviewer = await c.hire("reviewer", runtime="claude", model="claude-haiku-4-5")

        # Phase 1 — design: instructor ↔ programmer until the spec settles.
        dialogue = ""
        spec = ""
        for turn in range(1, DESIGN_TURNS + 1):
            move = await instructor.ask(
                f"You are the instructor in the DESIGN phase for: {task}\n"
                f"Dialogue so far:\n{dialogue or '(start)'}\n\n"
                "Steer the design: raise the next decision to settle, or — "
                "if the design is complete — restate the agreed spec and set "
                'settled=true. Reply as JSON: {"message": "...", '
                '"settled": true|false}',
                parse=parse_move,
            )
            if move["settled"]:
                spec = move["message"]
                break
            reply = await programmer.ask(
                f"You are the assistant in the DESIGN phase for: {task}\n"
                f"Dialogue so far:\n{dialogue}\nInstructor: {move['message']}\n\n"
                "Answer the design question concretely. Reply as a single "
                "JSON string.",
                parse=parse_text,
            )
            dialogue += f"instructor: {move['message']}\nprogrammer: {reply}\n"
            spec = dialogue
        print(f"design settled:\n{spec[:200]}")

        # Phase 2 — coding: one real work turn against the agreed design.
        artifact = await programmer.work(
            f"{task}\n\nAgreed design from the design phase:\n{spec}",
            expect_independent=True,
        )
        await c.freeze()

        # Phase 3 — code review: reviewer ↔ programmer on the artifact.
        for round_no in range(1, REVIEW_ROUNDS + 1):
            review = await reviewer.review(artifact)
            if review.approved:
                print(f"review round {round_no}: approved")
                break
            artifact = await programmer.revise(artifact, review)

        # Phase 4 — testing: neutral execution, failures loop back.
        for round_no in range(1, TEST_ROUNDS + 1):
            verification = await c.verify(artifact, VERIFY)
            if verification.applies_cleanly and verification.tests_passed:
                print(f"test round {round_no}: green")
                break
            if round_no == TEST_ROUNDS:
                break
            artifact = await programmer.revise(
                artifact,
                Review(
                    reviewer="tester",
                    target=programmer.id,
                    round=artifact.round,
                    body=f"Verdict: REVISE\n\n{describe(verification)}",
                    referenced_artifacts=(artifact.id,),
                ),
            )

        # Phase 5 — documentation: the user manual, recorded on the run.
        manual = await instructor.ask(
            f"Write a short user manual for what was just built ({task}): "
            "what it does, how to run it, how to run its tests. Reply as a "
            "single JSON string.",
            parse=parse_text,
        )
        await c.note(f"chatdev manual:\n{manual}")

        verdict = await c.judge()
        print("verdict:", verdict.selected_submission or "none", "—", *verdict.reasons)
        print(f"\nmanual:\n{manual[:400]}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
