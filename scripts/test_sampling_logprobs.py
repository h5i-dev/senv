#!/usr/bin/env python3
"""CPU checks for the Arith sampling distribution.

The training step used to score an untempered softmax for tokens drawn with
temperature 0.7 and top-p 0.95. These checks lock the scorer to that
distribution and to the transformers top-p implementation. No model download
and no GPU.
"""

from __future__ import annotations

import sys
from pathlib import Path

import torch
from transformers.generation.logits_process import TemperatureLogitsWarper, TopPLogitsWarper

sys.path.insert(0, str(Path(__file__).resolve().parent))

from arith_experiment import (  # noqa: E402
    TEMPERATURE,
    TOP_P,
    completion_logprobs,
    generation_kwargs,
    policy_logprobs,
    sampling_log_softmax,
)


class _Tokenizer:
    eos_token_id = 2
    pad_token_id = 2

    def encode(self, text: str, add_special_tokens: bool = False) -> list[int]:
        del add_special_tokens
        if text == "\n":
            return [9]
        return [1, 2]


class _Output:
    def __init__(self, logits: torch.Tensor) -> None:
        self.logits = logits


class _Model:
    """Fixed next-token table. Logits do not depend on the input."""

    def __call__(self, input_ids: torch.Tensor, attention_mask: torch.Tensor) -> _Output:
        del attention_mask
        vocab = 5
        row = torch.tensor([0.2, 1.5, 0.0, 3.0, -1.0])
        logits = row.view(1, 1, vocab).expand(input_ids.shape[0], input_ids.shape[1], vocab).clone()
        return _Output(logits)


def _hf_sampling_log_softmax(logits: torch.Tensor, temperature: float, top_p: float) -> torch.Tensor:
    scores = logits.float().unsqueeze(0)
    if temperature != 1.0:
        scores = TemperatureLogitsWarper(float(temperature))(torch.zeros(1, 1), scores)
    if top_p < 1.0:
        scores = TopPLogitsWarper(float(top_p))(torch.zeros(1, 1), scores)
    return torch.log_softmax(scores, dim=-1).squeeze(0)


def test_temperature_changes_the_score() -> None:
    logits = torch.tensor([0.0, 1.0, 3.0])
    plain = sampling_log_softmax(logits, 1.0, 1.0)
    tempered = sampling_log_softmax(logits, 0.7, 1.0)
    assert torch.allclose(plain, torch.log_softmax(logits, dim=-1))
    assert torch.allclose(tempered, torch.log_softmax(logits / 0.7, dim=-1))
    assert not torch.allclose(plain, tempered)
    # The audit's env-prompt sample had a per-token gap above 1. This row does too.
    assert float((plain - tempered).abs().max().item()) > 0.5


def test_top_p_matches_transformers() -> None:
    logits = torch.tensor([0.0, 0.2, 0.4, 1.0, 6.0, -0.5, 2.0])
    for temperature, top_p in ((1.0, 0.95), (0.7, 0.95), (0.7, 0.5), (1.0, 1.0)):
        actual = sampling_log_softmax(logits, temperature, top_p)
        expected = _hf_sampling_log_softmax(logits, temperature, top_p)
        assert torch.allclose(actual, expected, atol=1e-5), (temperature, top_p, actual, expected)
    tight = sampling_log_softmax(logits, 1.0, 0.5)
    assert torch.isneginf(tight[:4]).all()
    kept = tight[~torch.isneginf(tight)]
    assert torch.allclose(kept.exp().sum(), torch.tensor(1.0), atol=1e-5)


def test_generation_kwargs_disable_inherited_sampler() -> None:
    greedy = generation_kwargs(_Tokenizer(), "greedy", 24, True)
    sample = generation_kwargs(_Tokenizer(), "sample", 24, True)
    assert greedy["do_sample"] is False
    assert greedy["repetition_penalty"] == 1.0
    assert greedy["top_k"] == 0
    assert "temperature" not in greedy
    assert greedy["eos_token_id"] == [9, 2]
    assert sample["do_sample"] is True
    assert sample["temperature"] == TEMPERATURE == 0.7
    assert sample["top_p"] == TOP_P == 0.95
    assert sample["top_k"] == 0
    assert sample["repetition_penalty"] == 1.0


def test_policy_logprobs_use_the_sampling_distribution() -> None:
    model = _Model()
    prompt = torch.tensor([4, 4, 4])
    completion = torch.tensor([3, 1])
    plain = completion_logprobs(model, prompt, completion, temperature=1.0, top_p=1.0)
    sampling = policy_logprobs(model, prompt, completion)
    manual = completion_logprobs(model, prompt, completion, temperature=TEMPERATURE, top_p=TOP_P)
    assert plain.shape == completion.shape
    assert torch.allclose(sampling, manual)
    assert not torch.allclose(plain, sampling)
    try:
        completion_logprobs(model, prompt, torch.tensor([], dtype=torch.long))
    except RuntimeError as exc:
        assert "completion" in str(exc)
    else:
        raise AssertionError("empty completion was accepted")


def main() -> int:
    test_temperature_changes_the_score()
    test_top_p_matches_transformers()
    test_generation_kwargs_disable_inherited_sampler()
    test_policy_logprobs_use_the_sampling_distribution()
    print("PASS sampling logprob and decoder kwargs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
