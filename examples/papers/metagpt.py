"""MetaGPT: Meta Programming for a Multi-Agent Collaborative Framework
(Hong et al. 2024, arXiv:2308.00352) — an SOP where roles exchange
structured documents, not chat.

MetaGPT's core claim: cross-role communication should be *standardized
artifacts* (PRD, design doc, code, QA report), because free-form chat
compounds hallucination across hand-offs. Here the PRD and design are
validated-JSON ``ask`` turns; the engineer implements against the documents
alone; QA is both a real execution (``conductor.verify``) and a review
turn, merged into one revision — ``patterns.merge_reviews`` playing the
"QA report" document.

    python examples/papers/metagpt.py ["<task>"]   # default: implement quicksort with pytest
"""

import asyncio
import json
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor, Review, patterns

DEMO_TASK = "implement quicksort with pytest"
VERIFY = ["pytest", "-q"]


def parse_doc(*required: str):
    def parse(value: Any) -> dict[str, Any]:
        if not isinstance(value, Mapping):
            raise TypeError("reply must be a JSON object")
        missing = [k for k in required if k not in value]
        if missing:
            raise ValueError(f"document is missing required sections: {missing}")
        return dict(value)

    return parse


async def main(task: str) -> None:
    async with Conductor(".", "metagpt-demo", launcher="resident", isolation="supervised") as c:
        product_manager = await c.hire("pm", runtime="claude", model="claude-haiku-4-5")
        architect = await c.hire("architect", runtime="claude", model="claude-haiku-4-5")
        engineer = await c.hire("engineer", runtime="codex", model="gpt-5.4-mini", effort="medium")
        qa = await c.hire("qa", runtime="claude", model="claude-haiku-4-5")

        # SOP document 1 — the PRD, a structured artifact, not chat.
        prd = await product_manager.ask(
            f"You are the product manager. Write the PRD for: {task}\n"
            'Reply as JSON: {"goals": ["..."], "user_stories": ["..."], '
            '"acceptance_criteria": ["..."]}',
            parse=parse_doc("goals", "user_stories", "acceptance_criteria"),
        )

        # SOP document 2 — the design, grounded in the PRD.
        design = await architect.ask(
            "You are the architect. PRD:\n"
            + json.dumps(prd, indent=2)
            + '\n\nProduce the design. Reply as JSON: {"modules": '
            '[{"name": "...", "responsibility": "..."}], "interfaces": ["..."]}',
            parse=parse_doc("modules", "interfaces"),
        )

        # Implementation against the documents alone.
        artifact = await engineer.work(
            f"You are the engineer. Implement exactly this specification.\n"
            f"Task: {task}\n\nPRD:\n{json.dumps(prd, indent=2)}\n\n"
            f"Design:\n{json.dumps(design, indent=2)}",
            expect_independent=True,
        )
        await c.freeze()

        # QA: real execution + a review turn, folded into one QA report.
        verification = await c.verify(artifact, VERIFY)
        review = await qa.review(artifact)
        if not (verification.applies_cleanly and verification.tests_passed and review.approved):
            qa_report = patterns.merge_reviews(
                [
                    review,
                    Review(
                        reviewer="qa-verification",
                        target=engineer.id,
                        round=artifact.round,
                        body=(
                            "Verdict: REVISE\n\nNeutral execution of "
                            f"`{' '.join(verification.command)}`: "
                            + ("passed" if verification.tests_passed else "FAILED")
                            + (f"\nfailure: {verification.failure}" if verification.failure else "")
                            + "\n\nAcceptance criteria:\n"
                            + "\n".join(f"- {a}" for a in prd["acceptance_criteria"])
                        ),
                    ),
                ],
                artifact,
            )
            artifact = await engineer.revise(artifact, qa_report)
            await c.verify(artifact, VERIFY)

        verdict = await c.judge()
        print("PRD goals:", "; ".join(str(g) for g in prd["goals"]))
        print("modules:", "; ".join(m.get("name", "?") for m in design["modules"]))
        print("verdict:", verdict.selected_submission or "none", "—", *verdict.reasons)


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_TASK))
