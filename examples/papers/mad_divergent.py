"""Encouraging Divergent Thinking in Large Language Models through
Multi-Agent Debate (Liang et al. 2024, arXiv:2305.19118) — the MAD
tit-for-tat: affirmative vs. negative under an adaptive judge.

Self-reflection gets stuck in its own frame ("degeneration of thought");
MAD forces divergence by making one side *obligated to disagree*. An
affirmative seat proposes, a negative seat must rebut, and after every
exchange a judge decides whether the debate has produced a defensible
answer — stopping adaptively rather than after a fixed budget. Different
runtimes on the two sides sharpen the disagreement.

    python examples/papers/mad_divergent.py ["<question>"]
"""

import asyncio
import sys
from collections.abc import Mapping
from typing import Any

from h5i.orchestra import Conductor

DEMO_QUESTION = (
    "When Alice was 6, her sister was half her age. Alice is now 70. "
    "How old is her sister?"
)
MAX_ROUNDS = 3


def parse_text(value: Any) -> str:
    return value if isinstance(value, str) else str(value)


def parse_ruling(value: Any) -> dict[str, Any]:
    if not isinstance(value, Mapping) or "resolved" not in value:
        raise ValueError(
            'reply must be {"resolved": true|false, "answer": "...", "rationale": "..."}'
        )
    return {
        "resolved": bool(value["resolved"]),
        "answer": str(value.get("answer", "")).strip(),
        "rationale": str(value.get("rationale", "")).strip(),
    }


def rendered(transcript: list[tuple[str, str]]) -> str:
    return "".join(f"[{who}] {what}\n" for who, what in transcript)


async def main(question: str) -> None:
    async with Conductor(".", "mad-demo", launcher="resident", isolation="supervised") as c:
        affirmative = await c.hire("affirmative", runtime="claude", model="claude-haiku-4-5")
        negative = await c.hire("negative", runtime="codex", model="gpt-5.4-mini", effort="medium")
        judge = await c.hire("judge", runtime="claude", model="claude-haiku-4-5")

        transcript: list[tuple[str, str]] = []
        opening = await affirmative.ask(
            f"Question: {question}\n\nGive your answer and your strongest "
            "supporting argument. Reply as a single JSON string.",
            parse=parse_text,
        )
        transcript.append((affirmative.id, opening))

        ruling: dict[str, Any] = {"resolved": False, "answer": "", "rationale": ""}
        for round_no in range(1, MAX_ROUNDS + 1):
            # The negative side is OBLIGATED to disagree — that is the
            # divergence mechanism.
            rebuttal = await negative.ask(
                f"Question: {question}\n\nDebate so far:\n{rendered(transcript)}\n"
                "You are the negative side: you MUST disagree with the "
                "affirmative's latest position. Find the flaw, or the "
                "strongest alternative answer, and argue it. Reply as a "
                "single JSON string.",
                parse=parse_text,
            )
            transcript.append((negative.id, rebuttal))

            # Adaptive stop: the judge rules after every exchange.
            ruling = await judge.ask(
                f"You judge this debate.\nQuestion: {question}\n\n"
                f"Transcript:\n{rendered(transcript)}\n"
                "Has a defensible final answer emerged? Reply as JSON: "
                '{"resolved": true|false, "answer": "<final answer if '
                'resolved, else your current lean>", "rationale": "<why>"}',
                parse=parse_ruling,
            )
            print(f"round {round_no}: resolved={ruling['resolved']}")
            if ruling["resolved"] or round_no == MAX_ROUNDS:
                break

            defense = await affirmative.ask(
                f"Question: {question}\n\nDebate so far:\n{rendered(transcript)}\n"
                "Respond to the negative side: defend, repair, or — if it is "
                "genuinely right — concede and adopt the corrected answer. "
                "Reply as a single JSON string.",
                parse=parse_text,
            )
            transcript.append((affirmative.id, defense))

        for who, what in transcript:
            print(f"[{who}] {what[:110]}…")
        await c.note(f"MAD verdict: {ruling['answer']} — {ruling['rationale']}")
        print(f"\njudge's answer: {ruling['answer']}\nrationale: {ruling['rationale']}")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else DEMO_QUESTION))
