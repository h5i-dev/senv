#!/usr/bin/env python3
"""Policy-gradient experiment on custom Arith-v0 through the live senv server.

The task is not an upstream OpenEnv environment. Actions are free text. Nothing
here builds a list of candidate answers or scores one. The integer value of
the expression is not read from state(). Reward is the reward the Rust server
returns. Format validity is counted separately and is not added to the reward.

`--phase validate` checks that completion-token logprobs ignore the prompt and
that one LoRA step increases them. Training refuses to start when that check
fails.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from arith_task import action_line, aggregate, choose_ops, eval_expr, expression_from_prompt, seeds_for

SYSTEM = (
    "Reply with one line that starts with calc or answer. "
    "calc <integer> <op> <integer> uses one of + - *. "
    "Do not chain operators in one calc. "
    "answer <integer> submits the value of the whole expression."
)
LR = 1e-4
BASELINE_BETA = 0.05
GRAD_CLIP = 1.0
LORA_R = 8
MAX_NEW_TOKENS = 24
TEMPERATURE = 0.7
TOP_P = 0.95
VALIDATE_LR = 1e-4


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--phase",
        required=True,
        choices=("oracle", "baseline", "pretrained", "validate", "train", "choose"),
    )
    parser.add_argument("--split", default="val", choices=("val", "test", "train"))
    parser.add_argument("--decode", default="greedy", choices=("greedy", "sample"))
    parser.add_argument("--policy", default="random", choices=("random", "answer0"))
    parser.add_argument("--torch-seed", type=int, default=1)
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--train-episodes", type=int, default=200)
    parser.add_argument("--checkpoint-every", type=int, default=50)
    parser.add_argument("--out", required=True)
    parser.add_argument("--pilot-dir", default="")
    parser.add_argument("--adapter-dir", default="")
    parser.add_argument(
        "--answer-bonus",
        type=float,
        default=0.0,
        help="Added to the server reward during training only. 0 keeps the original objective.",
    )
    args = parser.parse_args()

    if args.split == "test" and os.environ.get("SENV_ARITH_FROZEN") != "1":
        print("refusing the test split before SENV_ARITH_FROZEN=1", file=sys.stderr)
        return 2
    if args.phase == "choose":
        return choose_phase(args)

    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    if args.phase == "oracle":
        report = run_oracle(args)
    elif args.phase == "baseline":
        report = run_baseline(args)
    elif args.phase == "pretrained":
        report = run_pretrained(args)
    elif args.phase == "validate":
        report = run_validate(args)
    elif args.phase == "train":
        report = run_train(args)
    else:
        print(f"unknown phase {args.phase}", file=sys.stderr)
        return 2
    out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    printable = {key: value for key, value in report.items() if key not in {"episodes", "curve"}}
    if "summary" in report:
        printable["summary"] = report["summary"]
    print(json.dumps(printable, indent=2, sort_keys=True), flush=True)
    return 0


def choose_phase(args: argparse.Namespace) -> int:
    records = []
    for path in sorted(Path(args.pilot_dir).glob("pilot-*.json")):
        payload = json.loads(path.read_text(encoding="utf-8"))
        summary = payload.get("summary") or {}
        records.append(
            {
                "split": payload.get("split"),
                "ops": payload.get("ops"),
                "model_id": payload.get("model_id"),
                "success_rate": summary.get("success_rate"),
                "mean_format_rate": summary.get("mean_format_rate"),
                "source": path.name,
            }
        )
    choice = choose_ops(records)
    choice["records"] = records
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(choice, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(choice, indent=2, sort_keys=True), flush=True)
    return 0


def run_oracle(args: argparse.Namespace) -> dict:
    client = connect_checked()
    try:
        episodes = []
        for seed in seeds_for("train", args.limit or 5):
            episodes.append(
                play_episode(
                    client,
                    seed,
                    f"oracle-{seed}",
                    lambda text: f"answer {eval_expr(expression_from_prompt(text))}",
                )
            )
        summary = aggregate(episodes)
        if summary["successes"] != summary["episodes"]:
            raise SystemExit(f"server check failed: {summary}")
        return {
            "phase": "oracle",
            "role": "server correctness check, not a baseline and not a learned policy",
            "split": "train",
            "ops": episodes[0].get("ops") if episodes else None,
            "summary": summary,
            "episodes": slim(episodes),
            **server_note(),
        }
    finally:
        close_client(client)


def run_baseline(args: argparse.Namespace) -> dict:
    client = connect_checked()
    try:
        episodes = []
        for seed in seeds_for(args.split, args.limit):
            rng = random.Random(f"{args.policy}-{seed}")

            def choose(text, rng=rng):
                del text
                if args.policy == "answer0":
                    return "answer 0"
                roll = rng.randrange(3)
                if roll == 0:
                    return "answer 0"
                if roll == 1:
                    return f"answer {rng.randint(-30, 30)}"
                op = rng.choice(("+", "-", "*"))
                return f"calc {rng.randint(1, 9)} {op} {rng.randint(1, 9)}"

            episodes.append(play_episode(client, seed, f"{args.policy}-{seed}", choose))
        return {
            "phase": "baseline",
            "policy": args.policy,
            "role": "simple baseline; it does not evaluate the expression",
            "split": args.split,
            "ops": episodes[0].get("ops") if episodes else None,
            "summary": aggregate(episodes),
            "episodes": slim(episodes),
            **server_note(),
        }
    finally:
        close_client(client)


def run_pretrained(args: argparse.Namespace) -> dict:
    bundle = load_bundle(lora=False)
    client = connect_checked()
    try:
        started = time.perf_counter()
        episodes = evaluate_lm(client, bundle, args.split, args.decode, args.limit, f"pretrained-{args.decode}")
        return {
            "phase": "pretrained",
            "split": args.split,
            "decode": args.decode,
            "lora": False,
            "ops": episodes[0].get("ops") if episodes else None,
            "summary": aggregate(episodes),
            "episodes": slim(episodes),
            "wall_s": time.perf_counter() - started,
            "model": bundle["meta"],
            **server_note(),
        }
    finally:
        close_client(client)
        release_bundle(bundle)


def run_validate(args: argparse.Namespace) -> dict:
    bundle = load_bundle(lora=True)
    try:
        started = time.perf_counter()
        detail = validate_logprob(bundle)
        detail["wall_s"] = time.perf_counter() - started
        detail["phase"] = "validate"
        detail["model"] = bundle["meta"]
        return detail
    finally:
        release_bundle(bundle)


def run_train(args: argparse.Namespace) -> dict:
    import torch

    bundle = load_bundle(lora=True)
    try:
        validate_logprob(bundle)
    finally:
        release_bundle(bundle)
    bundle = load_bundle(lora=True)
    client = None
    try:
        client = connect_checked()
        reseed(args.torch_seed)
        model = bundle["model"]
        tokenizer = bundle["tokenizer"]
        device = bundle["device"]
        params = [param for param in model.parameters() if param.requires_grad]
        opt = torch.optim.AdamW(params, lr=LR, weight_decay=0.0)
        # A 0.25 prior matches a 0/1 task reward. The answer bonus is at most
        # that prior, so the same prior makes every warmup advantage negative
        # and the update suppresses the few legal answer lines.
        baseline_init = 0.0 if args.answer_bonus > 0 else 0.25
        baseline = baseline_init
        curve = []
        window: list[dict] = []
        grad_norms: list[float] = []
        updates = 0
        skipped_zero_advantage = 0
        started = time.perf_counter()
        if torch.cuda.is_available():
            torch.cuda.reset_peak_memory_stats()
        curve.append(checkpoint(client, bundle, 0, window, args.limit))
        for episode_index in range(args.train_episodes):
            seed = seeds_for("train")[episode_index % TRAIN_MOD()]
            episode, turns = play_lm_episode(
                client, bundle, seed, f"train-{args.torch_seed}-{episode_index}", "sample"
            )
            window.append(episode)
            server_reward = float(episode["reward"])
            shaped = server_reward + args.answer_bonus * float(episode.get("answer_line_rate") or 0.0)
            episode["shaped_reward"] = shaped
            advantage = shaped - baseline
            baseline = (1.0 - BASELINE_BETA) * baseline + BASELINE_BETA * shaped
            if turns and advantage != 0.0:
                model.train()
                model.config.use_cache = False
                opt.zero_grad(set_to_none=True)
                turn_means = []
                for prompt_ids, completion_ids in turns:
                    logprobs = completion_logprobs(model, prompt_ids.to(device), completion_ids.to(device))
                    turn_means.append(logprobs.mean())
                loss = -advantage * torch.stack(turn_means).mean()
                loss.backward()
                grad_norm = torch.nn.utils.clip_grad_norm_(params, GRAD_CLIP)
                opt.step()
                grad_norms.append(float(grad_norm))
                updates += 1
            else:
                skipped_zero_advantage += 1
            done_count = episode_index + 1
            if done_count % args.checkpoint_every == 0 or done_count == args.train_episodes:
                curve.append(checkpoint(client, bundle, done_count, window, args.limit))
                window = []
            print(
                f"train seed={args.torch_seed} episode={done_count}/{args.train_episodes} "
                f"reward={episode['reward']} shaped={episode['shaped_reward']:.3f} "
                f"success={int(episode['success'])} "
                f"format={episode['format_rate']:.2f} answer_line={episode['answer_line_rate']:.2f} "
                f"baseline={baseline:.3f}",
                flush=True,
            )
        test_summary = None
        if os.environ.get("SENV_EVAL_TEST") == "1":
            if os.environ.get("SENV_ARITH_FROZEN") != "1":
                raise SystemExit("refusing test evaluation before the difficulty is frozen")
            reseed(args.torch_seed + 10_000)
            greedy = evaluate_lm(client, bundle, "test", "greedy", args.limit, f"trained-greedy-{args.torch_seed}")
            reseed(args.torch_seed + 20_000)
            sampled = evaluate_lm(client, bundle, "test", "sample", args.limit, f"trained-sample-{args.torch_seed}")
            test_summary = {
                "greedy": aggregate(greedy),
                "sample": aggregate(sampled),
                "greedy_episodes": slim(greedy),
                "sample_episodes": slim(sampled),
            }
        adapter_dir = args.adapter_dir or os.environ.get("SENV_ADAPTER_DIR", "")
        if adapter_dir:
            path = Path(adapter_dir)
            path.mkdir(parents=True, exist_ok=True)
            model.save_pretrained(path)
            tokenizer.save_pretrained(path)
        peak_alloc = None
        peak_reserved = None
        if torch.cuda.is_available():
            peak_alloc = torch.cuda.max_memory_allocated()
            peak_reserved = torch.cuda.max_memory_reserved()
        return {
            "phase": "train",
            "torch_seed": args.torch_seed,
            "train_episodes": args.train_episodes,
            "train_problem_seeds": [TRAIN_START_SEED(), TRAIN_START_SEED() + TRAIN_MOD() - 1],
            "lr": LR,
            "baseline_beta": BASELINE_BETA,
            "baseline_init": baseline_init,
            "answer_bonus": args.answer_bonus,
            "grad_clip": GRAD_CLIP,
            "lora_r": LORA_R,
            "temperature": TEMPERATURE,
            "top_p": TOP_P,
            "max_new_tokens": MAX_NEW_TOKENS,
            "updates": updates,
            "skipped_zero_advantage": skipped_zero_advantage,
            "mean_grad_norm_preclip": (sum(grad_norms) / len(grad_norms)) if grad_norms else None,
            "final_baseline": baseline,
            "curve": curve,
            "test": test_summary,
            "wall_s": time.perf_counter() - started,
            "peak_allocated_bytes": peak_alloc,
            "peak_reserved_bytes": peak_reserved,
            "adapter_dir": adapter_dir or None,
            "model": bundle["meta"],
            "objective": (
                "Monte Carlo REINFORCE. Each turn of an episode gets the terminal "
                "server reward minus an EMA baseline. The loss is the negative "
                "advantage times the mean completion-token logprob. Prompt tokens "
                "are not in that mean. Format validity is not part of the reward."
                if args.answer_bonus == 0
                else (
                    "Separate answer-format warmup. The training target is the "
                    "server reward plus answer_bonus times the fraction of actions "
                    "whose first tokens are 'answer' and an integer. Task success "
                    "stays the server reward. This is not the original objective."
                )
            ),
            **server_note(),
        }
    finally:
        if client is not None:
            close_client(client)
        release_bundle(bundle)


def TRAIN_MOD() -> int:
    from arith_task import TRAIN_COUNT

    return TRAIN_COUNT


def TRAIN_START_SEED() -> int:
    from arith_task import TRAIN_START

    return TRAIN_START


def checkpoint(client, bundle, episodes_done: int, window: list[dict], limit: int) -> dict:
    state = capture_rng()
    try:
        val_eps = evaluate_lm(client, bundle, "val", "greedy", limit, f"val-{episodes_done}")
    finally:
        restore_rng(state)
    point = {
        "episodes": episodes_done,
        "val": aggregate(val_eps),
        "train_window": aggregate(window) if window else None,
    }
    print(
        f"checkpoint episodes={episodes_done} val_success={point['val']['successes']}/{point['val']['episodes']} "
        f"val_format={point['val']['mean_format_rate']} "
        f"val_answer_line={point['val'].get('mean_answer_line_rate')}",
        flush=True,
    )
    return point


def evaluate_lm(client, bundle, split: str, decode: str, limit: int, name: str) -> list[dict]:
    episodes = []
    for seed in seeds_for(split, limit):
        episode, _turns = play_lm_episode(client, bundle, seed, f"{name}-{seed}", decode)
        episodes.append(episode)
    summary = aggregate(episodes)
    print(
        f"{name} split={split} decode={decode} success={summary['successes']}/{summary['episodes']} "
        f"reward={summary['mean_reward']} format={summary['mean_format_rate']}",
        flush=True,
    )
    return episodes


def play_lm_episode(client, bundle, seed: int, episode_id: str, decode: str):
    result = client.reset(seed=seed, episode_id=episode_id)
    ops = _ops(result)
    actions: list[str] = []
    valids: list[bool] = []
    turns = []
    reward = 0.0
    done = False
    for _ in range(8):
        if result.done:
            done = True
            break
        prompt_ids, completion_ids, line = generate_line(bundle, observation_text(result.observation), decode)
        actions.append(line)
        if completion_ids.numel() > 0:
            turns.append((prompt_ids, completion_ids))
        result = client.step(action_of(line))
        info = getattr(result.observation, "info", None) or {}
        valids.append(bool(info.get("arith.format_valid")))
        reward = _reward(result)
        done = bool(result.done)
    steps = len(actions)
    episode = {
        "seed": seed,
        "episode_id": episode_id,
        "actions": actions,
        "steps": steps,
        "reward": reward,
        "success": bool(done and reward >= 0.999),
        "format_rate": (sum(valids) / steps) if steps else 0.0,
        "format_valid_actions": int(sum(valids)),
        "answer_line_rate": (sum(1 for line in actions if leading_answer(line)) / steps) if steps else 0.0,
        "ops": ops,
    }
    return episode, turns


def leading_answer(line: str) -> bool:
    parts = line.strip().split()
    if len(parts) < 2 or parts[0].lower() != "answer":
        return False
    token = parts[1][1:] if parts[1].startswith("-") else parts[1]
    return token.isdigit() and 1 <= len(token) <= 12


def play_episode(client, seed: int, episode_id: str, choose) -> dict:
    result = client.reset(seed=seed, episode_id=episode_id)
    ops = _ops(result)
    actions = []
    valids = []
    reward = 0.0
    done = False
    for _ in range(8):
        if result.done:
            done = True
            break
        line = choose(observation_text(result.observation))
        actions.append(line)
        result = client.step(action_of(line))
        info = getattr(result.observation, "info", None) or {}
        valids.append(bool(info.get("arith.format_valid")))
        reward = _reward(result)
        done = bool(result.done)
    steps = len(actions)
    return {
        "seed": seed,
        "episode_id": episode_id,
        "actions": actions,
        "steps": steps,
        "reward": reward,
        "success": bool(done and reward >= 0.999),
        "format_rate": (sum(valids) / steps) if steps else 0.0,
        "format_valid_actions": int(sum(valids)),
        "ops": ops,
    }


def generate_line(bundle, observation: str, decode: str, system: str | None = None):
    import torch

    model = bundle["model"]
    tokenizer = bundle["tokenizer"]
    device = bundle["device"]
    messages = [
        {"role": "system", "content": SYSTEM if system is None else system},
        {"role": "user", "content": observation},
    ]
    prompt = tokenizer.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
    inputs = tokenizer(prompt, return_tensors="pt")
    inputs = {key: value.to(device) for key, value in inputs.items()}
    prompt_len = int(inputs["input_ids"].shape[1])
    stop_ids = bundle["stop_ids"]
    kwargs = {
        "max_new_tokens": MAX_NEW_TOKENS,
        "do_sample": decode == "sample",
        "pad_token_id": tokenizer.pad_token_id,
        "eos_token_id": stop_ids,
    }
    if decode == "sample":
        kwargs["temperature"] = TEMPERATURE
        kwargs["top_p"] = TOP_P
    was_training = model.training
    model.eval()
    model.config.use_cache = True
    with torch.no_grad():
        output = model.generate(**inputs, **kwargs)
    if was_training:
        model.train()
    model.config.use_cache = False
    new_tokens = output[0, prompt_len:]
    text = tokenizer.decode(new_tokens, skip_special_tokens=True)
    prompt_ids = inputs["input_ids"][0].detach().cpu()
    completion_ids = new_tokens.detach().cpu()
    return prompt_ids, completion_ids, action_line(text)


def completion_logprobs(model, prompt_ids, completion_ids):
    import torch

    if prompt_ids.numel() == 0 or completion_ids.numel() == 0:
        raise RuntimeError("logprob mask requires a prompt and a completion")
    input_ids = torch.cat([prompt_ids, completion_ids], dim=0).unsqueeze(0)
    attention = torch.ones_like(input_ids)
    logits = model(input_ids=input_ids, attention_mask=attention).logits[0]
    prompt_len = int(prompt_ids.shape[0])
    scored = []
    for offset in range(int(completion_ids.shape[0])):
        position = prompt_len + offset - 1
        if position < prompt_len - 1 or position < 0 or position >= logits.shape[0]:
            raise RuntimeError(f"scored position {position} is outside the completion mask")
        if position + 1 < prompt_len:
            raise RuntimeError("prompt token was included in the completion mask")
        target = input_ids[0, position + 1]
        if int(target.item()) != int(completion_ids[offset].item()):
            raise RuntimeError("completion target does not match the generated token")
        token_logprob = torch.log_softmax(logits[position].float(), dim=-1)[target]
        scored.append(token_logprob)
    if len(scored) != int(completion_ids.shape[0]):
        raise RuntimeError("mask length does not equal the completion length")
    return torch.stack(scored)


def validate_logprob(bundle) -> dict:
    import torch

    model = bundle["model"]
    trainable = [param for param in model.parameters() if param.requires_grad]
    if not trainable:
        raise SystemExit("LoRA added no trainable parameters")
    model.eval()
    prompt_ids, completion_ids, line = generate_line(
        bundle,
        "Reply with exactly one line: answer 1",
        "greedy",
    )
    if completion_ids.numel() == 0:
        raise SystemExit("validation generation produced no tokens")
    device = bundle["device"]
    first = completion_logprobs(model, prompt_ids.to(device), completion_ids.to(device))
    second = completion_logprobs(model, prompt_ids.to(device), completion_ids.to(device))
    max_gap = float((first - second).abs().max().item())
    if max_gap > 1e-3:
        raise SystemExit(f"repeated logprob forward disagreed by {max_gap}")
    if int(first.shape[0]) != int(completion_ids.shape[0]):
        raise SystemExit("mask length mismatch")
    before = float(first.sum().item())
    opt = torch.optim.AdamW(trainable, lr=VALIDATE_LR, weight_decay=0.0)
    increased = False
    after = before
    grad_norm = None
    for _ in range(5):
        model.train()
        model.config.use_cache = False
        opt.zero_grad(set_to_none=True)
        current = completion_logprobs(model, prompt_ids.to(device), completion_ids.to(device))
        loss = -current.mean()
        loss.backward()
        grad_norm = float(torch.nn.utils.clip_grad_norm_(trainable, GRAD_CLIP))
        if grad_norm == 0.0:
            raise SystemExit("LoRA gradient is zero; the completion mask is not training")
        opt.step()
        model.eval()
        with torch.no_grad():
            after = float(completion_logprobs(model, prompt_ids.to(device), completion_ids.to(device)).sum().item())
        if after > before + 1e-4:
            increased = True
            break
    if not increased:
        raise SystemExit(f"LoRA step did not increase completion logprob ({before} -> {after})")
    return {
        "ok": True,
        "completion_tokens": int(completion_ids.shape[0]),
        "prompt_tokens": int(prompt_ids.shape[0]),
        "mask_equals_completion": True,
        "prompt_tokens_excluded": True,
        "repeat_forward_max_abs_gap": max_gap,
        "logprob_sum_before": before,
        "logprob_sum_after": after,
        "grad_norm_preclip": grad_norm,
        "validate_lr": VALIDATE_LR,
        "sample_action_line": line,
        "newline_is_single_token": bundle["meta"]["newline_is_single_token"],
        "trainable_parameters": bundle["meta"]["trainable_parameters"],
    }


def load_bundle(lora: bool) -> dict:
    import torch
    from transformers import AutoModelForCausalLM, AutoTokenizer

    model_id = os.environ["SENV_MODEL_ID"]
    revision = os.environ.get("SENV_MODEL_REVISION") or None
    if not torch.cuda.is_available():
        raise SystemExit("CUDA is not available")
    if not torch.cuda.is_bf16_supported():
        raise SystemExit("this GPU does not support bf16")
    tokenizer = AutoTokenizer.from_pretrained(model_id, revision=revision, local_files_only=True)
    if tokenizer.pad_token_id is None:
        tokenizer.pad_token = tokenizer.eos_token
    model = AutoModelForCausalLM.from_pretrained(
        model_id,
        revision=revision,
        dtype=torch.bfloat16,
        local_files_only=True,
    )
    model.to("cuda")
    trainable = None
    if lora:
        from peft import LoraConfig, TaskType, get_peft_model

        model = get_peft_model(
            model,
            LoraConfig(
                r=LORA_R,
                lora_alpha=16,
                lora_dropout=0.0,
                bias="none",
                task_type=TaskType.CAUSAL_LM,
                target_modules=["q_proj", "k_proj", "v_proj", "o_proj"],
            ),
        )
        if hasattr(model, "enable_input_require_grads"):
            model.enable_input_require_grads()
        trainable = sum(param.numel() for param in model.parameters() if param.requires_grad)
        model.print_trainable_parameters()
    model.config.use_cache = False
    newline = tokenizer.encode("\n", add_special_tokens=False)
    stop_ids = []
    if len(newline) == 1:
        stop_ids.append(newline[0])
    if tokenizer.eos_token_id is not None:
        stop_ids.append(tokenizer.eos_token_id)
    parameters = sum(param.numel() for param in model.parameters())
    meta = model_meta(model_id, revision, parameters, trainable, len(newline) == 1)
    return {
        "model": model,
        "tokenizer": tokenizer,
        "device": torch.device("cuda"),
        "stop_ids": stop_ids,
        "meta": meta,
    }


def model_meta(model_id: str, revision: str | None, parameters: int, trainable: int | None, newline_one: bool) -> dict:
    import torch
    from huggingface_hub import model_info

    info = model_info(model_id, revision=revision)
    license_name = None
    card = getattr(info, "card_data", None)
    if card is not None:
        license_name = getattr(card, "license", None)
        if license_name is None and isinstance(card, dict):
            license_name = card.get("license")
    props = torch.cuda.get_device_properties(0)
    return {
        "model_id": model_id,
        "revision": info.sha,
        "requested_revision": revision,
        "license": license_name,
        "parameters": parameters,
        "trainable_parameters": trainable,
        "dtype": "bfloat16",
        "device_name": torch.cuda.get_device_name(0),
        "device_total_bytes": int(props.total_memory),
        "bf16_supported": bool(torch.cuda.is_bf16_supported()),
        "newline_is_single_token": newline_one,
        "torch": torch.__version__,
        "transformers": __import__("transformers").__version__,
        "peft": __import__("peft").__version__,
    }


def release_bundle(bundle) -> None:
    import torch

    bundle.clear()
    if torch.cuda.is_available():
        torch.cuda.empty_cache()


def capture_rng():
    import torch

    cuda = torch.cuda.get_rng_state_all() if torch.cuda.is_available() else None
    return random.getstate(), torch.get_rng_state(), cuda


def restore_rng(state) -> None:
    import torch

    random.setstate(state[0])
    torch.set_rng_state(state[1])
    if state[2] is not None:
        torch.cuda.set_rng_state_all(state[2])


def reseed(seed: int) -> None:
    import torch

    random.seed(seed)
    torch.manual_seed(seed)
    if torch.cuda.is_available():
        torch.cuda.manual_seed_all(seed)


def connect_checked():
    import textarena_env.client as textarena_client
    from textarena_env import TextArenaEnv

    client_file = os.path.realpath(textarena_client.__file__)
    expected = os.environ.get("OPENENV_CLIENT")
    if expected and os.path.realpath(expected) != client_file:
        raise SystemExit(f"client path {client_file} != {expected}")
    base_url = os.environ["SENV_BASE_URL"].rstrip("/")
    client = TextArenaEnv(base_url=base_url).sync()
    client.__enter__()
    return client


def close_client(client) -> None:
    closer = getattr(client, "__exit__", None)
    if closer is not None:
        closer(None, None, None)


def action_of(message: str):
    from textarena_env.models import TextArenaAction

    return TextArenaAction(message=message)


def observation_text(observation) -> str:
    parts = []
    prompt = getattr(observation, "prompt", "") or ""
    if prompt:
        parts.append(prompt)
    for message in getattr(observation, "messages", []) or []:
        content = getattr(message, "content", "") or ""
        if content:
            parts.append(content)
    return "\n".join(parts)


def _ops(result):
    metadata = getattr(result.observation, "metadata", None) or {}
    return metadata.get("ops")


def _reward(result) -> float:
    value = getattr(result, "reward", None)
    if value is None:
        return 0.0
    return float(value)


def server_note() -> dict:
    return {
        "base_url": os.environ.get("SENV_BASE_URL"),
        "client": "unmodified OpenEnv TextArenaEnv",
        "task": "custom Arith-v0; not an upstream environment",
        "answer_source": "server reward only; state() is not called by the policy",
        "candidate_filter": False,
    }


def slim(episodes: list[dict]) -> list[dict]:
    return [
        {
            "seed": episode["seed"],
            "reward": episode["reward"],
            "success": episode["success"],
            "steps": episode["steps"],
            "format_rate": episode["format_rate"],
            "actions": episode["actions"],
        }
        for episode in episodes
    ]


if __name__ == "__main__":
    raise SystemExit(main())
