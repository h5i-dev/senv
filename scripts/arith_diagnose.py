#!/usr/bin/env python3
"""Inference diagnosis for Arith-v0. Does not train and does not read the test split.

Compares a minimal chat completion with the experiment harness on a fixed
suite. Writes one JSON report. Large output belongs under /mnt/scratch.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from arith_experiment import (  # noqa: E402
    SYSTEM,
    action_line,
    action_of,
    connect_checked,
    close_client,
    generation_kwargs,
    load_bundle,
    observation_text,
    release_bundle,
    reseed,
)
from arith_task import expression_from_prompt, eval_expr  # noqa: E402


def boundary(tokenizer, prompt_ids, completion_ids, output_row, max_new: int) -> dict:
    prompt_len = int(prompt_ids.shape[0])
    prefix_matches = bool(output_row.shape[0] >= prompt_len) and bool(
        (output_row[:prompt_len].cpu() == prompt_ids.cpu()).all().item()
    )
    completion = [int(token) for token in completion_ids.tolist()]
    newline = tokenizer.encode("\n", add_special_tokens=False)
    eos = tokenizer.eos_token_id
    last = completion[-1] if completion else None
    if last is None:
        reason = "empty"
    elif newline and last == newline[0]:
        reason = "newline"
    elif eos is not None and last == eos:
        reason = "eos"
    elif len(completion) >= max_new:
        reason = "max_new_tokens"
    else:
        reason = "other"
    decoded_raw = tokenizer.decode(completion, skip_special_tokens=False)
    decoded_skip = tokenizer.decode(completion, skip_special_tokens=True)
    return {
        "prompt_tokens": prompt_len,
        "completion_tokens": len(completion),
        "prefix_matches_prompt": prefix_matches,
        "stop_reason": reason,
        "last_token_id": last,
        "newline_token_ids": newline,
        "eos_token_id": eos,
        "completion_token_ids": completion,
        "completion_token_strings": [tokenizer.decode([token]) for token in completion],
        "decoded_raw": decoded_raw,
        "decoded_skip_special": decoded_skip,
        "first_line": action_line(decoded_skip.split("\n", 1)[0]),
        "action_line": action_line(decoded_skip),
    }


def generate_raw(
    bundle,
    messages: list[dict],
    max_new: int,
    stop_on_newline: bool,
    tokenize_mode: str,
) -> dict:
    import torch

    tokenizer = bundle["tokenizer"]
    model = bundle["model"]
    device = bundle["device"]
    prompt = tokenizer.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
    native = tokenizer.apply_chat_template(messages, tokenize=True, add_generation_prompt=True)
    if hasattr(native, "keys"):
        native_list = native["input_ids"]
    else:
        native_list = native
    retokenized = tokenizer(prompt, return_tensors="pt", add_special_tokens=True)
    retokenized_no_special = tokenizer(prompt, return_tensors="pt", add_special_tokens=False)
    native_ids = [int(token) for token in native_list]
    retok_ids = [int(token) for token in retokenized["input_ids"][0].tolist()]
    retok_nospec_ids = [int(token) for token in retokenized_no_special["input_ids"][0].tolist()]
    if tokenize_mode == "native":
        chosen = torch.tensor([native_ids], dtype=torch.long)
    elif tokenize_mode == "retokenize":
        chosen = retokenized["input_ids"]
    else:
        raise SystemExit(f"unknown tokenize_mode {tokenize_mode}")
    attention = torch.ones_like(chosen)
    inputs = {"input_ids": chosen.to(device), "attention_mask": attention.to(device)}
    prompt_len = int(inputs["input_ids"].shape[1])
    kwargs = generation_kwargs(
        tokenizer,
        "greedy",
        max_new,
        stop_on_newline=stop_on_newline,
    )
    model.eval()
    with torch.no_grad():
        output = model.generate(**inputs, **kwargs)
    row = output[0]
    completion = row[prompt_len:].detach().cpu()
    prompt_ids = inputs["input_ids"][0].detach().cpu()
    detail = boundary(tokenizer, prompt_ids, completion, row.detach().cpu(), max_new)
    detail["generation_kwargs"] = {
        key: kwargs[key] for key in kwargs if key != "eos_token_id"
    }
    detail["eos_token_id_arg"] = kwargs["eos_token_id"]
    detail["prompt_text"] = prompt
    detail["native_template_token_ids"] = native_ids
    detail["retokenized_ids"] = retok_ids
    detail["retokenized_without_special_ids"] = retok_nospec_ids
    detail["native_equals_retokenized"] = native_ids == retok_ids
    detail["native_equals_retokenized_without_special"] = native_ids == retok_nospec_ids
    detail["max_new_tokens"] = max_new
    detail["stop_on_newline"] = stop_on_newline
    detail["tokenize_mode"] = tokenize_mode
    return detail


def logprob_gap(bundle) -> dict:
    """Compare the old untempered score with the sampling distribution the trainer now uses."""

    import torch

    from arith_experiment import TEMPERATURE, TOP_P, completion_logprobs, generate_line, policy_logprobs

    bundle["model"].eval()
    prompt_ids, completion_ids, line = generate_line(
        bundle,
        "What is 1+3? Reply with the integer only.",
        "sample",
    )
    device = bundle["device"]
    raw = completion_logprobs(
        bundle["model"], prompt_ids.to(device), completion_ids.to(device), temperature=1.0, top_p=1.0
    )
    tempered = completion_logprobs(
        bundle["model"],
        prompt_ids.to(device),
        completion_ids.to(device),
        temperature=TEMPERATURE,
        top_p=1.0,
    )
    sampling = policy_logprobs(bundle["model"], prompt_ids.to(device), completion_ids.to(device))
    return {
        "sampled_action_line": line,
        "temperature": TEMPERATURE,
        "top_p": TOP_P,
        "top_k": 0,
        "repetition_penalty": 1.0,
        "completion_tokens": int(completion_ids.shape[0]),
        "raw_logprob_sum": float(raw.sum().item()),
        "tempered_logprob_sum": float(tempered.sum().item()),
        "sampling_logprob_sum": float(sampling.sum().item()),
        "max_abs_token_gap_raw_vs_tempered": float((raw - tempered).abs().max().item()),
        "max_abs_token_gap_raw_vs_sampling": float((raw - sampling).abs().max().item()),
        "note": (
            "policy_logprobs is what the training step scores. "
            "It uses temperature and top-p. Top-k is disabled and repetition penalty is 1. "
            "The raw sum is the old untempered score."
        ),
    }


def verify_server() -> dict:
    """Known actions on validation seed 2000. The value is computed here, not read from state."""

    client = connect_checked()
    try:
        reset = client.reset(seed=2000, episode_id="diag-server")
        prompt = observation_text(reset.observation)
        expression = expression_from_prompt(prompt)
        value = eval_expr(expression)
        state = client.state()
        raw_state = getattr(state, "raw_state", None)
        rows = []
        for message in ("calc (6 * 8) * 9", "calc 6 * 8", "answer 1"):
            result = client.step(action_of(message))
            info = getattr(result.observation, "info", None) or {}
            text = observation_text(result.observation)
            rows.append(
                {
                    "action": message,
                    "reward": None if result.reward is None else float(result.reward),
                    "done": bool(result.done),
                    "reply": text.splitlines()[-1] if text else "",
                    "format_valid": info.get("arith.format_valid"),
                    "observation_contains_value": str(value) in text,
                }
            )
    finally:
        close_client(client)
    client = connect_checked()
    try:
        client.reset(seed=2000, episode_id="diag-server-correct")
        result = client.step(action_of(f"answer {value}"))
        info = getattr(result.observation, "info", None) or {}
        text = observation_text(result.observation)
        rows.append(
            {
                "action": f"answer {value}",
                "reward": None if result.reward is None else float(result.reward),
                "done": bool(result.done),
                "reply": text.splitlines()[-1] if text else "",
                "format_valid": info.get("arith.format_valid"),
                "observation_contains_value": str(value) in text,
            }
        )
    finally:
        close_client(client)
    return {
        "seed": 2000,
        "expression": expression,
        "independent_value": value,
        "prompt_contains_value": str(value) in prompt,
        "raw_state_keys": sorted(raw_state) if isinstance(raw_state, dict) else None,
        "actions": rows,
    }


def decoder_audit(bundle) -> dict:
    """Processors actually attached for a harness-shaped generate() call."""

    model = bundle["model"]
    tokenizer = bundle["tokenizer"]
    rows = {}
    for decode, stop_on_newline in (("greedy", True), ("sample", True), ("greedy", False)):
        kwargs = generation_kwargs(tokenizer, decode, 24, stop_on_newline)
        generation_config, _unused = model._prepare_generation_config(None, **kwargs)
        processors = model._get_logits_processor(
            generation_config,
            input_ids_seq_length=8,
            encoder_input_ids=None,
            prefix_allowed_tokens_fn=None,
            logits_processor=None,
            device="cpu",
        )
        label = f"{decode}_newline_{stop_on_newline}"
        rows[label] = {
            "temperature": generation_config.temperature,
            "top_p": generation_config.top_p,
            "top_k": generation_config.top_k,
            "repetition_penalty": generation_config.repetition_penalty,
            "do_sample": generation_config.do_sample,
            "processors": [type(item).__name__ for item in processors],
        }
    return rows


def feedback_probe(bundle, env_prompt: str) -> list[dict]:
    client = connect_checked()
    turns = []
    try:
        result = client.reset(seed=2000, episode_id="diag-feedback")
        for _ in range(3):
            seen = observation_text(result.observation)
            messages = [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": seen},
            ]
            # Same stop rule as generate_line: EOS only, so a later answer line remains.
            detail = generate_raw(bundle, messages, 24, False, "retokenize")
            result = client.step(action_of(detail["action_line"]))
            after = observation_text(result.observation)
            turns.append(
                {
                    "observation_before": seen,
                    "sent": detail["action_line"],
                    "first_line": detail["first_line"],
                    "decoded_raw": detail["decoded_raw"],
                    "stop_reason": detail["stop_reason"],
                    "reward": None if result.reward is None else float(result.reward),
                    "done": bool(result.done),
                    "observation_after": after,
                    "reply_visible_next": False,
                }
            )
            if result.done:
                break
        for index in range(len(turns) - 1):
            reply = turns[index]["observation_after"].splitlines()[-1]
            turns[index]["reply_visible_next"] = reply in turns[index + 1]["observation_before"]
    finally:
        close_client(client)
    if env_prompt and turns and not turns[0]["observation_before"].startswith("Expression:"):
        raise SystemExit("feedback reset did not return the environment prompt")
    return turns


def main() -> int:
    import torch

    out = Path(os.environ.get("SENV_DIAG_OUT", "/mnt/scratch/h5i-openenv/experiment-arith-diagnose"))
    out.mkdir(parents=True, exist_ok=True)
    env_prompt = None
    server = None
    if os.environ.get("SENV_BASE_URL"):
        server = verify_server()
        client = connect_checked()
        try:
            result = client.reset(seed=2000, episode_id="diag-prompt")
            env_prompt = observation_text(result.observation)
        finally:
            close_client(client)
    cases = [
        ("direct_1_plus_3", "What is 1+3? Reply with the integer only."),
        ("direct_1_plus_3_plain", "What is 1 + 3?"),
        ("one_op_8_minus_3", "What is 8 - 3? Reply with the integer only."),
        ("nested_2_mul_8_add_3", "What is ((2 * 8) + 3)? Reply with the integer only."),
        ("nested_6_mul_8_mul_9", "What is ((6 * 8) * 9)? Reply with the integer only."),
    ]
    if env_prompt:
        cases.append(("env_prompt_seed_2000", env_prompt))
    report = {
        "model_id": os.environ.get("SENV_MODEL_ID"),
        "requested_revision": os.environ.get("SENV_MODEL_REVISION"),
        "dtype": "bfloat16",
        "lora": False,
        "training": False,
        "cases": {},
        "server": server,
    }
    bundle = load_bundle(lora=False)
    try:
        report["meta"] = bundle["meta"]
        report["system_prompt"] = SYSTEM
        report["decoder"] = decoder_audit(bundle)
        reseed(1)
        report["logprob_temperature_gap"] = logprob_gap(bundle)
        minimal_system = "You are a helpful assistant."
        for name, user in cases:
            minimal_messages = [
                {"role": "system", "content": minimal_system},
                {"role": "user", "content": user},
            ]
            harness_messages = [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": user},
            ]
            report["cases"][name] = {
                "user": user,
                "minimal_native_eos_only_64": generate_raw(bundle, minimal_messages, 64, False, "native"),
                "harness_retokenize_newline_stop_24": generate_raw(
                    bundle, harness_messages, 24, True, "retokenize"
                ),
                "harness_native_newline_stop_24": generate_raw(
                    bundle, harness_messages, 24, True, "native"
                ),
                "harness_native_eos_only_64": generate_raw(bundle, harness_messages, 64, False, "native"),
            }
        if env_prompt:
            report["feedback_turns"] = feedback_probe(bundle, env_prompt)
        # fp32 reference. float() is in place, so the weights are cast back to bf16.
        messages = [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": cases[0][1]},
        ]
        nested = [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": cases[3][1]},
        ]
        bundle["model"].float()
        report["fp32_direct_1_plus_3"] = generate_raw(bundle, messages, 32, False, "native")
        report["fp32_harness_nested"] = generate_raw(bundle, nested, 32, False, "native")
        bundle["model"].to(dtype=torch.bfloat16)
        torch.cuda.empty_cache()
    finally:
        release_bundle(bundle)
    path = out / f"diagnose-{os.environ.get('SENV_DIAG_NAME', 'model')}.json"
    path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {path}", flush=True)
    for name, block in report["cases"].items():
        for key in (
            "minimal_native_eos_only_64",
            "harness_retokenize_newline_stop_24",
            "harness_native_newline_stop_24",
            "harness_native_eos_only_64",
        ):
            detail = block[key]
            print(
                f"{name} {key} stop={detail['stop_reason']} n={detail['completion_tokens']} "
                f"native_eq={detail['native_equals_retokenized']} "
                f"line={detail['action_line']!r}",
                flush=True,
            )
    gap = report["logprob_temperature_gap"]
    print(
        f"logprob raw {gap['raw_logprob_sum']:.4f} tempered {gap['tempered_logprob_sum']:.4f} "
        f"sampling {gap['sampling_logprob_sum']:.4f} "
        f"gap {gap['max_abs_token_gap_raw_vs_sampling']:.4f} line={gap['sampled_action_line']!r}",
        flush=True,
    )
    fp = report["fp32_direct_1_plus_3"]
    nested_fp = report["fp32_harness_nested"]
    print(
        f"fp32 direct line={fp['action_line']!r} harness_nested={nested_fp['action_line']!r}",
        flush=True,
    )
    if report.get("feedback_turns"):
        print("feedback", [turn["sent"] for turn in report["feedback_turns"]], flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
