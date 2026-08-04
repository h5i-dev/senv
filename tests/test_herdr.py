"""Unit tests for the herdr launcher and opener (``_herdr``)."""

import json
import stat

import pytest

from h5i.orchestra import HerdrLauncher
from h5i.orchestra._conductor import Conductor
from h5i.orchestra._errors import OrchestraError
from h5i.orchestra._herdr import (
    AGENT_BOOTSTRAP,
    HerdrClient,
    resident_command,
    resolve_herdr_bin,
    seat_label,
    shell_quote,
)
from h5i.orchestra._types import TurnContext
from h5i.orchestra._watch import Opener, SessionWatcher, resolve_opener, session_prefix


def turn(**overrides) -> TurnContext:
    raw = {
        "run_id": "r1",
        "agent_id": "claude",
        "env_id": "env/claude/r1-claude",
        "kind": "work",
        "instruction": "task",
        "repo_workdir": "/repo",
        "h5i_root": "/repo/.git/.h5i",
        "runtime": "claude",
    }
    raw.update(overrides)
    return TurnContext.from_raw(raw)


# ── the resident command (port of the Rust launcher's adapter argv) ──────────


def test_claude_resident_command():
    cmd = resident_command(turn(), env={})
    assert cmd == (
        "'h5i' env shell env/claude/r1-claude -- "
        f"claude --dangerously-skip-permissions {shell_quote(AGENT_BOOTSTRAP)}"
    )


def test_codex_resident_command_with_model_and_effort():
    cmd = resident_command(
        turn(agent_id="codex", runtime="codex", model="gpt-5.4-mini", effort="high"),
        env={},
    )
    assert "codex --sandbox danger-full-access" in cmd
    assert " --model 'gpt-5.4-mini'" in cmd
    assert " -c model_reasoning_effort='high'" in cmd


def test_claude_model_flag():
    assert " --model 'claude-haiku-4-5'" in resident_command(
        turn(model="claude-haiku-4-5"), env={}
    )


def test_claude_rejects_effort():
    with pytest.raises(OrchestraError, match="no reasoning-effort"):
        resident_command(turn(effort="high"), env={})


def test_unknown_runtime_fails_closed():
    with pytest.raises(OrchestraError, match="no adapter for runtime 'pi'"):
        resident_command(turn(runtime="pi"), env={})


def test_missing_runtime_fails_closed():
    with pytest.raises(OrchestraError, match="no roster runtime"):
        resident_command(turn(runtime=None), env={})


def test_h5i_env_var_overrides_binary():
    cmd = resident_command(turn(), env={"H5I": "/opt/h5i-dev"})
    assert cmd.startswith("'/opt/h5i-dev' env shell")


def test_explicit_h5i_bin_wins_over_env():
    cmd = resident_command(turn(), h5i_bin="/x/h5i", env={"H5I": "/y/h5i"})
    assert cmd.startswith("'/x/h5i' env shell")


def test_seat_label_matches_tmux_session_naming():
    assert seat_label("r1", "claude") == session_prefix("r1") + "claude"


def test_shell_quote_embedded_single_quote():
    assert shell_quote("it's") == "'it'\\''s'"


# ── HerdrClient over a fake herdr binary ─────────────────────────────────────


def fake_herdr(tmp_path, body: str) -> str:
    """Install an executable fake ``herdr`` and return its path."""
    path = tmp_path / "herdr"
    path.write_text(f"#!/bin/sh\n{body}\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return str(path)


SPLIT_RESPONSE = {
    "id": "cli:pane:split",
    "result": {"type": "pane_info", "pane": {"pane_id": "w1:p7", "agent_status": "unknown"}},
}


@pytest.mark.asyncio
async def test_client_split_parses_pane_record(tmp_path):
    log = tmp_path / "argv.log"
    bin_path = fake_herdr(
        tmp_path,
        f"echo \"$@\" >> {log}\necho '{json.dumps(SPLIT_RESPONSE)}'",
    )
    pane = await HerdrClient(bin_path).split(
        pane="w1:p1", direction="right", cwd="/repo"
    )
    assert pane["pane_id"] == "w1:p7"
    assert log.read_text().strip() == (
        "pane split --pane w1:p1 --direction right --no-focus --cwd /repo"
    )


@pytest.mark.asyncio
async def test_client_error_exit_raises(tmp_path):
    bin_path = fake_herdr(
        tmp_path, "echo '{\"error\":{\"message\":\"no such pane\"}}' >&2\nexit 1"
    )
    with pytest.raises(OrchestraError, match="no such pane"):
        await HerdrClient(bin_path).rename("w1:p9", "x")


@pytest.mark.asyncio
async def test_client_get_returns_none_for_missing_pane(tmp_path):
    bin_path = fake_herdr(tmp_path, "exit 1")
    assert await HerdrClient(bin_path).get("w1:p9") is None


@pytest.mark.asyncio
async def test_client_non_json_output_raises(tmp_path):
    bin_path = fake_herdr(tmp_path, "echo not-json")
    with pytest.raises(OrchestraError, match="non-JSON"):
        await HerdrClient(bin_path).list_panes()


def test_resolve_herdr_bin_precedence():
    assert resolve_herdr_bin("/x/herdr", env={"HERDR_BIN_PATH": "/y"}) == "/x/herdr"
    assert resolve_herdr_bin(env={"HERDR_BIN_PATH": "/y"}) == "/y"
    assert resolve_herdr_bin(env={}, which=lambda n: "/usr/bin/herdr") == "/usr/bin/herdr"
    with pytest.raises(OrchestraError, match="herdr binary"):
        resolve_herdr_bin(env={}, which=lambda n: None)


# ── the launcher flow (scripted client, no subprocesses) ─────────────────────


class FakeClient:
    """Scripted HerdrClient: records calls, panes live in a dict."""

    def __init__(self):
        self.calls: list[tuple] = []
        self.panes: dict[str, dict] = {}
        self._next = 0

    async def split(self, *, pane=None, direction="right", cwd=None):
        self._next += 1
        pane_id = f"w1:p{self._next}"
        self.calls.append(("split", pane, direction))
        self.panes[pane_id] = {"pane_id": pane_id}
        return self.panes[pane_id]

    async def rename(self, pane_id, label):
        self.calls.append(("rename", pane_id, label))
        self.panes[pane_id]["label"] = label

    async def run(self, pane_id, command):
        self.calls.append(("run", pane_id, command))

    async def get(self, pane_id):
        self.calls.append(("get", pane_id))
        return self.panes.get(pane_id)

    async def list_panes(self, *, workspace=None):
        self.calls.append(("list", workspace))
        return list(self.panes.values())


def launcher(env=None) -> tuple[HerdrLauncher, FakeClient]:
    ln = HerdrLauncher(env=env or {}, echo=lambda line: None)
    fake = FakeClient()
    ln._client = fake
    return ln, fake


@pytest.mark.asyncio
async def test_first_turn_splits_renames_and_runs():
    ln, fake = launcher(env={"HERDR_PANE_ID": "w1:p0"})
    await ln.on_turn(turn())
    kinds = [c[0] for c in fake.calls]
    assert kinds == ["list", "split", "rename", "run"]
    assert ("split", "w1:p0", "right") in fake.calls
    assert fake.panes["w1:p1"]["label"] == seat_label("r1", "claude")
    run_call = next(c for c in fake.calls if c[0] == "run")
    assert run_call[2].endswith(f"-- claude --dangerously-skip-permissions {shell_quote(AGENT_BOOTSTRAP)}")


@pytest.mark.asyncio
async def test_live_seat_is_not_recreated():
    ln, fake = launcher()
    await ln.on_turn(turn())
    fake.panes["w1:p1"]["agent"] = "claude"  # herdr detected the session
    fake.calls.clear()
    await ln.on_turn(turn())
    assert [c[0] for c in fake.calls] == ["get"]


@pytest.mark.asyncio
async def test_booting_seat_is_left_alone():
    ln, fake = launcher()
    await ln.on_turn(turn())  # started by us; herdr hasn't detected an agent yet
    fake.calls.clear()
    await ln.on_turn(turn())
    assert [c[0] for c in fake.calls] == ["get"]  # no restart, no re-split


@pytest.mark.asyncio
async def test_second_agent_grows_a_column_down():
    ln, fake = launcher(env={"HERDR_PANE_ID": "w1:p0"})
    await ln.on_turn(turn())
    await ln.on_turn(turn(agent_id="codex", runtime="codex", env_id="env/codex/r1-codex"))
    splits = [c for c in fake.calls if c[0] == "split"]
    assert splits == [("split", "w1:p0", "right"), ("split", "w1:p1", "down")]


@pytest.mark.asyncio
async def test_vanished_pane_is_resplit():
    ln, fake = launcher()
    await ln.on_turn(turn())
    del fake.panes["w1:p1"]  # the pane was closed
    await ln.on_turn(turn())
    assert len([c for c in fake.calls if c[0] == "split"]) == 2
    # ...and the dead pane was not used as the anchor for the new split.
    assert fake.calls[-3] == ("split", None, "right")


@pytest.mark.asyncio
async def test_seat_rediscovered_by_label_after_restart():
    l1, fake = launcher()
    await l1.on_turn(turn())
    fake.panes["w1:p1"]["agent"] = "claude"
    # A fresh launcher (score re-run) with the same herdr state: adopt, don't split.
    l2 = HerdrLauncher(env={}, echo=lambda line: None)
    l2._client = fake
    fake.calls.clear()
    await l2.on_turn(turn())
    assert not any(c[0] == "split" for c in fake.calls)


@pytest.mark.asyncio
async def test_adopted_dead_seat_is_restarted_in_place():
    l1, fake = launcher()
    await l1.on_turn(turn())
    # Session died: pane still there, no agent. A fresh launcher restarts it.
    l2 = HerdrLauncher(env={}, echo=lambda line: None)
    l2._client = fake
    fake.calls.clear()
    await l2.on_turn(turn())
    assert not any(c[0] == "split" for c in fake.calls)
    assert any(c[0] == "run" for c in fake.calls)


@pytest.mark.asyncio
async def test_session_dying_mid_run_is_restarted():
    ln, fake = launcher()
    await ln.on_turn(turn())
    fake.panes["w1:p1"]["agent"] = "claude"
    await ln.on_turn(turn())          # observed up
    fake.panes["w1:p1"].pop("agent")  # ...then the session died
    fake.calls.clear()
    await ln.on_turn(turn())
    assert any(c[0] == "run" for c in fake.calls)


# ── opener resolution & the watcher's herdr viewer ───────────────────────────


def _herdr_only(name: str):
    return "/usr/bin/herdr" if name == "herdr" else None


def test_inside_herdr_uses_herdr_panes():
    opener = resolve_opener(env={"HERDR_ENV": "1"}, which=_herdr_only)
    assert opener.kind == "herdr"


def test_tmux_wins_over_herdr():
    opener = resolve_opener(
        env={"HERDR_ENV": "1", "TMUX": "sock,1,0"}, which=_herdr_only
    )
    assert opener.kind == "tmux-link"


def test_herdr_env_without_binary_falls_through():
    assert resolve_opener(env={"HERDR_ENV": "1"}, which=lambda n: None).kind == "hint"


@pytest.mark.asyncio
async def test_watcher_opens_viewer_panes_in_a_column():
    session_a = session_prefix("run1") + "claude"
    session_b = session_prefix("run1") + "codex"

    class HerdrWatcher(SessionWatcher):
        async def _list_sessions(self):
            return [session_a, session_b]

    echoed: list[str] = []
    w = HerdrWatcher(
        "run1",
        opener=Opener("herdr pane", "herdr"),
        echo=echoed.append,
        spawn_gap=0.0,
    )
    fake = FakeClient()
    w._herdr_client = fake
    assert await w.poll_once()
    assert [c for c in fake.calls if c[0] == "split"] == [
        ("split", None, "right"),
        ("split", "w1:p1", "down"),
    ]
    runs = [c for c in fake.calls if c[0] == "run"]
    assert runs[0][2] == f"tmux attach-session -t {session_a}"
    assert any("opened in herdr pane w1:p1" in line for line in echoed)


@pytest.mark.asyncio
async def test_broken_herdr_viewer_degrades_to_hint():
    session = session_prefix("run1") + "claude"

    class Broken(SessionWatcher):
        async def _list_sessions(self):
            return [session]

        async def _open_herdr_pane(self, session):
            raise OrchestraError("herdr pane split failed")

    echoed: list[str] = []
    w = Broken("run1", opener=Opener("herdr pane", "herdr"), echo=echoed.append,
               spawn_gap=0.0)
    assert await w.poll_once()
    assert any(f"tmux attach -t {session}" in line for line in echoed)


# ── Conductor wiring ─────────────────────────────────────────────────────────


def test_launcher_herdr_maps_to_client_mode():
    c = Conductor(".", "r", launcher="herdr")
    assert c._launcher == "client"
    assert isinstance(c._on_turn, HerdrLauncher)
    assert c._watch is False  # seats are already visible in herdr


def test_launcher_herdr_rejects_on_turn():
    with pytest.raises(TypeError, match='implies launcher="client"'):
        Conductor(".", "r", launcher="herdr", on_turn=lambda t: None)


@pytest.mark.asyncio
async def test_turn_is_served_by_the_herdr_launcher():
    from mock_server import MockOrchestra, launch_conductor

    mock = MockOrchestra()
    c = await launch_conductor(mock, launcher="herdr")
    try:
        fake = FakeClient()
        c._on_turn._client = fake
        reply = await mock.request(
            "launcher.on_turn",
            {
                "run_id": "testrun",
                "agent_id": "claude",
                "env_id": "env/claude/t",
                "kind": "work",
                "instruction": "task",
                "repo_workdir": "/repo",
                "h5i_root": "/repo/.git/.h5i",
                "runtime": "claude",
            },
        )
        assert reply.get("result") == {}
        assert any(call[0] == "split" for call in fake.calls)
        assert mock.calls_to("conductor.launch")[0]["launcher"] == "client"
    finally:
        await c.close()


@pytest.mark.asyncio
async def test_herdr_launcher_failure_answers_the_engine_with_an_error():
    from mock_server import MockOrchestra, launch_conductor

    mock = MockOrchestra()
    c = await launch_conductor(mock, launcher="herdr")
    try:
        c._on_turn._client = FakeClient()
        reply = await mock.request(
            "launcher.on_turn",
            {"run_id": "testrun", "agent_id": "pi", "env_id": "e",
             "repo_workdir": "/repo", "runtime": "pi"},
        )
        assert "no adapter for runtime 'pi'" in reply["error"]["message"]
    finally:
        await c.close()
