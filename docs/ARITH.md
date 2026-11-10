# Arith-v0 measured run

Custom task, served by `senv serve --env arith` and scored through the unmodified OpenEnv `TextArenaEnv` client. It is not an upstream OpenEnv environment. The server keeps the expression value out of the observation and out of `state.raw_state`. Policies in these runs do not call `state()`.

Date: 2026-11-10. Host GPU: NVIDIA A10G, bf16. Full episode logs stay under `/mnt/scratch/h5i-openenv/experiment-arith`. The summary table is `docs/experiment/arith-results.json`.

## Models

| Model | Revision | License | LoRA |
|---|---|---|---|
| `HuggingFaceTB/SmolLM2-135M-Instruct` | `12fd25f77366fa6b3b4b768ec3050bf629380bac` | apache-2.0 | r=8 on q/k/v/o, 921,600 trainable of 135,436,608 |
| `Qwen/Qwen2.5-0.5B-Instruct` | `7ae557604adf67be50417f59c2c2f167def9a775` | apache-2.0 | r=8 on q/k/v/o, 1,081,344 trainable of 495,114,112 |

Both chat templates include the system role. A repeated completion-token forward matched within 0. The logprob check uses learning rate 1e-4 and grad clip 1. At learning rate 0.01 the Qwen completion logprob fell from −0.409 to −85.9, so that check was rejected. At 1e-4, SmolLM2 moved from −6.374 to −5.907 and Qwen from −0.409 to −0.066.

Training objective for the frozen run: Monte Carlo REINFORCE on the terminal server reward only. Advantage is that reward minus an EMA baseline initialized at 0.25. Prompt tokens are excluded. Format validity is counted and is not added to the reward. AdamW learning rate 1e-4, grad clip 1, temperature 0.7, top-p 0.95, 24 new tokens.

## Frozen task

Validation pilots are 40 sample episodes, seeds 2000..2039. The freeze rule takes the largest operator count in {4, 3, 2} whose validation success sits in [0.05, 0.75]. Nothing landed in that band, so the run froze at 3 operators (`no_validation_success_in_[0.05,0.75]_frozen_at_3`). The test split is seeds 10000..10199 and was not used to choose the operator count.

| Pilot | Success | Mean format rate |
|---|---:|---:|
| ops 2, Qwen sample | 1/40 | 0.254 |
| ops 2, SmolLM2 sample | 0/40 | 0.009 |
| ops 3, Qwen sample | 0/40 | 0.179 |
| ops 3, SmolLM2 sample | 0/40 | 0.006 |
| ops 4, Qwen sample | 0/40 | 0.164 |
| ops 4, SmolLM2 sample | 0/40 | 0.000 |

The server check `answer <value>` scored 5/5 on train seeds. That check recomputes the public expression. It is not a baseline.

## Held-out ops 3

Wilson intervals are 95%.

| Policy | Success | Format | Wilson |
|---|---:|---:|---|
| `answer 0` | 3/200 | 1.000 | 0.005–0.043 |
| random legal line | 6/200 | 1.000 | 0.014–0.064 |
| SmolLM2 pretrained greedy | 0/200 | 0.000 | 0–0.019 |
| SmolLM2 pretrained sample | 0/200 | 0.006 | 0–0.019 |
| SmolLM2 after 200 REINFORCE episodes, seed 1, greedy | 0/200 | 0.000 | 0–0.019 |
| SmolLM2 after 200 REINFORCE episodes, seed 1, sample | 0/200 | 0.006 | 0–0.019 |

Seed 1 trained for 200 episodes, 200 updates, 0 skipped, wall time 6972 s. Validation greedy success was 0/40 at episodes 0, 50, 100, 150, and 200, and the validation format rate stayed 0. The baseline fell from 0.25 to about 0 because every server reward was 0.

Seed 2 was stopped at episode 99/200. All 99 logged rewards were 0. Format was 0 on 94 episodes and 0.12 on five. Validation greedy at episodes 0 and 50 was 0/40. No result file was written for that seed. Qwen was not given the 200-episode test or a training run on this frozen task.

## What the actions were

SmolLM2 greedy repeats the parenthesized expression, for example `(((1 + 3) * 3) - 8)`, for all 8 steps. Sampled SmolLM2 mostly writes prose or copies `calc <integer> <op> <integer>` from the prompt. The few format-valid lines are the example `calc 2 + 3` with extra words.

Qwen almost always starts with `calc`, then copies the parenthesized expression (`calc (6 * 8) * 9`). The parser accepts a line only when the first tokens are `calc <integer> <op> <integer>` or `answer <integer>`. A parenthesis in the second token is invalid. Trailing tokens are ignored only after a complete command, so `calc 6 * 8 * 9` scores as `6 * 8` and does not solve the expression.

On validation seed 2000 with 2 operators the expression is `((6 * 8) * 9)` and the value is 432. A format-first prompt made Qwen write `calc (6 * 8) * 9 answer 432`. The integer is right and the line is still invalid, because `answer` is not the first token. Across 8 greedy episodes that prompt had 16/56 embedded `answer` integers correct and 2/8 episodes containing the right integer. The same 8 episodes at 3 operators had 0 embedded integers correct (seed 2000's value is 434). The 40-episode ops-2 sample pilot had 8/46 embedded integers correct, 3/40 episodes containing the right integer, and 1/10 leading `answer` lines correct. That single leading correct line is the 1/40 validation success.

A cleaner instruction without a numeric example did not make Qwen drop the parentheses. Adding the examples `calc 8 - 3` and `answer 5` raised the ops-3 greedy format rate to 0.56 on 8 validation episodes by eliciting lines such as `calc 10 - 5 answer 5` and `calc 8 - 3 answer 5`, and the embedded integers were still wrong. SmolLM2 greedy under that instruction repeated the instruction sentence. Both checks used validation seeds 2000..2007 only.

## Answer-format warmup

Separate from the frozen 3-operator run. Operator count 2, Qwen2.5-0.5B-Instruct, one torch seed, 40 sampled training episodes on train seeds 0..39. The training target is the server reward plus 0.5 times the fraction of actions whose first tokens are `answer` and an integer. Validation is greedy on seeds 2000..2039. The test split was not read. Wall time 1046 s. Peak allocated GPU memory 5,086,340,096 bytes. Baseline prior 0.25.

| Checkpoint | Val success | Val format | Val leading-answer rate | Train window reward | Train window leading-answer rate |
|---|---:|---:|---:|---:|---:|
| 0 | 0/40 | 0.025 | 0.00 |  |  |
| 20 | 0/40 | 0.000 | 0.00 | 0 | 0.0225 |
| 40 | 0/40 | 0.000 | 0.00 | 0 | 0.000 |

Server reward was 0 on all 40 episodes. The only positive shaped returns were episode 1 (0.100) and episode 3 (0.125). Both were below the 0.25 baseline, so their advantages were negative and those answer lines were pushed down. From episode 4 on, the leading-answer rate in the log is 0. Validation greedy success stayed 0/40. This run does not show that the format bonus teaches the command.

The same 40 episodes with the baseline started at 0 are a separate check. Qwen, operator count 2, torch seed 1, validation greedy on seeds 2000..2039, test split unread. Wall time 219 s. Peak allocated GPU memory 3,852,050,432 bytes. Forty updates, none skipped.

| Checkpoint | Val success | Val format | Val leading-answer rate | Mean steps | Wilson 95% |
|---|---:|---:|---:|---:|---|
| 0 | 0/40 | 0.025 | 0.00 | 8.0 | 0–0.088 |
| 20 | 0/40 | 1.000 | 0.50 | 2.0 | 0–0.088 |
| 40 | 2/40 | 1.000 | 0.50 | 2.0 | 0.014–0.165 |

The second training window, episodes 21..40, had server reward on 1 of 20 sampled episodes (episode 27). A greedy replay of the final adapter on the same 40 validation seeds reproduces 2/40, format 1, and 2 steps. Every episode is one `calc` line and then one `answer` line. The calc does not match the expression: `calc 8 * 6` is 17 of the 40 lines, and `calc 4 * 6` is 7. The answer is `answer 10` on 20 of 40 episodes. Seed 2013 is the only validation problem whose value is 10, and the policy answered 10 after `calc 8 * 6`. Seed 2039 answered 12 for `((3 * 1) * 4)`, which is correct, while the two other value-12 problems were answered 10. The bonus taught a legal two-step template. It did not teach the policy to evaluate the expression.
