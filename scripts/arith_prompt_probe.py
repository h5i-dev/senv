#!/usr/bin/env python3
"""Validation-only prompt probe for custom Arith-v0.

This is a separate experiment from the frozen ops=3 run. It does not read the
test split and it does not train. The live server still parses and scores
every action. The probe only changes the text shown to the model. It does not
build candidate answers and it does not put the expression value in that text.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from arith_experiment import (
    action_of,
    close_client,
    connect_checked,
    generate_line,
    load_bundle,
    observation_text,
    release_bundle,
)
from arith_experiment import _reward
from arith_task import aggregate, expression_from_prompt, seeds_for

CLEAN_SYSTEM = (
    "Reply with one line. "
    "A calc line is the word calc, an integer, one of + - *, and an integer. "
    "An answer line is the word answer and an integer. "
    "Do not use parentheses. Do not chain operators."
)


def format_first_text(observation, with_example: bool) -> str:
    prompt = getattr(observation, "prompt", "") or ""
    expression = expression_from_prompt(prompt)
    if not expression:
        raise SystemExit("server prompt has no expression")
    lines = [
        "Reply with one line.",
        "A calc line has four tokens: the word calc, an integer, one of + - *, and an integer.",
        "An answer line has two tokens: the word answer, and an integer.",
        "Do not use parentheses. Do not chain operators.",
    ]
    if with_example:
        lines.extend(
            [
                "Example calc line, not this problem: calc 8 - 3",
                "Example answer line, not this problem: answer 5",
            ]
        )
    lines.extend(
        [
            f"Expression: {expression}",
            "Parentheses in the expression group left to right.",
            "A calc line does not finish the problem. An answer line submits the value of the whole expression.",
        ]
    )
    text = "\n".join(lines)
    if "<integer>" in text or "<op>" in text or "<int>" in text:
        raise SystemExit("format-first instruction still contains an angle-bracket template")
    # Server replies are appended after the check. An invalid action's reply
    # quotes `calc <int> <op> <int>` and that text is part of the environment.
    for message in getattr(observation, "messages", []) or []:
        content = getattr(message, "content", "") or ""
        if content:
            text = f"{text}\n{content}"
    return text


def user_text(variant: str, observation) -> str:
    if variant == "server_prompt":
        return observation_text(observation)
    if variant == "format_first":
        return format_first_text(observation, with_example=False)
    if variant == "format_example":
        return format_first_text(observation, with_example=True)
    raise SystemExit(f"unknown variant {variant}")


def system_for(variant: str) -> str | None:
    if variant == "server_prompt":
        return None
    return CLEAN_SYSTEM


def play(client, bundle, seed: int, variant: str, decode: str) -> dict:
    result = client.reset(seed=seed, episode_id=f"prompt-{variant}-{seed}")
    actions: list[str] = []
    valids: list[bool] = []
    reward = 0.0
    done = False
    for _ in range(8):
        if result.done:
            done = True
            break
        text = user_text(variant, result.observation)
        _prompt_ids, _completion_ids, line = generate_line(
            bundle, text, decode, system=system_for(variant)
        )
        actions.append(line)
        result = client.step(action_of(line))
        info = getattr(result.observation, "info", None) or {}
        valids.append(bool(info.get("arith.format_valid")))
        reward = _reward(result)
        done = bool(result.done)
    steps = len(actions)
    return {
        "seed": seed,
        "actions": actions,
        "steps": steps,
        "reward": reward,
        "success": bool(done and reward >= 0.999),
        "format_rate": (sum(valids) / steps) if steps else 0.0,
        "format_valid_actions": int(sum(valids)),
    }


def self_test() -> None:
    class Message:
        def __init__(self, content: str) -> None:
            self.content = content

    class Observation:
        prompt = "Expression: ((2 * 3) + 4)\ncalc <integer> <op> <integer>\n"
        messages = [Message("Result: 6")]

    text = format_first_text(Observation(), with_example=False)
    if "((2 * 3) + 4)" not in text or "Result: 6" not in text:
        raise SystemExit("format-first text dropped the public expression or the server reply")
    if "<integer>" in text:
        raise SystemExit("angle-bracket template leaked into the format-first text")
    if "answer 10" in text or text.strip().endswith("10"):
        raise SystemExit("format-first text includes the expression value")
    example = format_first_text(Observation(), with_example=True)
    if "calc 8 - 3" not in example or "answer 5" not in example:
        raise SystemExit("example variant lost its labeled examples")
    print("arith_prompt_probe self-test ok")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--variant", choices=("server_prompt", "format_first", "format_example"))
    parser.add_argument("--decode", default="greedy", choices=("greedy", "sample"))
    parser.add_argument("--limit", type=int, default=8)
    parser.add_argument("--out", default="")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    if not args.variant or not args.out:
        raise SystemExit("probe requires --variant and --out")
    if args.limit <= 0 or args.limit > 40:
        raise SystemExit("probe limit must stay inside the validation split")
    seeds = seeds_for("val", args.limit)
    if any(seed < 2000 or seed >= 2040 for seed in seeds):
        raise SystemExit("probe refused a seed outside validation")
    bundle = load_bundle(lora=False)
    client = connect_checked()
    started = time.perf_counter()
    try:
        episodes = [play(client, bundle, seed, args.variant, args.decode) for seed in seeds]
    finally:
        close_client(client)
        release_bundle(bundle)
    report = {
        "experiment": "arith-prompt-probe",
        "label": "separate from the frozen ops=3 run; validation only; no training",
        "variant": args.variant,
        "decode": args.decode,
        "split": "val",
        "summary": aggregate(episodes),
        "episodes": episodes,
        "wall_s": time.perf_counter() - started,
        "model_id": os.environ.get("SENV_MODEL_ID"),
        "model_revision": os.environ.get("SENV_MODEL_REVISION"),
        "candidate_filter": False,
        "test_split_used": False,
    }
    path = Path(args.out)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    printable = dict(report)
    printable.pop("episodes")
    print(json.dumps(printable, indent=2, sort_keys=True), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
