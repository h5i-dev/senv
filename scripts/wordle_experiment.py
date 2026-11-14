#!/usr/bin/env python3
"""Wordle baselines and a short training run against a live senv server.

The agent loop uses the unmodified OpenEnv TextArenaEnv client. Nothing in the
decision path reads state().raw_state or the secret. The harness scores only
the reward and done flag returned by step().

Phases, in order:
  random            uniform guesses, ignores feedback
  linear-policy     four-feature linear softmax, REINFORCE on the env reward
  lm-pretrained     SmolLM2-135M-Instruct, inference only
  lm-reinforce      the same weights after a short REINFORCE update

Held-out seeds are 10000..10039. Training seeds start at 0 and do not overlap
the held-out range. A small learned policy is not a language model; the report
keeps those rows separate.
"""

from __future__ import annotations

import json
import math
import os
import random
import re
import sys
import time
from pathlib import Path

WORD_LEN = 5
HELD_OUT_START = 10_000
HELD_OUT_COUNT = 40
POLICY_TRAIN_EPISODES = 240
POLICY_LR = 0.4
POLICY_BASELINE_BETA = 0.1
LM_TRAIN_EPISODES = 32
LM_LR = 1e-5
LM_BASELINE_BETA = 0.1
LM_MAX_NEW_TOKENS = 24
LM_TEMPERATURE = 0.7
LM_TOP_P = 0.95
MODEL_ID = "HuggingFaceTB/SmolLM2-135M-Instruct"

_FEEDBACK = re.compile(
    r"You submitted \[([A-Za-z]{5})\]\.\nFeedback:\n[A-Za-z ]+\n([GYX](?: [GYX]){4})"
)
_GUESS = re.compile(r"\[([A-Za-z]{5})\]")
_BARE = re.compile(r"\b([A-Za-z]{5})\b")

SYSTEM_PROMPT = (
    "You are playing Wordle. Reply with exactly one five-letter English word "
    "in square brackets, such as [crane]. Use the feedback. Do not explain."
)


def main() -> int:
    self_test()
    if "--self-test" in sys.argv:
        print("self-test ok")
        return 0

    base_url = os.environ["SENV_BASE_URL"].rstrip("/")
    out_dir = Path(os.environ["SENV_EXPERIMENT_OUT"])
    out_dir.mkdir(parents=True, exist_ok=True)
    words = load_words(Path(__file__).resolve().parents[1] / "assets" / "wordle" / "words.txt")
    held_out = list(range(HELD_OUT_START, HELD_OUT_START + HELD_OUT_COUNT))
    report: dict = {
        "base_url": base_url,
        "model_id": MODEL_ID,
        "held_out_seeds": held_out,
        "policy_train_episodes": POLICY_TRAIN_EPISODES,
        "policy_lr": POLICY_LR,
        "lm_train_episodes": LM_TRAIN_EPISODES,
        "lm_lr": LM_LR,
        "lm_max_new_tokens": LM_MAX_NEW_TOKENS,
        "lm_temperature": LM_TEMPERATURE,
        "lm_top_p": LM_TOP_P,
        "notes": [
            "Agents do not call state() and do not read secret_word.",
            "linear-policy is not a language model.",
            "linear-policy trains with one-step REINFORCE on wordle.greens + 0.5*wordle.yellows when that signal is present, otherwise the client step reward. Held-out mean_reward is the client reward only.",
            "lm-pretrained is inference only and runs before any optimizer step.",
            "lm-reinforce is gradient updates on that same model.",
            "Success means the episode ended with reward >= 0.999.",
        ],
        "phases": {},
    }

    def save() -> None:
        (out_dir / "results.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

    client = connect(base_url)
    try:
        started = time.perf_counter()
        report["phases"]["random"] = evaluate(
            client,
            held_out,
            "random",
            lambda text, rng: random_action(words, rng),
        )
        report["phases"]["random"]["wall_s"] = time.perf_counter() - started
        save()
        print(summarize("random", report["phases"]["random"]), flush=True)

        theta = [0.0, 0.0, 0.0, 0.0]
        started = time.perf_counter()
        theta, train = train_linear_policy(client, words, theta)
        train["wall_s"] = time.perf_counter() - started
        report["linear_policy_train"] = train
        report["linear_policy_theta"] = {
            "bias": theta[0],
            "consistent": theta[1],
            "green_hits": theta[2],
            "novel": theta[3],
        }
        save()
        print("linear theta", report["linear_policy_theta"], flush=True)

        started = time.perf_counter()
        report["phases"]["linear-sample"] = evaluate(
            client,
            held_out,
            "linear-sample",
            lambda text, rng: linear_action(words, theta, text, rng, greedy=False),
        )
        report["phases"]["linear-sample"]["wall_s"] = time.perf_counter() - started
        save()
        print(summarize("linear-sample", report["phases"]["linear-sample"]), flush=True)

        started = time.perf_counter()
        report["phases"]["linear-greedy"] = evaluate(
            client,
            held_out,
            "linear-greedy",
            lambda text, rng: linear_action(words, theta, text, rng, greedy=True),
        )
        report["phases"]["linear-greedy"]["wall_s"] = time.perf_counter() - started
        save()
        print(summarize("linear-greedy", report["phases"]["linear-greedy"]), flush=True)
    finally:
        close_client(client)
        save()

    started = time.perf_counter()
    try:
        lm_report = run_language_model(base_url, held_out, out_dir)
    except Exception as exc:
        report["language_model_error"] = repr(exc)
        save()
        raise
    lm_report["wall_s"] = time.perf_counter() - started
    report["language_model"] = lm_report
    save()
    printable = {key: value for key, value in lm_report.items() if key not in {"pretrained_eval", "trained_eval"}}
    print(json.dumps(printable, indent=2), flush=True)
    return 0


def connect(base_url: str):
    from textarena_env import TextArenaEnv

    client = TextArenaEnv(base_url=base_url).sync()
    client.__enter__()
    return client


def close_client(client: object) -> None:
    closer = getattr(client, "__exit__", None)
    if closer is not None:
        closer(None, None, None)


def evaluate(client, seeds: list[int], name: str, choose) -> dict:
    episodes = []
    for seed in seeds:
        rng = random.Random(f"{name}-{seed}")
        episodes.append(play_episode(client, seed, f"{name}-{seed}", choose, rng))
    return aggregate(episodes)


def play_episode(client, seed: int, episode_id: str, choose, rng: random.Random) -> dict:
    result = client.reset(seed=seed, episode_id=episode_id)
    actions: list[str] = []
    reward = 0.0
    done = False
    for _ in range(8):
        if result.done:
            done = True
            reward = _reward(result)
            break
        action = choose(observation_text(result.observation), rng)[:200]
        actions.append(action)
        result = client.step(_action(action))
        reward = _reward(result)
        done = bool(result.done)
        if done:
            break
    return {
        "seed": seed,
        "episode_id": episode_id,
        "actions": actions,
        "steps": len(actions),
        "reward": reward,
        "success": bool(done and reward >= 0.999),
    }


def train_linear_policy(client, words: list[str], theta: list[float]) -> tuple[list[float], dict]:
    """One-step REINFORCE. The update uses a shaped step reward.

    Held-out evaluation does not use this reward. It reports the client reward.
    """
    baseline = 0.0
    official_returns = []
    shaped_returns = []
    for episode in range(POLICY_TRAIN_EPISODES):
        seed = episode
        rng = random.Random(f"linear-train-{seed}")
        result = client.reset(seed=seed, episode_id=f"linear-train-{seed}")
        official = 0.0
        shaped_total = 0.0
        steps = 0
        while steps < 8 and not result.done:
            text = observation_text(result.observation)
            history = parse_history(text)
            chosen, step_grad = sample_linear(words, theta, history, rng)
            result = client.step(_action(f"[{chosen}]"))
            official = _reward(result)
            shaped = shaped_step_reward(result)
            shaped_total += shaped
            advantage = shaped - baseline
            for index in range(4):
                update = max(-1.0, min(1.0, advantage * step_grad[index]))
                theta[index] += POLICY_LR * update
            baseline = (1.0 - POLICY_BASELINE_BETA) * baseline + POLICY_BASELINE_BETA * shaped
            steps += 1
            if episode == 0 and steps == 1:
                print(
                    f"first step official {official:.4f} shaped {shaped:.4f} "
                    f"info {getattr(result.observation, 'info', None)}",
                    flush=True,
                )
        official_returns.append(official)
        shaped_returns.append(shaped_total)
        if (episode + 1) % 40 == 0:
            recent = official_returns[-40:]
            print(
                f"linear train {episode + 1} official_mean {sum(recent) / len(recent):.4f} "
                f"wins {sum(1 for value in recent if value >= 0.999)} "
                f"shaped_mean {sum(shaped_returns[-40:]) / 40:.4f} theta {theta}",
                flush=True,
            )
    first = official_returns[:40]
    last = official_returns[-40:]
    return theta, {
        "episodes": POLICY_TRAIN_EPISODES,
        "seeds": list(range(POLICY_TRAIN_EPISODES)),
        "lr": POLICY_LR,
        "baseline_beta": POLICY_BASELINE_BETA,
        "algorithm": (
            "one-step REINFORCE; shaped reward is wordle.greens + 0.5*wordle.yellows "
            "when reward_signals is present, otherwise the client step reward"
        ),
        "features": ["bias", "consistent", "green_hits", "novel"],
        "first_40_mean_reward": sum(first) / len(first),
        "last_40_mean_reward": sum(last) / len(last),
        "first_40_wins": sum(1 for value in first if value >= 0.999),
        "last_40_wins": sum(1 for value in last if value >= 0.999),
        "first_40_mean_shaped": sum(shaped_returns[:40]) / 40,
        "last_40_mean_shaped": sum(shaped_returns[-40:]) / 40,
        "theta": theta,
    }


def shaped_step_reward(result) -> float:
    info = getattr(result.observation, "info", None)
    if not isinstance(info, dict):
        info = dict(info) if hasattr(info, "items") else {}
    signals = info.get("reward_signals") if isinstance(info, dict) else None
    if isinstance(signals, dict):
        greens = float(signals.get("wordle.greens", 0.0))
        yellows = float(signals.get("wordle.yellows", 0.0))
        if greens > 0.0 or yellows > 0.0:
            return greens + 0.5 * yellows
    return _reward(result)


def run_language_model(base_url: str, held_out: list[int], out_dir: Path) -> dict:
    import torch
    import transformers

    cuda = torch.cuda.is_available()
    device = torch.device("cuda" if cuda else "cpu")
    dtype = torch.float32
    if cuda:
        torch.cuda.reset_peak_memory_stats()
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
    if tokenizer.pad_token_id is None:
        tokenizer.pad_token = tokenizer.eos_token
    revision = getattr(model.config, "_commit_hash", None)
    info = {
        "model_id": MODEL_ID,
        "revision": revision,
        "parameters": int(sum(parameter.numel() for parameter in model.parameters())),
        "dtype": str(dtype).replace("torch.", ""),
        "device": torch.cuda.get_device_name(0) if cuda else "cpu",
        "cuda": cuda,
        "torch": torch.__version__,
        "transformers": transformers.__version__,
        "inference_only_before_training": True,
    }
    print(f"loaded {MODEL_ID} revision={revision} device={info['device']}", flush=True)

    def choose_factory():
        def choose(text: str, rng: random.Random) -> str:
            import torch

            seed = rng.randrange(2**31)
            torch.manual_seed(seed)
            if device.type == "cuda":
                torch.cuda.manual_seed_all(seed)
            return generate_action(model, tokenizer, device, text)

        return choose

    client = connect(base_url)
    try:
        model.eval()
        pretrained = evaluate(client, held_out, "lm-pretrained", choose_factory())
        info["pretrained_eval"] = pretrained
        print(summarize("lm-pretrained", pretrained), flush=True)
        (out_dir / "lm-partial.json").write_text(
            json.dumps(info, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

        model.train()
        train = train_lm(model, tokenizer, device, client)
        info["reinforce"] = train
        print(
            "lm train "
            f"first_8 {train['first_8_mean_reward']:.4f} "
            f"last_8 {train['last_8_mean_reward']:.4f}",
            flush=True,
        )

        model.eval()
        trained = evaluate(client, held_out, "lm-reinforce", choose_factory())
        info["trained_eval"] = trained
        print(summarize("lm-reinforce", trained), flush=True)
    finally:
        close_client(client)

    if cuda:
        info["peak_memory_allocated_bytes"] = int(torch.cuda.max_memory_allocated())
        info["peak_memory_reserved_bytes"] = int(torch.cuda.max_memory_reserved())
    return info


def train_lm(model, tokenizer, device, client) -> dict:
    import torch

    optimizer = torch.optim.AdamW(model.parameters(), lr=LM_LR)
    baseline = 0.0
    returns = []
    for episode in range(LM_TRAIN_EPISODES):
        seed = episode
        rng = random.Random(f"lm-train-{seed}")
        del rng
        torch.manual_seed(seed)
        if device.type == "cuda":
            torch.cuda.manual_seed_all(seed)
        result = client.reset(seed=seed, episode_id=f"lm-train-{seed}")
        traces = []
        reward = 0.0
        steps = 0
        while steps < 8 and not result.done:
            text = observation_text(result.observation)
            prompt_ids, completion_ids, action = sample_completion(model, tokenizer, device, text)
            traces.append((prompt_ids, completion_ids))
            result = client.step(_action(action))
            reward = _reward(result)
            steps += 1
        advantage = reward - baseline
        optimizer.zero_grad(set_to_none=True)
        updated = False
        for prompt_ids, completion_ids in traces:
            logp = completion_logprob(model, prompt_ids.to(device), completion_ids.to(device))
            if logp is None:
                continue
            loss = -(advantage) * logp
            loss.backward()
            updated = True
        if updated:
            torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
            optimizer.step()
        baseline = (1.0 - LM_BASELINE_BETA) * baseline + LM_BASELINE_BETA * reward
        returns.append(reward)
        print(f"lm train {episode + 1} reward {reward:.4f} steps {steps}", flush=True)
    first = returns[:8]
    last = returns[-8:]
    return {
        "episodes": LM_TRAIN_EPISODES,
        "seeds": list(range(LM_TRAIN_EPISODES)),
        "lr": LM_LR,
        "optimizer": "AdamW",
        "grad_clip": 1.0,
        "baseline_beta": LM_BASELINE_BETA,
        "algorithm": "REINFORCE on sampled completion tokens, terminal env reward",
        "max_new_tokens": LM_MAX_NEW_TOKENS,
        "temperature": LM_TEMPERATURE,
        "top_p": LM_TOP_P,
        "first_8_mean_reward": sum(first) / len(first),
        "last_8_mean_reward": sum(last) / len(last),
        "first_8_wins": sum(1 for value in first if value >= 0.999),
        "last_8_wins": sum(1 for value in last if value >= 0.999),
        "mean_reward": sum(returns) / len(returns),
        "wins": sum(1 for value in returns if value >= 0.999),
    }


def generate_action(model, tokenizer, device, text: str) -> str:
    import torch

    model.eval()
    with torch.no_grad():
        _prompt, completion, action = sample_completion(model, tokenizer, device, text)
    return action


def sample_completion(model, tokenizer, device, text: str):
    import torch

    messages = [
        {"role": "system", "content": SYSTEM_PROMPT},
        {"role": "user", "content": text},
    ]
    encoded = tokenizer.apply_chat_template(
        messages, add_generation_prompt=True, return_tensors="pt"
    )
    prompt_ids = encoded["input_ids"] if hasattr(encoded, "keys") else encoded
    prompt_ids = prompt_ids.to(device)
    was_training = model.training
    model.eval()
    with torch.no_grad():
        generated = model.generate(
            prompt_ids,
            max_new_tokens=LM_MAX_NEW_TOKENS,
            do_sample=True,
            temperature=LM_TEMPERATURE,
            top_p=LM_TOP_P,
            pad_token_id=tokenizer.pad_token_id,
        )
    if was_training:
        model.train()
    completion_ids = generated[0, prompt_ids.shape[1] :].detach().to("cpu")
    prompt_cpu = prompt_ids.detach().to("cpu")
    decoded = tokenizer.decode(completion_ids, skip_special_tokens=True)
    return prompt_cpu, completion_ids, parse_model_action(decoded)


def completion_logprob(model, prompt_ids, completion_ids):
    import torch

    if completion_ids.numel() == 0:
        return None
    full = torch.cat([prompt_ids, completion_ids.view(1, -1)], dim=1)
    logits = model(full).logits
    start = prompt_ids.shape[1] - 1
    relevant = logits[:, start:-1, :]
    logp = torch.log_softmax(relevant.float(), dim=-1)
    chosen = logp.gather(2, completion_ids.view(1, -1).unsqueeze(-1)).squeeze(-1)
    return chosen.mean()


def parse_model_action(text: str) -> str:
    match = _GUESS.search(text)
    if match:
        return f"[{match.group(1).lower()}]"
    bare = _BARE.search(text)
    if bare:
        return f"[{bare.group(1).lower()}]"
    stripped = " ".join(text.split())
    return stripped[:200] if stripped else "[?????]"


def random_action(words: list[str], rng: random.Random) -> str:
    return f"[{rng.choice(words)}]"


def linear_action(words, theta, text: str, rng: random.Random, greedy: bool) -> str:
    history = parse_history(text)
    if greedy:
        best_index = 0
        best_score = None
        for index, word in enumerate(words):
            score = dot(theta, features(word, history))
            if best_score is None or score > best_score:
                best_score = score
                best_index = index
        return f"[{words[best_index]}]"
    chosen, _grad = sample_linear(words, theta, history, rng)
    return f"[{chosen}]"


def sample_linear(words, theta, history, rng: random.Random):
    scores = []
    feats = []
    for word in words:
        feat = features(word, history)
        feats.append(feat)
        scores.append(dot(theta, feat))
    index = categorical(rng, scores)
    expected = expected_features(scores, feats)
    grad = [feats[index][axis] - expected[axis] for axis in range(4)]
    return words[index], grad


def expected_features(scores: list[float], feats: list[tuple[float, float, float, float]]):
    peak = max(scores)
    weights = [math.exp(score - peak) for score in scores]
    total = sum(weights)
    expected = [0.0, 0.0, 0.0, 0.0]
    for weight, feat in zip(weights, feats):
        scale = weight / total
        for axis in range(4):
            expected[axis] += scale * feat[axis]
    return expected


def categorical(rng: random.Random, scores: list[float]) -> int:
    peak = max(scores)
    total = 0.0
    weights = []
    for score in scores:
        weight = math.exp(score - peak)
        weights.append(weight)
        total += weight
    pick = rng.random() * total
    acc = 0.0
    for index, weight in enumerate(weights):
        acc += weight
        if acc >= pick:
            return index
    return len(scores) - 1


def features(word: str, history: list[tuple[str, str]]) -> tuple[float, float, float, float]:
    consistent = 1.0
    greens = 0
    green_hits = 0
    for guess, marks in history:
        parts = marks.split()
        for index, mark_char in enumerate(parts):
            if mark_char == "G":
                greens += 1
                if word[index] == guess[index]:
                    green_hits += 1
        if consistent and mark(word, guess) != marks:
            consistent = 0.0
    green_score = 1.0 if greens == 0 else green_hits / greens
    novel = 0.0 if any(word == guess for guess, _marks in history) else 1.0
    return (1.0, consistent, green_score, novel)


def parse_history(text: str) -> list[tuple[str, str]]:
    return [(match.group(1).lower(), match.group(2)) for match in _FEEDBACK.finditer(text)]


def mark(secret: str, guess: str) -> str:
    """Two-pass Wordle marks. Greens consume a letter before yellows do."""
    remaining = list(secret)
    flags = [""] * WORD_LEN
    for index in range(WORD_LEN):
        if guess[index] == remaining[index]:
            flags[index] = "G"
            remaining[index] = ""
    chars = []
    for index in range(WORD_LEN):
        if flags[index] == "G":
            chars.append("G")
            continue
        letter = guess[index]
        try:
            found = remaining.index(letter)
        except ValueError:
            chars.append("X")
            continue
        remaining[found] = ""
        chars.append("Y")
    return " ".join(chars)


def dot(theta: list[float], feat: tuple[float, float, float, float]) -> float:
    return theta[0] * feat[0] + theta[1] * feat[1] + theta[2] * feat[2] + theta[3] * feat[3]


def observation_text(observation) -> str:
    parts: list[str] = []
    prompt = getattr(observation, "prompt", "") or ""
    if prompt:
        parts.append(prompt)
    for message in getattr(observation, "messages", []):
        content = (getattr(message, "content", "") or "").strip()
        if content and content not in parts:
            parts.append(content)
    return "\n".join(parts)


def aggregate(episodes: list[dict]) -> dict:
    rewards = [episode["reward"] for episode in episodes]
    wins = sum(1 for episode in episodes if episode["success"])
    return {
        "episodes": len(episodes),
        "mean_reward": (sum(rewards) / len(rewards)) if rewards else None,
        "successes": wins,
        "success_rate": (wins / len(episodes)) if episodes else None,
        "mean_steps": (sum(episode["steps"] for episode in episodes) / len(episodes)) if episodes else None,
        "records": [
            {
                "seed": episode["seed"],
                "reward": episode["reward"],
                "success": episode["success"],
                "steps": episode["steps"],
            }
            for episode in episodes
        ],
    }


def summarize(name: str, phase: dict) -> str:
    return (
        f"{name} n={phase['episodes']} mean_reward={phase['mean_reward']:.4f} "
        f"success={phase['successes']}/{phase['episodes']} "
        f"mean_steps={phase['mean_steps']:.2f}"
    )


def _reward(result) -> float:
    value = getattr(result, "reward", None)
    if value is None:
        return 0.0
    return float(value)


def _action(message: str):
    from textarena_env.models import TextArenaAction

    return TextArenaAction(message=message)


def load_words(path: Path) -> list[str]:
    words = []
    for line in path.read_text(encoding="utf-8").splitlines():
        word = line.strip()
        if len(word) == WORD_LEN and word.isalpha() and word.islower():
            words.append(word)
    if len(words) < 100:
        raise SystemExit(f"word list {path} has {len(words)} words")
    return words


def self_test() -> None:
    if mark("crane", "cigar") != "G X X Y Y":
        raise SystemExit(f"cigar/crane marks {mark('crane', 'cigar')}")
    if mark("alley", "llama") != "Y G Y X X":
        raise SystemExit(f"llama/alley marks {mark('alley', 'llama')}")
    if mark("apple", "apple") != "G G G G G":
        raise SystemExit("apple marks")
    text = (
        "You submitted [cigar].\nFeedback:\nC I G A R\nG X X Y Y\nYou have 5 guesses left."
    )
    history = parse_history(text)
    if history != [("cigar", "G X X Y Y")]:
        raise SystemExit(f"parse {history}")
    if features("crane", history)[1] != 1.0:
        raise SystemExit("crane should be consistent with cigar's marks against crane")
    if features("cigar", history)[1] != 0.0:
        raise SystemExit("repeating cigar is not a fresh consistent secret check")
    # cigar against secret cigar is all green, so it is NOT consistent with GXXYY.
    if mark("cigar", "cigar") == "G X X Y Y":
        raise SystemExit("all-green was parsed as the mixed mark")
    words = ["crane", "cigar", "apple", "stone"]
    rng = random.Random(0)
    chosen, grad = sample_linear(words, [0.0, 0.0, 0.0, 0.0], history, rng)
    if chosen not in words or len(grad) != 4:
        raise SystemExit("sample_linear")
    action = parse_model_action("I guess [Crane] now")
    if action != "[crane]":
        raise SystemExit(action)
    if "--self-test" in sys.argv:
        words_path = Path(__file__).resolve().parents[1] / "assets" / "wordle" / "words.txt"
        if words_path.is_file():
            full = load_words(words_path)
            started = time.perf_counter()
            sample_linear(full, [0.0, 2.0, 0.0, 0.0], history, random.Random(1))
            elapsed = time.perf_counter() - started
            print(f"one full-list sample {elapsed:.3f}s over {len(full)} words")


if __name__ == "__main__":
    raise SystemExit(main())
