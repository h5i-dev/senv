# Wordle baselines and a short training run

One run on 2026-11-09 against `senv serve --env wordle` (release binary built from commit `49b3aa8`). Actions go through the unmodified OpenEnv `TextArenaEnv` client (`58d0344011fe961342706c90288f7e842b15b1c9`). The decision functions do not call `state()` and do not read `secret_word`. Success means the client ended the episode with reward at least 0.999. Mean reward is the client reward (0 on a non-terminal step, 1 on a win, otherwise the terminal partial score).

Held-out seeds are 10000..10039 (40 episodes). Training seeds start at 0 and do not overlap that range. Per-seed rows are in `docs/experiment/results.json`.

## Setup

- Host: AMD EPYC 7R32, 4 vCPU, 15 GiB RAM, NVIDIA A10G (driver 595.112.07)
- Client stack: Python 3.12.15, torch 2.15.0+cu132, transformers 5.22.2, huggingface_hub 2.4.0
- Language model: `HuggingFaceTB/SmolLM2-135M-Instruct`, snapshot `12fd25f77366fa6b3b4b768ec3050bf629380bac`, 134,515,008 parameters, float32. `config._commit_hash` was empty in the process; the hash is the Hugging Face cache ref `refs/main`.
- Device: CUDA was available. Weights and the REINFORCE backward ran on the A10G. Peak `torch.cuda.max_memory_allocated` was 2,732,257,792 bytes (2.54 GiB). Peak reserved was 3,166,699,520 bytes (2.95 GiB).
- Decoding: temperature 0.7, top_p 0.95, 24 new tokens. The reply is turned into one `[word]` when the text contains a bracketed or bare five-letter token.
- Linear policy: softmax over the 15,034-word list. Features are bias, consistent with the feedback, fraction of known greens matched, and whether the word is new. Greedy breaks ties by word-list order.
- Linear training reward (training only): `wordle.greens + 0.5 * wordle.yellows` from `observation.info.reward_signals` when either count is positive, otherwise the client step reward. One-step REINFORCE, learning rate 0.4, baseline EMA 0.1, 240 episodes (seeds 0..239).
- Language-model training: AdamW, learning rate 1e-5, grad clip 1.0, baseline EMA 0.1, terminal client reward, 32 episodes (seeds 0..31). Inference-only evaluation ran before any optimizer step.

```sh
export CARGO_TARGET_DIR=/mnt/scratch/h5i-openenv/target
export SENV_BIN="$CARGO_TARGET_DIR/release/senv"
export SENV_PYTHON=/mnt/scratch/h5i-openenv/venv/bin/python
export OPENENV_SRC=/mnt/scratch/h5i-openenv/src/OpenEnv
export EXPERIMENT_OUT=/mnt/scratch/h5i-openenv/experiment
export HF_HOME=/mnt/scratch/h5i-openenv/hf
bash scripts/wordle_experiment.sh
```

## Held-out seeds 10000..10039

| policy | mean reward | wins | mean steps | wall |
| --- | ---: | ---: | ---: | ---: |
| uniform random, ignores feedback | 0.165 | 0/40 | 6.00 | 0.1 s |
| linear softmax after training | 0.485 | 6/40 | 5.88 | 19.4 s |
| linear greedy after training | 0.855 | 23/40 | 5.80 | 17.4 s |
| SmolLM2-135M-Instruct, before the optimizer | 0.183 | 0/40 | 3.10 | (inside the 86.1 s LM block) |
| SmolLM2-135M-Instruct, after 32 REINFORCE episodes | 0.203 | 0/40 | 3.00 | (same block) |

Random played all 6 guesses in every episode (rewards from 0.0 to 0.6). The linear sample ended early on 4 episodes (steps 4 or 5) and won 6. The linear greedy won 23, including wins on the sixth guess; its lowest client reward was 0.4. Eight greedy episodes ended at step 5 and 32 ran to step 6.

The linear weights moved during training. Final weights: bias about 0, consistent 2.409, green_hits 6.683, novel 0.101. On the training seeds, the first 40 episodes had official mean reward 0.148 and 0 wins (shaped mean 0.905). The last 40 had official mean reward 0.298 and 1 win (shaped mean 1.483). Training wall time was 119.1 s. The held-out gap versus uniform random is the evidence that this policy improved. The training objective was the shaped green/yellow signal, not the win flag.

## Language model

The 32 training episodes won 0 times (mean client reward 0.278). The first 8 averaged 0.225 and the last 8 averaged 0.325, with 0 wins in both windows. Held-out wins stayed at 0, and the held-out mean moved from 0.183 to 0.203. That is not evidence that the language model learned Wordle.

Step counts show why. Before training, 34 of 40 held-out episodes lasted 3 steps, 5 lasted 4, and 1 lasted 2. After training, all 40 lasted 3 steps. Rewards were 0.0, 0.1, 0.2, 0.3, 0.4, or 0.5. A second consecutive invalid move ends the episode and keeps the previous guess's partial score. A separate look at seeds 10000 and 10001 with the same prompt and the pretrained weights (sampled, so not the scored actions) repeated one bracketed guess and then echoed feedback. The scored episodes were not action-logged. Their step histogram matches that early stop.

The LM block, including the weight load and both evaluations, took 86.1 s.

## Constrained language-model run

A second run scores dictionary words instead of letting the model emit free text. Each guess sent through `TextArenaEnv` is one of at most 32 words that match the feedback already in the observation and have not been guessed. The shortlist is a seeded sample when more words remain. The language model assigns a score to the token sequence `[word]`, and the policy is a softmax at temperature 1. Sampling during evaluation uses a padded batch of those scores. The training update uses the unpadded score of each candidate; a smoke check on four words agreed within 0.01. `scripts/wordle_lm.sh` is the command. Rows are in `docs/experiment/lm-results.json`.

Held-out seeds are again 10000..10039. Training seeds are 0..31. The model is the same SmolLM2-135M-Instruct snapshot `12fd25f77366fa6b3b4b768ec3050bf629380bac`, 134,515,008 parameters, float32, on the A10G. torch 2.15.0+cu132, transformers 5.22.2. Peak allocated memory was 6,027,527,680 bytes (5.61 GiB). Peak reserved was 22,227,714,048 bytes (20.70 GiB). Training is AdamW at learning rate 5e-5, grad clip 1.0 (the mean pre-clip grad norm was 20.3), baseline EMA 0.1, one-step REINFORCE on `wordle.greens + 0.5 * wordle.yellows` when that signal is present. Held-out numbers below are the client reward.

| policy | mean reward | wins | mean steps | wall |
| --- | ---: | ---: | ---: | ---: |
| uniform random, ignores feedback | 0.165 | 0/40 | 6.00 | 12.8 s |
| uniform draw on the 32-word shortlist | 0.910 | 26/40 | 5.20 | 21.1 s |
| SmolLM2 sample, before the optimizer | 0.923 | 30/40 | 5.28 | 35.9 s |
| SmolLM2 greedy, before the optimizer | 0.955 | 32/40 | 5.20 | 33.8 s |
| SmolLM2 sample, after 32 REINFORCE episodes | 0.935 | 31/40 | 5.10 | 33.8 s |
| SmolLM2 greedy, after 32 REINFORCE episodes | 0.950 | 33/40 | 5.03 | 33.5 s |

The uniform shortlist is not a language model. It wins 26 of 40 games, so most of the success is the feedback filter and the 32-word cap. Before any update, sampling the language model on that same shortlist wins 30, and the greedy scores win 32. Training took 324.2 s. On the training seeds the first 8 episodes won 7 (mean client reward 0.950) and the last 8 won 8 (mean 1.000), 30 wins in 32 episodes. On the held-out seeds the sample win count moved from 30 to 31 and the greedy win count from 32 to 33, while the greedy mean reward moved from 0.955 to 0.950. That shift is not clear evidence that the update learned a better Wordle policy. The pretrained gap over the uniform shortlist is the measured language-model effect in this run.

Guesses in this run are full games of dictionary words. The free-text section above is the earlier run, where repeated invalid replies ended episodes around step 3.
