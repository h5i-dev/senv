#!/usr/bin/env python3
"""Pure helpers for the custom Arith-v0 task.

Nothing here loads a model or opens a socket. The expression evaluator is a
server-correctness check. The learned policy does not call it.
"""

from __future__ import annotations

import math

VAL_START = 2_000
VAL_COUNT = 40
TEST_START = 10_000
TEST_COUNT = 200
TRAIN_START = 0
TRAIN_COUNT = 200


def seeds_for(split: str, limit: int = 0) -> list[int]:
    if split == "val":
        start, count = VAL_START, VAL_COUNT
    elif split == "test":
        start, count = TEST_START, TEST_COUNT
    elif split == "train":
        start, count = TRAIN_START, TRAIN_COUNT
    else:
        raise ValueError(f"unknown split {split}")
    if limit > 0:
        count = min(count, limit)
    return list(range(start, start + count))


def ranges_are_disjoint() -> bool:
    spans = (
        set(seeds_for("train")),
        set(seeds_for("val")),
        set(seeds_for("test")),
    )
    return spans[0].isdisjoint(spans[1]) and spans[0].isdisjoint(spans[2]) and spans[1].isdisjoint(spans[2])


def expression_from_prompt(prompt: str) -> str:
    for line in prompt.splitlines():
        if line.startswith("Expression: "):
            return line[len("Expression: ") :].strip()
    raise ValueError("prompt has no Expression line")


def eval_expr(text: str) -> int:
    """Evaluate one fully parenthesized left-associative expression.

    Digits are single characters. This mirrors `expression` in src/arith.rs.
    """

    text = text.strip()
    if len(text) == 1 and text.isdigit():
        return int(text)
    if len(text) < 5 or text[0] != "(" or text[-1] != ")":
        raise ValueError(f"not a parenthesized expression: {text!r}")
    inner = text[1:-1]
    depth = 0
    op_at = None
    for index, ch in enumerate(inner):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth < 0:
                raise ValueError(text)
        elif ch in "+-*" and depth == 0:
            op_at = index
    if op_at is None or depth != 0:
        raise ValueError(text)
    left = eval_expr(inner[:op_at].strip())
    right = eval_expr(inner[op_at + 1 :].strip())
    op = inner[op_at]
    if op == "+":
        return left + right
    if op == "-":
        return left - right
    if op == "*":
        return left * right
    raise ValueError(op)


def action_line(text: str) -> str:
    for line in text.splitlines():
        stripped = line.strip()
        if stripped:
            return stripped
    return ""


def wilson(successes: int, total: int, z: float = 1.96) -> dict | None:
    """95% Wilson interval for a binomial proportion. Returns bounds, not a story."""

    if total <= 0:
        return None
    p = successes / total
    z2 = z * z
    denom = 1.0 + z2 / total
    center = (p + z2 / (2.0 * total)) / denom
    half = z * math.sqrt(p * (1.0 - p) / total + z2 / (4.0 * total * total)) / denom
    return {"low": center - half, "high": center + half, "z": z}


def choose_ops(records: list[dict]) -> dict:
    """Freeze the largest operator count whose validation success sits in band.

    `records` must be validation pilots. The test split is not an input.
    """

    if not records:
        raise ValueError("no pilot records")
    for record in records:
        if record.get("split") != "val":
            raise ValueError("difficulty choice saw a non-validation record")
    for ops in (4, 3, 2):
        band = [
            record
            for record in records
            if int(record["ops"]) == ops and 0.05 <= float(record["success_rate"]) <= 0.75
        ]
        if band:
            return {
                "ops": ops,
                "rule": "largest_ops_with_a_model_in_[0.05,0.75]",
                "matched": [record["model_id"] for record in band],
            }
    return {
        "ops": 3,
        "rule": "no_validation_success_in_[0.05,0.75]_frozen_at_3",
        "matched": [],
    }


def aggregate(episodes: list[dict]) -> dict:
    rewards = [float(episode["reward"]) for episode in episodes]
    successes = sum(1 for episode in episodes if episode["success"])
    format_rates = [float(episode["format_rate"]) for episode in episodes]
    total = len(episodes)
    summary = {
        "episodes": total,
        "successes": successes,
        "success_rate": (successes / total) if total else None,
        "success_wilson95": wilson(successes, total),
        "mean_reward": (sum(rewards) / total) if total else None,
        "mean_format_rate": (sum(format_rates) / total) if total else None,
        "mean_steps": (sum(int(episode["steps"]) for episode in episodes) / total) if total else None,
    }
    if episodes and "answer_line_rate" in episodes[0]:
        summary["mean_answer_line_rate"] = sum(float(episode["answer_line_rate"]) for episode in episodes) / total
    return summary


def self_test() -> None:
    if not ranges_are_disjoint():
        raise SystemExit("train, val, and test seeds overlap")
    if eval_expr("((2 * 3) + 4)") != 10:
        raise SystemExit("((2 * 3) + 4)")
    if eval_expr("(((8 - 3) * 2) + 5)") != 15:
        raise SystemExit("nested")
    if eval_expr("(1 - 9)") != -8:
        raise SystemExit("sub")
    if action_line("\n  answer 4\nmore") != "answer 4":
        raise SystemExit("action line")
    interval = wilson(100, 200)
    if interval is None or not interval["low"] <= 0.5 <= interval["high"]:
        raise SystemExit(f"wilson {interval}")
    if wilson(0, 0) is not None:
        raise SystemExit("empty wilson")
    chosen = choose_ops(
        [
            {"split": "val", "ops": 2, "model_id": "a", "success_rate": 0.9},
            {"split": "val", "ops": 3, "model_id": "a", "success_rate": 0.4},
            {"split": "val", "ops": 3, "model_id": "b", "success_rate": 0.0},
            {"split": "val", "ops": 4, "model_id": "a", "success_rate": 0.0},
            {"split": "val", "ops": 4, "model_id": "b", "success_rate": 0.02},
        ]
    )
    if chosen["ops"] != 3:
        raise SystemExit(f"choose {chosen}")
    fallback = choose_ops(
        [
            {"split": "val", "ops": 2, "model_id": "a", "success_rate": 0.0},
            {"split": "val", "ops": 3, "model_id": "a", "success_rate": 1.0},
            {"split": "val", "ops": 4, "model_id": "a", "success_rate": 0.0},
        ]
    )
    if fallback["ops"] != 3 or "frozen_at_3" not in fallback["rule"]:
        raise SystemExit(f"fallback {fallback}")
    try:
        choose_ops([{"split": "test", "ops": 3, "model_id": "a", "success_rate": 0.2}])
    except ValueError:
        pass
    else:
        raise SystemExit("test records must not choose the difficulty")


if __name__ == "__main__":
    self_test()
    print("arith_task self-test ok")
