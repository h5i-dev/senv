"""Code Generation with AlphaCodium: From Prompt Engineering to Flow
Engineering (Ridnik et al. 2024, arXiv:2401.08500) — reflect on the
problem, generate extra tests, then iterate.

AlphaCodium's bet is *flow engineering*: most of the value comes before
and after generation, not from a better prompt. The flow: structured
problem reflection → reasoning about the given tests → generating
additional AI tests that probe edge cases → code → an iterate loop that
must keep both the public and the AI-generated tests green. Reflection
and test generation are validated-JSON ``ask`` turns; the iterate loop is
``verify`` + revise.

    python examples/papers/alphacodium.py ["<task>"]   # default: implement quicksort with pytest
"""

import asyncio
import json
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor, Review, Verification

DEMO_TASK = "implement quicksort with pytest"
VERIFY = ["pytest", "-q"]
MAX_ITERATIONS = 3
N_AI_TESTS = 4


def parse_reflection(value: Any) -> dict[str, Any]:
    if not isinstance(value, Mapping):
        raise TypeError("reply must be a JSON object")
    missing = [k for k in ("goal", "inputs", "outputs", "edge_cases") if k not in value]
    if missing:
        raise ValueError(f"reflection is missing sections: {missing}")
    return dict(value)


def parse_tests(value: Any) -> list[str]:
    tests = value.get("tests") if isinstance(value, Mapping) else value
    if not isinstance(tests, list) or not tests:
        raise ValueError('reply must be {"tests": ["<test description>", ...]}')
    return [str(t).strip() for t in tests][:N_AI_TESTS]


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
    async with Conductor(".", "alphacodium-demo", launcher="resident", isolation="supervised") as c:
        analyst = await c.hire("analyst", runtime="claude", model="claude-haiku-4-5")
        coder = await c.hire("coder", runtime="codex", model="gpt-5.4-mini", effort="medium")

        # 1. Structured problem reflection — before any code.
        reflection = await analyst.ask(
            f"Problem: {task}\n\nReflect on it in structured form. Reply as "
            'JSON: {"goal": "...", "inputs": "...", "outputs": "...", '
            '"edge_cases": ["..."]}',
            parse=parse_reflection,
        )

        # 2. AI test generation — probe the edge cases the reflection found.
        ai_tests = await analyst.ask(
            f"Problem: {task}\nReflection:\n{json.dumps(reflection, indent=2)}\n\n"
            f"Design {N_AI_TESTS} ADDITIONAL tests that probe the edge cases "
            "and failure modes a naive solution would miss. Reply as JSON: "
            '{"tests": ["<concise test description>", ...]}',
            parse=parse_tests,
        )
        print("AI tests:", "; ".join(ai_tests))

        # 3. Code against reflection + all tests (public and AI-generated).
        artifact = await coder.work(
            f"{task}\n\nProblem reflection:\n{json.dumps(reflection, indent=2)}\n\n"
            "Beyond the task's own tests, your test suite MUST also cover:\n"
            + "\n".join(f"- {t}" for t in ai_tests),
            expect_independent=True,
        )
        await c.freeze()

        # 4. Iterate: everything must stay green.
        for iteration in range(1, MAX_ITERATIONS + 1):
            verification = await c.verify(artifact, VERIFY)
            if verification.applies_cleanly and verification.tests_passed:
                print(f"iteration {iteration}: all tests green")
                break
            if iteration == MAX_ITERATIONS:
                break
            artifact = await coder.revise(
                artifact,
                Review(
                    reviewer="flow-iterate",
                    target=coder.id,
                    round=artifact.round,
                    body=(
                        "Verdict: REVISE\n\nThe suite (public + AI-generated "
                        f"tests) failed neutrally:\n{describe(verification)}\n\n"
                        "Fix the implementation; keep every test."
                    ),
                    referenced_artifacts=(artifact.id,),
                ),
            )
            print(f"iteration {iteration}: red — revised")

        verdict = await c.judge()
        print("verdict:", verdict.selected_submission or "none", "—", *verdict.reasons)


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
