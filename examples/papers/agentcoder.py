"""AgentCoder: Multi-Agent-based Code Generation with Effective Testing and
Optimisation (Huang et al. 2024, arXiv:2312.13010) — programmer / test
designer / test executor.

The test designer writes tests *without seeing the implementation* — tests
written after the code inherit its blind spots. So both seats work in
parallel before the freeze (h5i stamps both artifacts independent), then
the programmer is shown the test suite as sealed-phase materials and must
satisfy it. The test executor role is not an LLM at all:
``conductor.verify`` runs the suite neutrally with the designer's artifact
as a **sealed overlay** (``sealed_from=tests``): the designer's test files
are overlaid over the programmer's candidate at verify time, so the
programmer *cannot* weaken or skip them — an edit to a sealed path is
discarded and surfaced as ``sealed_overridden`` tamper evidence, and the
programmer needn't even copy the tests into its tree. Each failure loops
back to the programmer as a constructed review.

    python examples/papers/agentcoder.py ["<task>"]   # default: implement quicksort with pytest
"""

import asyncio
import sys

from h5i.orchestra import Conductor, Review, Verification

DEMO_TASK = "implement quicksort with pytest"
VERIFY = ["pytest", "-q"]
MAX_ITERATIONS = 3


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
    async with Conductor(".", "agentcoder-demo", launcher="resident", isolation="supervised") as c:
        programmer = await c.hire("programmer", runtime="claude", model="claude-haiku-4-5")
        test_designer = await c.hire(
            "test-designer", runtime="codex", model="gpt-5.4-mini", effort="medium"
        )

        # Implementation and test suite in parallel, mutually blind.
        _implementation, tests = await asyncio.gather(
            programmer.work(task, expect_independent=True),
            test_designer.work(
                f"Design the test suite ONLY for: {task}\nWrite thorough "
                "tests (basic, edge, and large-scale cases). Do NOT write "
                "the implementation itself.",
                expect_independent=True,
            ),
        )
        await c.freeze()

        # The programmer sees the independent test suite as materials. It
        # may copy the tests locally to iterate, but the copy carries no
        # authority: verification below overlays the designer's originals.
        candidate = await programmer.work(
            "The granted teammate artifact is an independently designed test "
            "suite for your task. Make your implementation honestly satisfy "
            "it — you may copy the tests into your worktree to run them "
            "locally, but the neutral verifier always uses the designer's "
            "originals, so editing them cannot help you.",
            materials=[tests],
        )

        # Test executor loop: neutral runs against the SEALED designer
        # tests, failures loop back as reviews.
        for iteration in range(1, MAX_ITERATIONS + 1):
            verification = await c.verify(candidate, VERIFY, sealed_from=tests)
            if verification.sealed_overridden:
                print(
                    "note: candidate edits to sealed test paths were "
                    f"discarded: {', '.join(verification.sealed_overridden)}"
                )
            if verification.applies_cleanly and verification.tests_passed:
                print(f"iteration {iteration}: test executor is green")
                break
            evidence = describe(verification)
            print(f"iteration {iteration}: tests failed")
            if iteration == MAX_ITERATIONS:
                break
            candidate = await programmer.revise(
                candidate,
                Review(
                    reviewer="test-executor",
                    target=programmer.id,
                    round=candidate.round,
                    body=(
                        "Verdict: REVISE\n\nThe independently designed test "
                        "suite (sealed — your copy of it carries no authority) "
                        f"failed in a neutral worktree:\n{evidence}\n\n"
                        "Fix the implementation; editing tests cannot help."
                    ),
                    referenced_artifacts=(candidate.id,),
                ),
            )

        verdict = await c.judge()
        print("verdict:", verdict.selected_submission or "none", "—", *verdict.reasons)


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
