#!/usr/bin/env python3
"""Drive the unmodified TextArenaEnv client against `senv serve --env wordle`.

The client is imported from the pinned OpenEnv tree. This file does not vendor
or patch it. Wordle here is the senv game on that client's WebSocket contract,
not the TextArena package and not NLTK.
"""

from __future__ import annotations

import os
import re
import sys

_FEEDBACK = re.compile(r"You submitted \[(\w+)\]\.\nFeedback:\n[^\n]*\n([GYX ]+)")
_WIN = re.compile(r"\[(\w+)\]\n\[GAME\] Congratulations! You guessed the word correctly!")


def main() -> int:
    base_url = os.environ.get("SENV_BASE_URL")
    if not base_url:
        print("SENV_BASE_URL is required", file=sys.stderr)
        return 2

    import textarena_env.client as textarena_client
    from textarena_env.models import TextArenaAction
    from textarena_env import TextArenaEnv

    client_file = os.path.realpath(textarena_client.__file__)
    expected = os.environ.get("OPENENV_CLIENT")
    if expected and os.path.realpath(expected) != client_file:
        print(f"client path {client_file} != {expected}", file=sys.stderr)
        return 2
    print(f"client: {client_file}")

    left = TextArenaEnv(base_url=base_url).sync()
    right = TextArenaEnv(base_url=base_url).sync()
    try:
        with left, right:
            reset = left.reset(seed=7, episode_id="wl")
            _expect(reset.done is False, f"reset done: {reset.done}")
            _expect(reset.reward == 0.0, f"reset reward: {reset.reward!r}")
            prompt = reset.observation.prompt
            _expect("You are Playing Wordle." in prompt, f"prompt: {prompt!r}")
            _expect("You have 6 attempts" in prompt, "prompt missing attempts")

            state = left.state()
            _expect(state.episode_id == "wl", f"episode: {state.episode_id}")
            _expect(state.step_count == 0, f"step_count: {state.step_count}")
            _expect(state.env_id == "Wordle-v0", f"env_id: {state.env_id}")
            secret = state.raw_state["game_state"]["secret_word"]
            _expect(isinstance(secret, str) and len(secret) == 5, f"secret: {secret!r}")

            other = "apple" if secret != "apple" else "crane"
            guess = left.step(TextArenaAction(message=f"[{other}]"))
            _expect(guess.done is False, f"guess done: {guess.done}")
            _expect(guess.reward == 0.0, f"guess reward: {guess.reward!r}")
            text = _text(guess.observation.messages)
            _expect(_FEEDBACK.search(text) is not None, f"feedback text: {text!r}")
            signals = guess.observation.info.get("reward_signals", {})
            _expect("wordle.greens" in signals, f"signals: {signals!r}")
            _expect(signals["wordle.repetitions"] == 1.0, f"repetitions: {signals!r}")

            won = left.step(TextArenaAction(message=f"[{secret}]"))
            _expect(won.done is True, f"win done: {won.done}")
            _expect(won.reward == 1.0, f"win reward: {won.reward!r}")
            won_text = _text(won.observation.messages)
            _expect(_WIN.search(won_text) is not None, f"win text: {won_text!r}")
            after = left.state()
            _expect(after.step_count == 2, f"after step_count: {after.step_count}")
            _expect(after.turn == 2, f"after turn: {after.turn}")

            right_reset = right.reset(seed=8, episode_id="wr")
            _expect(right_reset.reward == 0.0, "right reset reward")
            right_state = right.state()
            _expect(right_state.step_count == 0, f"right steps: {right_state.step_count}")
            _expect(right_state.episode_id == "wr", f"right episode: {right_state.episode_id}")
            right_secret = right_state.raw_state["game_state"]["secret_word"]
            _expect(right_secret != secret, "seed 7 and seed 8 picked the same secret")
    finally:
        _close(left)
        _close(right)

    again = TextArenaEnv(base_url=base_url).sync()
    other = TextArenaEnv(base_url=base_url).sync()
    try:
        with again, other:
            again.reset(seed=7, episode_id="same-a")
            other.reset(seed=7, episode_id="same-b")
            a = again.state().raw_state["game_state"]["secret_word"]
            b = other.state().raw_state["game_state"]["secret_word"]
            _expect(a == b, f"seed stability: {a} != {b}")

            first = again.step(TextArenaAction(message="no brackets"))
            _expect(first.done is False, f"first invalid done: {first.done}")
            _expect("invalid move" in _text(first.observation.messages), "missing invalid text")
            _expect(again.state().turn == 0, "invalid consumed a turn")
            second = again.step(TextArenaAction(message="still bad"))
            _expect(second.done is True, f"second invalid done: {second.done}")
            _expect(second.reward == 0.0, f"second invalid reward: {second.reward!r}")
            _expect(other.state().step_count == 0, "invalid session touched the other client")
    finally:
        _close(again)
        _close(other)

    print("PASS official TextArenaEnv Wordle reset/step/state, feedback, win, invalid, isolation")
    return 0


def _text(messages: list) -> str:
    return "".join(message.content for message in messages)


def _expect(condition: bool, detail: str) -> None:
    if not condition:
        raise SystemExit(f"FAIL {detail}")


def _close(client: object) -> None:
    close = getattr(client, "close", None)
    if close is None:
        return
    try:
        close()
    except Exception:
        pass


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception as error:
        print(f"FAIL {type(error).__name__}: {error}", file=sys.stderr)
        raise
