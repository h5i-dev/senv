#!/usr/bin/env python3
"""Drive the unmodified official EchoEnv client against a running senv server.

The client is imported from the pinned OpenEnv tree (PYTHONPATH). This file
does not vendor or patch that client. It checks reset(), step(), and state(),
plus a second client for session isolation and a fresh client after close.
"""

from __future__ import annotations

import os
import sys


def main() -> int:
    base_url = os.environ.get("SENV_BASE_URL")
    if not base_url:
        print("SENV_BASE_URL is required", file=sys.stderr)
        return 2

    import echo_env.client as echo_client
    from echo_env import CallToolAction, EchoEnv

    client_file = os.path.realpath(echo_client.__file__)
    expected = os.environ.get("OPENENV_CLIENT")
    if expected and os.path.realpath(expected) != client_file:
        print(f"client path {client_file} != {expected}", file=sys.stderr)
        return 2
    print(f"client: {client_file}")

    left = EchoEnv(base_url=base_url).sync()
    right = EchoEnv(base_url=base_url).sync()
    try:
        with left, right:
            reset = left.reset(episode_id="py-left", seed=1)
            _expect(reset.done is False, f"reset done: {reset.done}")
            _expect(reset.reward == 0.0, f"reset reward: {reset.reward!r}")
            message = reset.observation.metadata.get("message")
            _expect(message == "Echo environment ready!", f"reset message: {message!r}")
            _expect(
                reset.metadata is not None and reset.metadata.get("message") == message,
                f"reset top-level metadata: {reset.metadata!r}",
            )

            stepped = left.step(
                CallToolAction(tool_name="echo_message", arguments={"message": "Hello!"})
            )
            _expect(stepped.done is False, f"step done: {stepped.done}")
            _expect(stepped.reward is None, f"step reward: {stepped.reward!r}")
            data = stepped.observation.result["data"]
            _expect(data == "Hello!", f"echo_message data: {data!r}")

            length = left.step(
                CallToolAction(
                    tool_name="echo_with_length",
                    arguments={"message": "héllo"},
                )
            )
            length_data = length.observation.result["data"]
            _expect(
                length_data == {"message": "héllo", "length": 5},
                f"echo_with_length data: {length_data!r}",
            )

            state = left.state()
            _expect(state.episode_id == "py-left", f"episode: {state.episode_id!r}")
            _expect(state.step_count == 2, f"step_count: {state.step_count}")

            # Production list_tools/call_tool use HTTP /mcp and must not count as steps.
            names = [tool.name for tool in left.list_tools()]
            _expect(
                names == ["echo_message", "echo_with_length"],
                f"tools: {names!r}",
            )
            via_http = left.call_tool("echo_message", message="via-http")
            _expect(via_http == "via-http", f"call_tool: {via_http!r}")
            after_http = left.state()
            _expect(after_http.step_count == 2, f"step_count after HTTP tool: {after_http.step_count}")

            other = right.reset(episode_id="py-right")
            _expect(
                other.observation.metadata.get("message") == "Echo environment ready!",
                "right reset failed",
            )
            right_state = right.state()
            _expect(right_state.episode_id == "py-right", f"right episode: {right_state.episode_id!r}")
            _expect(right_state.step_count == 0, f"right step_count: {right_state.step_count}")
            left_again = left.state()
            _expect(left_again.step_count == 2, f"left changed: {left_again.step_count}")
            _expect(left_again.episode_id == "py-left", "left episode changed")
    finally:
        _close(left)
        _close(right)

    # The closed socket destroys its session. A new client is a new episode.
    again = EchoEnv(base_url=base_url).sync()
    try:
        with again:
            fresh = again.reset()
            _expect(fresh.reward == 0.0, f"reconnect reward: {fresh.reward!r}")
            fresh_state = again.state()
            _expect(fresh_state.step_count == 0, f"reconnect step_count: {fresh_state.step_count}")
            _expect(fresh_state.episode_id != "py-left", "reconnect reused py-left")
            echoed = again.step(
                CallToolAction(tool_name="echo_message", arguments={"message": "again"})
            )
            _expect(echoed.observation.result["data"] == "again", "reconnect step failed")
            _expect(again.state().step_count == 1, f"reconnect after step: {again.state().step_count}")
    finally:
        _close(again)

    print("PASS official EchoEnv reset/step/state, isolation, reconnect")
    return 0


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
