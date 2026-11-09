#!/usr/bin/env python3
"""Language-model Wordle through the live senv TextArenaEnv socket.

Every guess sent to the server is a dictionary word the player has not already
used. The language model does not emit free text. It scores the bracketed
form of each candidate, `[word]`, and a softmax over those scores is the
policy. Candidates are the words consistent with feedback the observation
already showed. When more than CANDIDATES words remain, a seeded sample of
that size is scored. That cap is a compute limit.

A uniform draw over the same candidate list is a separate baseline. It is not
a language model. The earlier four-feature linear policy is not rerun here.

The decision path does not call state() and does not read the secret.
`--smoke` plays one episode and checks the batched score against the
single-word score used for the gradient.
"""

from __future__ import annotations

import json
import math
import os
import random
import sys
import time
from pathlib import Path

import wordle_experiment as we

MODEL_ID = "HuggingFaceTB/SmolLM2-135M-Instruct"
CANDIDATES = 32
TEMPERATURE = 1.0
HELD_OUT_START = 10_000
HELD_OUT_COUNT = 40
TRAIN_EPISODES = 32
LR = 5e-5
BASELINE_BETA = 0.1
GRAD_CLIP = 1.0

SYSTEM = (
    "You are playing Wordle. The next reply is one new five-letter guess "
    "in brackets. Use the feedback and do not repeat a guess."
)


def main() -> int:
    we.self_test()
    _filter_self_test()
    if "--self-test" in sys.argv:
        print("self-test ok")
        return 0

    base_url = os.environ["SENV_BASE_URL"].rstrip("/")
    out_dir = Path(os.environ["SENV_EXPERIMENT_OUT"])
    out_dir.mkdir(parents=True, exist_ok=True)
    words = we.load_words(Path(__file__).resolve().parents[1] / "assets" / "wordle" / "words.txt")
    smoke = "--smoke" in sys.argv
    held_out = [HELD_OUT_START] if smoke else list(range(HELD_OUT_START, HELD_OUT_START + HELD_OUT_COUNT))
    train_n = 1 if smoke else TRAIN_EPISODES
    report: dict = {
        "base_url": base_url,
        "model_id": MODEL_ID,
        "candidates": CANDIDATES,
        "temperature": TEMPERATURE,
        "held_out_seeds": held_out,
        "train_episodes": train_n,
        "lr": LR,
        "notes": [
            "Guesses are dictionary words sent through TextArenaEnv.",
            "The policy does not call state().",
            "uniform-candidates draws uniformly from the same capped consistent list the language model scores.",
            "uniform-candidates is not a language model.",
            "Language-model training is one-step REINFORCE on the shaped green/yellow signal.",
            "Held-out mean_reward is the client reward.",
            "The free-text SmolLM2 run in docs/EXPERIMENT.md is a separate earlier result.",
        ],
        "phases": {},
    }

    def save() -> None:
        (out_dir / "lm-results.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

    client = we.connect(base_url)
    try:
        started = time.perf_counter()
        report["phases"]["random"] = run_policy(
            client, words, held_out, "random", lambda text, episode_id, step, rng: we.random_action(words, rng)
        )
        report["phases"]["random"]["wall_s"] = time.perf_counter() - started
        save()
        print(we.summarize("random", report["phases"]["random"]), flush=True)

        started = time.perf_counter()
        report["phases"]["uniform-candidates"] = run_policy(
            client,
            words,
            held_out,
            "uniform-candidates",
            lambda text, episode_id, step, rng: uniform_candidate_action(words, text, episode_id, step, rng),
        )
        report["phases"]["uniform-candidates"]["wall_s"] = time.perf_counter() - started
        save()
        print(we.summarize("uniform-candidates", report["phases"]["uniform-candidates"]), flush=True)

        bundle = load_model()
        report["model"] = {
            key: bundle[key]
            for key in ("revision", "parameters", "dtype", "device_name", "cuda", "torch", "transformers")
        }
        save()
        if smoke:
            check_score_agreement(bundle, words)

        started = time.perf_counter()
        report["phases"]["lm-pretrained-sample"] = run_lm_policy(
            client, words, held_out, "lm-pretrained-sample", bundle, greedy=False
        )
        report["phases"]["lm-pretrained-sample"]["wall_s"] = time.perf_counter() - started
        save()
        print(we.summarize("lm-pretrained-sample", report["phases"]["lm-pretrained-sample"]), flush=True)

        if not smoke:
            started = time.perf_counter()
            report["phases"]["lm-pretrained-greedy"] = run_lm_policy(
                client, words, held_out, "lm-pretrained-greedy", bundle, greedy=True
            )
            report["phases"]["lm-pretrained-greedy"]["wall_s"] = time.perf_counter() - started
            save()
            print(we.summarize("lm-pretrained-greedy", report["phases"]["lm-pretrained-greedy"]), flush=True)

        started = time.perf_counter()
        train = train_lm(client, words, bundle, train_n)
        train["wall_s"] = time.perf_counter() - started
        report["lm_train"] = train
        save()
        print(
            f"lm train first8 {train['first_mean_reward']:.4f} last8 {train['last_mean_reward']:.4f} "
            f"wins {train['wins']}",
            flush=True,
        )

        started = time.perf_counter()
        report["phases"]["lm-trained-sample"] = run_lm_policy(
            client, words, held_out, "lm-trained-sample", bundle, greedy=False
        )
        report["phases"]["lm-trained-sample"]["wall_s"] = time.perf_counter() - started
        save()
        print(we.summarize("lm-trained-sample", report["phases"]["lm-trained-sample"]), flush=True)

        if not smoke:
            started = time.perf_counter()
            report["phases"]["lm-trained-greedy"] = run_lm_policy(
                client, words, held_out, "lm-trained-greedy", bundle, greedy=True
            )
            report["phases"]["lm-trained-greedy"]["wall_s"] = time.perf_counter() - started
            save()
            print(we.summarize("lm-trained-greedy", report["phases"]["lm-trained-greedy"]), flush=True)
    finally:
        we.close_client(client)
        save()
    if bundle_cuda(report):
        report["peak_memory_allocated_bytes"] = int(torch_cuda_peak())
        report["peak_memory_reserved_bytes"] = int(torch_cuda_reserved())
        save()
    return 0


def run_policy(client, words, seeds, name, choose) -> dict:
    episodes = []
    for seed in seeds:
        rng = random.Random(f"{name}-{seed}")
        episode_id = f"{name}-{seed}"
        result = client.reset(seed=seed, episode_id=episode_id)
        actions = []
        pools = []
        reward = 0.0
        done = False
        while len(actions) < 8 and not result.done:
            text = we.observation_text(result.observation)
            pool = candidate_pool(words, we.parse_history(text), seed, len(actions))
            pools.append(len(pool))
            action = choose(text, seed, len(actions), rng)
            actions.append(action)
            result = client.step(we._action(action))
            reward = we._reward(result)
            done = bool(result.done)
        episodes.append(_episode(seed, actions, pools, reward, done))
    return _aggregate(episodes)


def run_lm_policy(client, words, seeds, name, bundle, greedy: bool) -> dict:
    episodes = []
    for seed in seeds:
        rng = random.Random(f"{name}-{seed}")
        episode_id = f"{name}-{seed}"
        result = client.reset(seed=seed, episode_id=episode_id)
        actions = []
        pools = []
        reward = 0.0
        done = False
        while len(actions) < 8 and not result.done:
            text = we.observation_text(result.observation)
            pool = candidate_pool(words, we.parse_history(text), seed, len(actions))
            pools.append(len(pool))
            action = lm_action(bundle, text, pool, rng, greedy)
            actions.append(action)
            result = client.step(we._action(action))
            reward = we._reward(result)
            done = bool(result.done)
        episodes.append(_episode(seed, actions, pools, reward, done))
        print(f"{name} seed {seed} reward {reward:.3f} steps {len(actions)} actions {actions}", flush=True)
    return _aggregate(episodes)


def train_lm(client, words, bundle, episodes_n: int) -> dict:
    import torch

    model = bundle["model"]
    optimizer = torch.optim.AdamW(model.parameters(), lr=LR)
    baseline = 0.0
    official = []
    shaped_totals = []
    grad_norms = []
    for episode in range(episodes_n):
        seed = episode
        episode_id = f"lm-train-{seed}"
        result = client.reset(seed=seed, episode_id=episode_id)
        actions = []
        reward = 0.0
        shaped_total = 0.0
        norms = []
        while len(actions) < 8 and not result.done:
            text = we.observation_text(result.observation)
            history = we.parse_history(text)
            pool = candidate_pool(words, history, seed, len(actions))
            rng = random.Random(f"lm-train-act-{seed}-{len(actions)}")
            scores = exact_scores(bundle, text, pool)
            index = _categorical(rng, scores, TEMPERATURE)
            action = f"[{pool[index]}]"
            result = client.step(we._action(action))
            reward = we._reward(result)
            shaped = we.shaped_step_reward(result)
            shaped_total += shaped
            advantage = shaped - baseline
            norm = reinforce_step(bundle, optimizer, text, pool, index, advantage, scores)
            norms.append(norm)
            baseline = (1.0 - BASELINE_BETA) * baseline + BASELINE_BETA * shaped
            actions.append(action)
        official.append(reward)
        shaped_totals.append(shaped_total)
        grad_norms.append(sum(norms) / len(norms) if norms else 0.0)
        print(
            f"lm train {episode + 1} reward {reward:.3f} shaped {shaped_total:.3f} "
            f"steps {len(actions)} grad {grad_norms[-1]:.4f} actions {actions}",
            flush=True,
        )
    first = official[: min(8, len(official))]
    last = official[-min(8, len(official)) :]
    return {
        "episodes": episodes_n,
        "seeds": list(range(episodes_n)),
        "lr": LR,
        "optimizer": "AdamW",
        "grad_clip": GRAD_CLIP,
        "baseline_beta": BASELINE_BETA,
        "temperature": TEMPERATURE,
        "candidates": CANDIDATES,
        "algorithm": (
            "one-step REINFORCE on softmax over language-model logprobs of [word]; "
            "shaped reward is wordle.greens + 0.5*wordle.yellows when present, "
            "otherwise the client step reward"
        ),
        "first_mean_reward": sum(first) / len(first),
        "last_mean_reward": sum(last) / len(last),
        "first_wins": sum(1 for value in first if value >= 0.999),
        "last_wins": sum(1 for value in last if value >= 0.999),
        "mean_reward": sum(official) / len(official),
        "wins": sum(1 for value in official if value >= 0.999),
        "mean_shaped": sum(shaped_totals) / len(shaped_totals),
        "mean_grad_norm": sum(grad_norms) / len(grad_norms),
    }


def reinforce_step(
    bundle, optimizer, text: str, pool: list[str], chosen: int, advantage: float, scores: list[float]
) -> float:
    import torch

    if not pool:
        return 0.0
    pi = _probs(scores, TEMPERATURE)
    optimizer.zero_grad(set_to_none=True)
    prompt = prompt_ids(bundle["tokenizer"], text, bundle["device"])
    for index, word in enumerate(pool):
        coef = -advantage * ((1.0 if index == chosen else 0.0) - pi[index]) / TEMPERATURE
        if abs(coef) < 1e-8:
            continue
        score = score_one(bundle["model"], bundle["tokenizer"], prompt, word, bundle["device"])
        (coef * score).backward()
    norm = float(torch.nn.utils.clip_grad_norm_(bundle["model"].parameters(), GRAD_CLIP))
    optimizer.step()
    return norm


def lm_action(bundle, text: str, pool: list[str], rng: random.Random, greedy: bool) -> str:
    if not pool:
        return "[?????]"
    scores = score_words(bundle, text, pool).tolist()
    if greedy:
        index = max(range(len(scores)), key=lambda i: (scores[i], -i))
    else:
        index = _categorical(rng, scores, TEMPERATURE)
    return f"[{pool[index]}]"


def uniform_candidate_action(words, text: str, seed: int, step: int, rng: random.Random) -> str:
    pool = candidate_pool(words, we.parse_history(text), seed, step)
    if not pool:
        return "[?????]"
    return f"[{rng.choice(pool)}]"


def score_words(bundle, text: str, pool: list[str]):
    import torch

    prompt = prompt_ids(bundle["tokenizer"], text, bundle["device"])
    pieces = [completion_ids(bundle["tokenizer"], word) for word in pool]
    width = max(len(piece) for piece in pieces)
    prompt_len = prompt.shape[1]
    batch = len(pool)
    full = torch.full(
        (batch, prompt_len + width),
        bundle["pad_id"],
        dtype=torch.long,
        device=bundle["device"],
    )
    mask = torch.zeros_like(full)
    for row, piece in enumerate(pieces):
        full[row, :prompt_len] = prompt[0]
        full[row, prompt_len : prompt_len + len(piece)] = torch.tensor(piece, device=bundle["device"])
        mask[row, : prompt_len + len(piece)] = 1
    was_training = bundle["model"].training
    bundle["model"].eval()
    with torch.no_grad():
        logits = bundle["model"](full, attention_mask=mask).logits
    if was_training:
        bundle["model"].train()
    logp = torch.log_softmax(logits[:, :-1, :].float(), dim=-1)
    totals = []
    for row, piece in enumerate(pieces):
        target = torch.tensor(piece, device=bundle["device"]).view(-1, 1)
        start = prompt_len - 1
        chosen = logp[row, start : start + len(piece), :].gather(1, target).squeeze(1)
        totals.append(chosen.sum())
    return torch.stack(totals)


def exact_scores(bundle, text: str, pool: list[str]) -> list[float]:
    """Unpadded per-word scores. The training policy samples from these."""
    import torch

    prompt = prompt_ids(bundle["tokenizer"], text, bundle["device"])
    values = []
    with torch.no_grad():
        for word in pool:
            score = score_one(bundle["model"], bundle["tokenizer"], prompt, word, bundle["device"])
            values.append(float(score))
    return values


def score_one(model, tokenizer, prompt, word: str, device):
    import torch

    piece = torch.tensor(completion_ids(tokenizer, word), dtype=torch.long, device=device).view(1, -1)
    full = torch.cat([prompt, piece], dim=1)
    logits = model(full).logits
    start = prompt.shape[1] - 1
    logp = torch.log_softmax(logits[:, start:-1, :].float(), dim=-1)
    chosen = logp.gather(2, piece.unsqueeze(-1)).squeeze(-1)
    return chosen.sum()


def prompt_ids(tokenizer, text: str, device):
    encoded = tokenizer.apply_chat_template(
        [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": text},
        ],
        add_generation_prompt=True,
        return_tensors="pt",
    )
    ids = encoded["input_ids"] if hasattr(encoded, "keys") else encoded
    return ids.to(device)


def completion_ids(tokenizer, word: str) -> list[int]:
    ids = tokenizer(f"[{word}]", add_special_tokens=False)["input_ids"]
    if not ids:
        raise RuntimeError(f"empty tokenization for {word}")
    return list(ids)


def candidate_pool(words: list[str], history: list[tuple[str, str]], seed: int, step: int) -> list[str]:
    # Keyed by the env seed and the guesses already played, so two policies
    # that share a history on the same seed score the same shortlist.
    rng = random.Random(f"cand-{seed}-{step}-" + ",".join(guess for guess, _marks in history))
    guessed = {guess for guess, _marks in history}
    pool = []
    for word in words:
        if word in guessed:
            continue
        if all(we.mark(word, guess) == marks for guess, marks in history):
            pool.append(word)
    if not pool:
        pool = [word for word in words if word not in guessed]
    if len(pool) > CANDIDATES:
        pool = rng.sample(pool, CANDIDATES)
    pool.sort()
    return pool


def load_model() -> dict:
    import torch
    import transformers

    cuda = torch.cuda.is_available()
    device = torch.device("cuda" if cuda else "cpu")
    if cuda:
        torch.backends.cuda.matmul.allow_tf32 = True
        torch.cuda.reset_peak_memory_stats()
    dtype = torch.float32
    tokenizer = transformers.AutoTokenizer.from_pretrained(MODEL_ID)
    try:
        model = transformers.AutoModelForCausalLM.from_pretrained(
            MODEL_ID, dtype=dtype, low_cpu_mem_usage=True
        )
    except TypeError:
        model = transformers.AutoModelForCausalLM.from_pretrained(
            MODEL_ID, torch_dtype=dtype, low_cpu_mem_usage=True
        )
    model.to(device)
    model.eval()
    if tokenizer.pad_token_id is None:
        tokenizer.pad_token = tokenizer.eos_token
    revision = getattr(model.config, "_commit_hash", None) or cache_revision(MODEL_ID)
    info = {
        "model": model,
        "tokenizer": tokenizer,
        "device": device,
        "pad_id": int(tokenizer.pad_token_id),
        "revision": revision,
        "parameters": int(sum(parameter.numel() for parameter in model.parameters())),
        "dtype": "float32",
        "device_name": torch.cuda.get_device_name(0) if cuda else "cpu",
        "cuda": cuda,
        "torch": torch.__version__,
        "transformers": transformers.__version__,
    }
    print(
        f"loaded {MODEL_ID} revision={revision} params={info['parameters']} "
        f"device={info['device_name']}",
        flush=True,
    )
    return info


def cache_revision(model_id: str) -> str | None:
    folder = "models--" + model_id.replace("/", "--")
    roots = []
    if os.environ.get("HUGGINGFACE_HUB_CACHE"):
        roots.append(Path(os.environ["HUGGINGFACE_HUB_CACHE"]))
    if os.environ.get("HF_HOME"):
        roots.append(Path(os.environ["HF_HOME"]) / "hub")
    for root in roots:
        ref = root / folder / "refs" / "main"
        if ref.is_file():
            return ref.read_text(encoding="utf-8").strip()
    return None


def check_score_agreement(bundle, words: list[str]) -> None:
    text = "You are Playing Wordle."
    pool = words[:4]
    batched = score_words(bundle, text, pool)
    prompt = prompt_ids(bundle["tokenizer"], text, bundle["device"])
    for index, word in enumerate(pool):
        single = score_one(bundle["model"], bundle["tokenizer"], prompt, word, bundle["device"])
        gap = abs(float(batched[index].detach()) - float(single.detach()))
        if gap > 1e-2:
            raise SystemExit(f"score mismatch {word} batch {float(batched[index])} single {float(single)}")
    print("score agreement ok", flush=True)


def _filter_self_test() -> None:
    history = [("cigar", "G X X Y Y")]
    pool = candidate_pool(["crane", "cigar", "apple", "stone"], history, 0, 1)
    if pool != ["crane"]:
        raise SystemExit(f"candidate filter {pool}")


def _episode(seed: int, actions: list[str], pools: list[int], reward: float, done: bool) -> dict:
    return {
        "seed": seed,
        "actions": actions,
        "candidate_counts": pools,
        "steps": len(actions),
        "reward": reward,
        "success": bool(done and reward >= 0.999),
    }


def _aggregate(episodes: list[dict]) -> dict:
    rewards = [episode["reward"] for episode in episodes]
    wins = sum(1 for episode in episodes if episode["success"])
    return {
        "episodes": len(episodes),
        "mean_reward": (sum(rewards) / len(rewards)) if rewards else None,
        "successes": wins,
        "success_rate": (wins / len(episodes)) if episodes else None,
        "mean_steps": (sum(episode["steps"] for episode in episodes) / len(episodes)) if episodes else None,
        "records": episodes,
    }


def _probs(scores: list[float], temperature: float) -> list[float]:
    scaled = [score / temperature for score in scores]
    peak = max(scaled)
    weights = [math.exp(score - peak) for score in scaled]
    total = sum(weights)
    return [weight / total for weight in weights]


def _categorical(rng: random.Random, scores: list[float], temperature: float) -> int:
    weights = _probs(scores, temperature)
    pick = rng.random()
    acc = 0.0
    for index, weight in enumerate(weights):
        acc += weight
        if acc >= pick:
            return index
    return len(weights) - 1


def bundle_cuda(report: dict) -> bool:
    return bool(report.get("model", {}).get("cuda"))


def torch_cuda_peak() -> int:
    import torch

    return int(torch.cuda.max_memory_allocated())


def torch_cuda_reserved() -> int:
    import torch

    return int(torch.cuda.max_memory_reserved())


if __name__ == "__main__":
    raise SystemExit(main())
