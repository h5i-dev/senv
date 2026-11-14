# Arith-v0 diagnosis

RL training stayed stopped. No new training run and no larger-model download. The held-out test seeds were not read. Raw JSON stays under `/mnt/scratch/h5i-openenv/experiment-arith-diagnose/`. The frozen 0/200 SmolLM2 result and the warmup logs were not rewritten.

Date: 2026-11-10. GPU: NVIDIA A10G. torch 2.15.0+cu132, transformers 5.22.2, peft 0.21.2. Untouched weights, no LoRA.

| Model | Revision |
|---|---|
| `Qwen/Qwen2.5-0.5B-Instruct` | `7ae557604adf67be50417f59c2c2f167def9a775` |
| `HuggingFaceTB/SmolLM2-135M-Instruct` | `12fd25f77366fa6b3b4b768ec3050bf629380bac` |

## What was wrong

Three separate defects. None of them is "the model is too small" by itself.

1. `generate` inherited sampler settings the training script never set. Qwen's `generation_config.json` sets `repetition_penalty` 1.1, `top_k` 20, `top_p` 0.8, and `temperature` 0.7. The harness passed temperature 0.7 and top-p 0.95, so those two were overridden, and the penalty and top-k stayed. Greedy decoding still ran `RepetitionPenaltyLogitsProcessor`. SmolLM2's card leaves top-k unset, and the library default then fills top-k 50. A harness-shaped sample therefore ran top-k plus top-p, not top-p alone.

2. The trainer scored `log_softmax` of the raw logits. Sampling used temperature 0.7 and top-p 0.95, and, before this fix, the inherited top-k and Qwen penalty as well. On one Qwen sample of the live seed-2000 prompt the untempered token-logprob sum was −13.980 and the temperature-0.7 sum was −16.001 (max absolute token gap 1.268). The line was `calc 10 * (4 / 2)`. That sample still included top-k 20 and penalty 1.1, which neither sum applied. A near-certain `4` for "1+3" only moves by about 0.0004, so the easy case hides the bug. A SmolLM2 sample of the same question, 24 tokens, moved from −28.442 untempered to −24.789 under temperature and top-p (max absolute token gap 0.658).

3. Newline-as-EOS and first-line extraction dropped a correct answer. After the decoder was pinned, Qwen's greedy continuation of the live ops-2 seed-2000 prompt (expression `((6 * 8) * 9)`, value 432) was:

```text
calc 6 * 8 * 9
answer 432
```

Tokens: `calc`, ` `, `6`, ` *`, ` `, `8`, ` *`, ` `, `9`, `\n`, `answer`, ` `, `4`, `3`, `2`, `<|im_end|>`. Stop was EOS, 16 tokens. Stopping on the newline token keeps only `calc 6 * 8 * 9\n`. Taking the first line of the full text also keeps `calc 6 * 8 * 9`. The server would score that as `calc 6 * 8` (result 48, reward 0) and never see `answer 432`.

## What was not a bug

Checked on the same prompts, greedy, native chat template, bf16.

- Native `apply_chat_template(tokenize=True)` token ids matched `tokenizer(prompt_string)` on every Qwen and SmolLM2 case. `prefix_matches_prompt` was true. Completion slicing was not eating the prompt or gluing prompt tokens onto the reply.
- The parenthesized echo is model output. Before the decoder pin, Qwen's harness continuation was the tokens `calc`, ` (`, `6`, ` *`, ` `, `8`, `)`, ` *`, ` `, `9`, `<|im_end|>`, decoded `calc (6 * 8) * 9`. That is not a slice of the prompt.
- bf16 matched fp32 on the probes that were run in both: Qwen `1+3` is `4`; the harness nested question stays `27`; SmolLM2's direct `1+3` prose is the same sentence.
- Feedback reaches the next turn. The invalid-action sentence is in the next observation. Qwen, under the old penalty, repeated `calc (6 * 8) * 9` anyway. SmolLM2 repeats `(6 * 8) * 9` for three turns after the same reply.
- The server agrees with an independent evaluator on seed 2000. The prompt and `state.raw_state` do not contain the value. Keys are `done`, `expression`, `last_reply`, `max_steps`, `ops`, `steps`.

| Action | Reward | Done | Reply |
|---|---:|---|---|
| `calc (6 * 8) * 9` | 0 | no | invalid-action text, value not added |
| `calc 6 * 8` | 0 | no | `Result: 48` |
| `answer 1` | 0 | yes | `Incorrect.` |
| `answer 432` | 1 | yes | `Correct.` |

`answer 432` puts 432 in the transcript because that string is the action. The server reply is `Correct.`

The loss is still the mean completion-token logprob, so longer lines are down-weighted. Prompt tokens stay out of the mask. The baseline still starts at 0 when the answer bonus is on, and at 0.25 otherwise. Those choices match the code that produced the warmup logs. They were not changed here. The parser was not loosened: a parenthesis is still invalid, and `calc 6 * 8 * 9` is still the single operation `6 * 8`.

## Fixes

`generation_kwargs` passes `repetition_penalty=1.0` and `top_k=0` on every `generate`, and `load_bundle` pins the same fields on `generation_config` so the checkpoint and the library default (top-k 50) cannot come back. Sampling logprobs go through `policy_logprobs`: temperature 0.7, then top-p 0.95 with the transformers rule (sort ascending, drop the tail whose cumulative mass is at most `1 - top_p`, always keep the top token), then log-softmax. `generate_line` no longer treats newline as EOS. `action_line` submits the first `answer <int>` line when one is present, and otherwise the first non-empty line.

`scripts/test_sampling_logprobs.py` checks the temperature gap, exact agreement with `TemperatureLogitsWarper` and `TopPLogitsWarper`, the decoder kwargs, and that `policy_logprobs` is not the untempered score. `scripts/arith_task.py` checks that `calc 6 * 8 * 9\nanswer 432` submits `answer 432`. Both passed. The mask check's LoRA step was not re-run; that step is a training update, and this pass did not train.

## Same suite, before and after

Greedy Qwen. "Before" is the inherited penalty. "Pinned" turns the penalty and top-k off and still keeps the first line. "Keep answer" is the pinned decoder plus the answer-line rule. True values: `1+3` = 4, `8-3` = 5, `((2 * 8) + 3)` = 19, `((6 * 8) * 9)` = 432.

| Prompt | Before | Pinned, first line | Keep answer |
|---|---|---|---|
| `1+3`, integer only, minimal or harness | `4` | `4` | `4` |
| `1 + 3`, minimal | `The answer to 1 + 3 is 4.` | `1 + 3 is equal to 4.` | same |
| `1 + 3`, harness | `4` | `4` | `4` |
| `8 - 3`, integer only | `5` | `5` | `5` |
| `((2 * 8) + 3)`, minimal | `19` | `21` | `21` |
| `((2 * 8) + 3)`, harness | `27` | `27` | `27` |
| `((6 * 8) * 9)`, minimal | `720` | `72` | `72` |
| `((6 * 8) * 9)`, harness | `54` | `54` | `54` |
| live seed 2000, newline stop | `calc (6 * 8) * 9` | `calc 6 * 8 * 9` | `calc 6 * 8 * 9` |
| live seed 2000, EOS only | `calc (6 * 8) * 9` | raw has `answer 432`, sent line was the calc | sent `answer 432` |

The correct `19` on the minimal nested prompt existed only while repetition penalty 1.1 was on. With the penalty off, that prompt says `21`. "Reply with one line." and the full harness system string both say `27` with the penalty on or off, in bf16 and in fp32. The harness string is still a prompt limitation. It was not rewritten to chase `19`.

On the live prompt, the pinned decoder's EOS continuation is the two-line text above. The feedback probe then sent `answer 432` through the real client. The server returned reward 1.0, done, reply `Correct.` That is one validation seed, greedy, not a held-out score and not a trained policy.

SmolLM2 on the final decoder does not emit a bare integer or a legal command. Harness `1+3` is `1+3 is 4.` Direct `1+3` is a sentence about an arithmetic series, in bf16 and fp32. The nested product ends `... equals 540.` The live prompt is copied back as `(6 * 8) * 9` and then the instruction text, for 64 tokens, with no `answer` line. Three feedback turns repeat that line, reward 0, and the invalid-action reply is in the next observation. Chat-template ids matched the retokenized prompt there too.

## What this does not explain away

Qwen's direct nested answers are still wrong (`21` or `27` against 19, `72` or `54` against 432). SmolLM2 does not follow the command format on these prompts. The system string still changes Qwen's nested greedy answer, and neither side is the true value once the penalty is off. Keeping a later answer line only helps when the model actually writes one. Twenty-four new tokens were enough for the seed-2000 Qwen continuation (16 tokens) and would still cut off a longer first line. The mean-logprob loss and the 0.25 baseline are unchanged. No validation sweep and no test sweep was run after the fix. RL has not been resumed.

## Reproduce

```text
python scripts/test_sampling_logprobs.py
python scripts/arith_task.py
```

Use `/mnt/scratch/h5i-openenv/venv/bin/python`. The GPU suite needs the pinned OpenEnv tree on `PYTHONPATH`, `HF_HOME=/mnt/scratch/h5i-openenv/hf`, and `senv serve --env arith --arith-ops 2`. Then `SENV_DIAG_NAME=qwen-keep-answer` or `smol` with `scripts/arith_diagnose.py`. It does not train and it does not open seeds 10000..10199.

Preserved logs: `diagnose-qwen-before.json`, `qwen-ablation.json`, `qwen-genconfig-probe.json`, `diagnose-qwen-after.json`, `diagnose-qwen-keep-answer.json`, `diagnose-smol.json`.
