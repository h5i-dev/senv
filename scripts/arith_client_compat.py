#!/usr/bin/env python3
"""Drive the unmodified TextArenaEnv client against `senv serve --env arith`.

The client is imported from the pinned OpenEnv tree. Arith-v0 is a custom senv
task on that socket, not an upstream OpenEnv environment and not TextArena.
The value used below is recomputed from the expression in the prompt. It is
not read from state().
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from arith_task import eval_expr, expression_from_prompt


def main() -> int:
    base_url = os.environ.get("SENV_BASE_URL")
    if not base_url:
        print("SENV_BASE_URL is required", file=sys.stderr)
        return 2

    import textarena_env.client as textarena_client
    from textarena_env import TextArenaEnv
    from textarena_env.models import TextArenaAction

    client_file = os.path.realpath(textarena_client.__file__)
    expected = os.environ.get("OPENENV_CLIENT")
    if expected and os.path.realpath(expected) != client_file:
        print(f"client path {client_file} != {expected}", file=sys.stderr)
        return 2
    print(f"client: {client_file}")

    left = TextArenaEnv(base_url=base_url).sync()
    right = TextArenaEnv(base_url=base_url).sync()
    with left, right:
            hidden = None
            for seed in range(50):
                reset = left.reset(seed=seed, episode_id=f"c{seed}")
                _expect(reset.done is False, f"reset done {reset.done}")
                _expect(reset.reward == 0.0, f"reset reward {reset.reward!r}")
                expr = expression_from_prompt(reset.observation.prompt)
                value = eval_expr(expr)
                rendered = str(value)
                state = left.state()
                _expect(state.env_id == "Arith-v0", f"env_id {state.env_id}")
                _expect(state.step_count == 0, f"steps {state.step_count}")
                blob = json.dumps(state.raw_state)
                _expect("value" not in state.raw_state, blob)
                _expect("answer" not in state.raw_state, blob)
                _expect("secret" not in state.raw_state, blob)
                if len(rendered) >= 2 and rendered not in reset.observation.prompt and rendered not in blob:
                    hidden = (seed, expr, value, rendered)
                    break
            _expect(hidden is not None, "no seed hid the value from the prompt and state")
            seed, expr, value, rendered = hidden

            wrong = left.step(TextArenaAction(message="answer 0" if value != 0 else "answer 1"))
            _expect(wrong.done is True, "wrong answer should finish")
            _expect(wrong.reward == 0.0, f"wrong reward {wrong.reward!r}")
            wrong_reply = _replies(wrong.observation.messages)[-1]
            _expect(wrong_reply == "Incorrect.", wrong_reply)
            _expect(rendered not in wrong_reply, f"wrong reply leaked {rendered}")
            wrong_state = left.state()
            _expect("value" not in wrong_state.raw_state, "value field after a miss")
            _expect(wrong_state.raw_state["last_reply"] == "Incorrect.", wrong_state.raw_state)

            reset = left.reset(seed=seed, episode_id="solve")
            _expect(expression_from_prompt(reset.observation.prompt) == expr, "seed changed the expression")
            calc = left.step(TextArenaAction(message="calc 8 - 3"))
            _expect(calc.done is False, "calc ended the episode")
            _expect(calc.reward == 0.0, f"calc reward {calc.reward!r}")
            _expect(calc.observation.info.get("arith.format_valid") is True, calc.observation.info)
            _expect(_replies(calc.observation.messages)[-1] == "Result: 5", _replies(calc.observation.messages))
            won = left.step(TextArenaAction(message=f"answer {value}"))
            _expect(won.done is True and won.reward == 1.0, f"win {won.done} {won.reward}")
            _expect(_replies(won.observation.messages)[-1] == "Correct.", _replies(won.observation.messages))
            _expect(won.observation.info.get("arith.success") is True, won.observation.info)
            solved = left.state()
            _expect(solved.raw_state.get("last_reply") == "Correct.", solved.raw_state)
            _expect("value" not in solved.raw_state, "value field after a hit")

            other = right.reset(seed=seed + 1, episode_id="other")
            _expect(other.reward == 0.0 and other.done is False, "right reset")
            _expect(expression_from_prompt(other.observation.prompt) != expr, "sessions shared an expression")
            right_state = right.state()
            _expect(right_state.step_count == 0, f"right steps {right_state.step_count}")
            _expect(right_state.episode_id == "other", right_state.episode_id)
    print("PASS official TextArenaEnv Arith-v0 reset/step/state, score, hidden value, isolation")
    return 0


def _replies(messages) -> list[str]:
    return [getattr(message, "content", "") for message in messages]


def _expect(condition: bool, detail: object) -> None:
    if not condition:
        print(f"FAIL {detail}", file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    raise SystemExit(main())
