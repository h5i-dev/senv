#!/usr/bin/env python3
"""GRPO on custom Arith-v0 through the live senv server.

The agent plays multi-turn episodes. Each turn it writes one line, either
`calc <int> <op> <int>` (the server replies with the result) or
`answer <int>` (terminal, reward 1 if correct). Generation stops at the first
newline, so one turn is exactly one action and the trained tokens are exactly
the tokens that were sent. The reward is the server reward only.

Per step, `--problems` training seeds are each played `--group` times in
parallel sessions. The advantage of an episode is its reward minus the mean
reward of its group (no std scaling). Every completion token of every turn in
the episode gets that advantage. One on-policy gradient step per batch, so the
importance ratio is 1 and the loss is plain policy gradient with a token-level
mean. Sampling is temperature 1, no top-p, no top-k, no repetition penalty, so
the trained log-probabilities are the plain softmax the sampler used.

Held-out problems: validation seeds 2000..2039 and test seeds 10000..10199 from
`arith_task`. Training seeds come from 0..1999, minus any seed whose expression
string equals a held-out expression (there are only 6561 distinct 2-operator
problems). Test is scored only before the first update and after the last one.
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

from arith_task import aggregate, expression_from_prompt, seeds_for  # noqa: E402
from arith_experiment import (  # noqa: E402
    SYSTEM,
    action_of,
    close_client,
    connect_checked,
    observation_text,
)

MAX_TURNS = 8  # src/arith.rs MAX_STEPS
# Worked trace uses two-digit operands, which the 1..9 digit generator never draws.
TOOL_EXAMPLE = (
    SYSTEM
    + "\n\nExample. Expression: ((12 * 11) - 10)\n"
    "You: calc 12 * 11\nTool: Result: 132\n"
    "You: calc 132 - 10\nTool: Result: 122\n"
    "You: answer 122\n"
    "Work from the innermost parentheses outward, one calc per reply, then answer."
)
TRAIN_POOL = range(0, 2000)


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--model", default="Qwen/Qwen2.5-0.5B-Instruct")
    p.add_argument("--revision", default="7ae557604adf67be50417f59c2c2f167def9a775")
    p.add_argument("--steps", type=int, default=60)
    p.add_argument("--problems", type=int, default=8)
    p.add_argument("--group", type=int, default=8)
    p.add_argument("--lr", type=float, default=5e-5)
    p.add_argument("--lora-r", type=int, default=16)
    p.add_argument("--max-new-tokens", type=int, default=32)
    p.add_argument("--micro-batch", type=int, default=16)
    p.add_argument("--eval-every", type=int, default=10)
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--eval-only", action="store_true")
    p.add_argument("--no-test", action="store_true")
    p.add_argument(
        "--system",
        default="default",
        choices=("default", "tool-example"),
        help="default is arith_experiment.SYSTEM; tool-example adds one worked calc trace",
    )
    p.add_argument(
        "--invalid-penalty",
        type=float,
        default=0.0,
        help="Training only: subtract this per format-invalid action. Eval success is the server reward.",
    )
    p.add_argument(
        "--render",
        default="flat",
        choices=("flat", "chat"),
        help="flat: whole transcript in one user message (old harness). "
        "chat: prompt as user, each PLAYER_ACTION as assistant, each GAME_MESSAGE as user.",
    )
    p.add_argument("--out", required=True)
    p.add_argument("--adapter-dir", default="")
    args = p.parse_args()

    import torch

    random.seed(args.seed)
    torch.manual_seed(args.seed)
    torch.cuda.manual_seed_all(args.seed)

    n_sessions = max(args.problems * args.group, 40)
    clients = [connect_checked() for _ in range(n_sessions)]
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    try:
        policy = Policy(args)
        held_out = held_out_expressions(clients[0])
        pool = [s for s in TRAIN_POOL if train_expression(clients[0], s) not in held_out]
        print(f"train pool {len(pool)} seeds after removing held-out expressions", flush=True)

        report: dict = {
            "config": vars(args),
            "train_pool_size": len(pool),
            "model": policy.meta,
            "curve": [],
            "train_log": [],
        }
        started = time.perf_counter()
        if not args.no_test:
            report["test_before"] = evaluate(policy, clients, "test", "greedy")
        report["curve"].append({"step": 0, "val": evaluate(policy, clients, "val", "greedy")})
        if args.eval_only:
            report["val_sample_before"] = evaluate(policy, clients, "val", "sample")
            write(out, report)
            return 0

        rng = random.Random(args.seed)
        torch.cuda.reset_peak_memory_stats()
        for step in range(1, args.steps + 1):
            t0 = time.perf_counter()
            seeds = rng.sample(pool, args.problems)
            batch_seeds = [s for s in seeds for _ in range(args.group)]
            episodes = play_batch(policy, clients, batch_seeds, "sample", f"train-{step}")
            rewards = [
                e["reward"] - args.invalid_penalty * (len(e["valid"]) - sum(e["valid"])) for e in episodes
            ]
            advantages = []
            mixed_groups = 0
            for g in range(args.problems):
                chunk = rewards[g * args.group : (g + 1) * args.group]
                mean = sum(chunk) / len(chunk)
                if max(chunk) != min(chunk):
                    mixed_groups += 1
                advantages.extend(r - mean for r in chunk)
            stats = policy.update(episodes, advantages)
            row = {
                "step": step,
                "mean_reward": sum(e["reward"] for e in episodes) / len(episodes),
                "mean_shaped": sum(rewards) / len(rewards),
                "mixed_groups": mixed_groups,
                "mean_turns": sum(e["steps"] for e in episodes) / len(episodes),
                "format_rate": sum(e["format_rate"] for e in episodes) / len(episodes),
                "wall_s": time.perf_counter() - t0,
                **stats,
            }
            report["train_log"].append(row)
            print(json.dumps(row), flush=True)
            if step % args.eval_every == 0 or step == args.steps:
                report["curve"].append({"step": step, "val": evaluate(policy, clients, "val", "greedy")})
                write(out, report)
        report["train_wall_s"] = time.perf_counter() - started
        report["peak_allocated_bytes"] = torch.cuda.max_memory_allocated()
        if not args.no_test:
            report["test_after"] = evaluate(policy, clients, "test", "greedy")
            report["test_after_sample"] = evaluate(policy, clients, "test", "sample")
        if args.adapter_dir:
            policy.model.save_pretrained(args.adapter_dir)
        write(out, report)
        return 0
    finally:
        for c in clients:
            close_client(c)


def write(out: Path, report: dict) -> None:
    out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def held_out_expressions(client) -> set[str]:
    found = set()
    for split in ("val", "test"):
        for seed in seeds_for(split):
            found.add(train_expression(client, seed))
    return found


def train_expression(client, seed: int) -> str:
    result = client.reset(seed=seed, episode_id=f"expr-{seed}")
    return expression_from_prompt(observation_text(result.observation))


def evaluate(policy, clients, split: str, decode: str) -> dict:
    seeds = seeds_for(split)
    episodes = []
    for start in range(0, len(seeds), len(clients)):
        chunk = seeds[start : start + len(clients)]
        episodes.extend(play_batch(policy, clients, chunk, decode, f"{split}-{decode}"))
    summary = aggregate(episodes)
    summary["calc_rate"] = sum(
        sum(1 for a in e["actions"] if a.split()[:1] == ["calc"]) for e in episodes
    ) / max(1, sum(e["steps"] for e in episodes))
    print(
        f"eval {split} {decode}: success {summary['successes']}/{summary['episodes']} "
        f"format {summary['mean_format_rate']:.3f} steps {summary['mean_steps']:.2f} "
        f"calc_rate {summary['calc_rate']:.2f}",
        flush=True,
    )
    summary["examples"] = [
        {"seed": e["seed"], "expression": e["expression"], "actions": e["actions"], "reward": e["reward"]}
        for e in episodes[:6]
    ]
    return summary


def play_batch(policy, clients, seeds: list[int], decode: str, tag: str) -> list[dict]:
    """Play one episode per seed, each in its own session, batching generation by turn."""

    n = len(seeds)
    results = [clients[i].reset(seed=s, episode_id=f"{tag}-{i}-{s}") for i, s in enumerate(seeds)]
    episodes = [
        {
            "seed": s,
            "expression": expression_from_prompt(observation_text(results[i].observation)),
            "actions": [],
            "valid": [],
            "turns": [],
            "reward": 0.0,
            "done": False,
        }
        for i, s in enumerate(seeds)
    ]
    for _ in range(MAX_TURNS):
        active = [i for i in range(n) if not episodes[i]["done"] and not results[i].done]
        if not active:
            break
        observations = [results[i].observation for i in active]
        generated = policy.generate(observations, decode)
        for i, (prompt_ids, completion_ids, line) in zip(active, generated):
            ep = episodes[i]
            ep["actions"].append(line)
            ep["turns"].append((prompt_ids, completion_ids))
            results[i] = clients[i].step(action_of(line))
            info = getattr(results[i].observation, "info", None) or {}
            ep["valid"].append(bool(info.get("arith.format_valid")))
            ep["reward"] = float(results[i].reward or 0.0)
            ep["done"] = bool(results[i].done)
    for ep in episodes:
        steps = len(ep["actions"])
        ep["steps"] = steps
        ep["success"] = bool(ep["done"] and ep["reward"] >= 0.999)
        ep["format_rate"] = sum(ep["valid"]) / steps if steps else 0.0
    return episodes


class Policy:
    def __init__(self, args) -> None:
        import torch
        from peft import LoraConfig, TaskType, get_peft_model
        from transformers import AutoModelForCausalLM, AutoTokenizer

        self.args = args
        self.device = torch.device("cuda")
        tok = AutoTokenizer.from_pretrained(args.model, revision=args.revision, local_files_only=True)
        tok.padding_side = "left"
        if tok.pad_token_id is None:
            tok.pad_token = tok.eos_token
        self.tok = tok
        model = AutoModelForCausalLM.from_pretrained(
            args.model, revision=args.revision, dtype=torch.bfloat16, local_files_only=True
        ).to(self.device)
        gc = model.generation_config
        gc.temperature, gc.top_p, gc.top_k, gc.repetition_penalty = 1.0, 1.0, 0, 1.0
        model = get_peft_model(
            model,
            LoraConfig(
                r=args.lora_r,
                lora_alpha=2 * args.lora_r,
                lora_dropout=0.0,
                bias="none",
                task_type=TaskType.CAUSAL_LM,
                target_modules=["q_proj", "k_proj", "v_proj", "o_proj", "gate_proj", "up_proj", "down_proj"],
            ),
        )
        self.model = model
        self.params = [q for q in model.parameters() if q.requires_grad]
        self.opt = torch.optim.AdamW(self.params, lr=args.lr, weight_decay=0.0)
        self.stop_ids = sorted({i for i in (tok.eos_token_id, tok.pad_token_id) if i is not None})
        # Token ids whose text contains a newline end the action line.
        self.newline_ids = {
            i for i in range(len(tok)) if "\n" in tok.decode([i], skip_special_tokens=False)
        }
        self.meta = {
            "model_id": args.model,
            "revision": args.revision,
            "parameters": sum(q.numel() for q in model.parameters()),
            "trainable_parameters": sum(q.numel() for q in self.params),
            "device": torch.cuda.get_device_name(0),
            "torch": torch.__version__,
            "transformers": __import__("transformers").__version__,
            "peft": __import__("peft").__version__,
        }

    def prompt_ids(self, observation) -> list[int]:
        system = TOOL_EXAMPLE if self.args.system == "tool-example" else SYSTEM
        messages = [{"role": "system", "content": system}]
        if self.args.render == "chat":
            messages.append({"role": "user", "content": observation.prompt})
            for m in observation.messages or []:
                role = "assistant" if m.category == "PLAYER_ACTION" else "user"
                messages.append({"role": role, "content": m.content})
        else:
            messages.append({"role": "user", "content": observation_text(observation)})
        text = self.tok.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
        return self.tok(text, add_special_tokens=False)["input_ids"]

    def generate(self, observations: list, decode: str):
        import torch

        prompts = [self.prompt_ids(o) for o in observations]
        width = max(len(q) for q in prompts)
        pad = self.tok.pad_token_id
        input_ids = torch.tensor([[pad] * (width - len(q)) + q for q in prompts], device=self.device)
        attention = torch.tensor([[0] * (width - len(q)) + [1] * len(q) for q in prompts], device=self.device)
        kwargs = {
            "max_new_tokens": self.args.max_new_tokens,
            "do_sample": decode == "sample",
            "pad_token_id": pad,
            "eos_token_id": self.stop_ids,
            "top_k": 0,
            "top_p": 1.0,
            "temperature": 1.0,
            "repetition_penalty": 1.0,
        }
        self.model.eval()
        with torch.no_grad():
            out = self.model.generate(input_ids=input_ids, attention_mask=attention, use_cache=True, **kwargs)
        rows = []
        for q, seq in zip(prompts, out[:, width:].tolist()):
            kept: list[int] = []
            for t in seq:
                if t in self.stop_ids:
                    kept.append(t)
                    break
                kept.append(t)
                if t in self.newline_ids:
                    break
            text = self.tok.decode(kept, skip_special_tokens=True)
            line = text.split("\n", 1)[0].strip()
            rows.append((q, kept, line))
        return rows

    def update(self, episodes: list[dict], advantages: list[float]) -> dict:
        import torch

        items = []
        for ep, adv in zip(episodes, advantages):
            if adv == 0.0:
                continue
            for prompt_ids, completion_ids in ep["turns"]:
                if completion_ids:
                    items.append((prompt_ids, completion_ids, adv))
        if not items:
            return {"loss": 0.0, "grad_norm": 0.0, "trained_seqs": 0}
        total_tokens = sum(len(c) for _, c, _ in items)
        self.model.train()
        self.opt.zero_grad(set_to_none=True)
        loss_sum = 0.0
        mb = self.args.micro_batch
        for start in range(0, len(items), mb):
            chunk = items[start : start + mb]
            # Left padding puts every completion at the right edge, so only the
            # last `keep` positions need logits (the full-vocab logits of the
            # whole prompt do not fit at this batch size).
            width = max(len(q) + len(c) for q, c, _ in chunk)
            span = max(len(c) for _, c, _ in chunk)
            keep = span + 1
            pad = self.tok.pad_token_id
            ids = torch.full((len(chunk), width), pad, device=self.device)
            attn = torch.zeros((len(chunk), width), device=self.device, dtype=torch.long)
            weight = torch.zeros((len(chunk), span), device=self.device)
            for row, (q, c, adv) in enumerate(chunk):
                seq = q + c
                ids[row, width - len(seq) :] = torch.tensor(seq, device=self.device)
                attn[row, width - len(seq) :] = 1
                weight[row, span - len(c) :] = adv
            logits = self.model(input_ids=ids, attention_mask=attn, logits_to_keep=keep).logits
            # logits[:, j] sits at position width-keep+j and predicts the next token.
            logits = logits[:, :-1].float()
            targets = ids[:, width - span :]
            logp = torch.log_softmax(logits, dim=-1).gather(-1, targets.unsqueeze(-1)).squeeze(-1)
            loss = -(logp * weight).sum() / total_tokens
            loss.backward()
            loss_sum += float(loss.detach())
        grad_norm = float(torch.nn.utils.clip_grad_norm_(self.params, 1.0))
        self.opt.step()
        return {"loss": loss_sum, "grad_norm": grad_norm, "trained_seqs": len(items)}


if __name__ == "__main__":
    raise SystemExit(main())
