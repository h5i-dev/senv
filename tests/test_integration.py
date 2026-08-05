"""Integration against the real `h5i orchestra serve` binary.

Skipped unless an h5i build with the bridge is found ($H5I, a sibling
../h5i/target/debug/h5i, or PATH). The cross-process test mirrors the Rust
acceptance harness (tests/orchestra_subprocess.rs): a scripted `sh`
subprocess plays each agent inside a real `h5i env shell`, so the whole
Python → bridge → box → host submit/review path runs for real — no LLM,
fully deterministic.
"""

from __future__ import annotations

import asyncio
import os
import shutil
import subprocess
from pathlib import Path

import pytest

from h5i.orchestra import Conductor, TurnContext, Verdict, patterns


def _find_h5i() -> str | None:
    candidates = [
        os.environ.get("H5I"),
        str(Path(__file__).resolve().parents[2] / "h5i" / "target" / "debug" / "h5i"),
        shutil.which("h5i"),
    ]
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            probe = subprocess.run(
                [candidate, "orchestra", "--help"], capture_output=True, timeout=30, check=False
            )
            if probe.returncode == 0:
                return candidate
    return None


H5I = _find_h5i()
pytestmark = pytest.mark.skipif(
    H5I is None, reason="no h5i binary with `orchestra serve` found"
)


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    root = tmp_path / "repo"
    root.mkdir()

    def git(*args: str) -> None:
        subprocess.run(["git", *args], cwd=root, check=True, capture_output=True)

    git("init", "-q")
    git("config", "user.name", "t")
    git("config", "user.email", "t@t")
    (root / "README.md").write_text("base\n")
    git("add", "README.md")
    git("commit", "-qm", "init")
    return root


def h5i_cli(repo: Path, *args: str) -> None:
    out = subprocess.run(
        [H5I, *args],
        cwd=repo,
        env={**os.environ, "H5I_AGENT": "human"},
        capture_output=True,
        text=True,
        check=False,
    )
    assert out.returncode == 0, f"h5i {args} failed: {out.stderr}"


async def test_step_journal_replays_across_sessions(repo: Path):
    """The durability kernel end-to-end: run a score twice; journaled steps
    (a step closure and a custom judge) replay without re-executing."""
    effect_runs = 0
    policy_runs = 0

    def effect():
        nonlocal effect_runs
        effect_runs += 1
        return {"value": 42}

    def policy(run) -> Verdict:
        nonlocal policy_runs
        policy_runs += 1
        return Verdict(method="int-test", decided_by="pytest",
                       reasons=("no candidates on purpose",))

    async def score() -> tuple[dict, Verdict, int]:
        async with Conductor(
            str(repo), "pyintegration", actor="human", h5i_bin=H5I,
            score_digest="test-digest",
        ) as c:
            result = await c.step("fetch", effect)
            await c.note("python was here")
            await c.freeze()
            verdict = await c.judge(policy)
            return result, verdict, c.replayed_steps

    result, verdict, replayed = await score()
    assert result == {"value": 42}
    assert verdict.method == "int-test"
    assert (effect_runs, policy_runs, replayed) == (1, 1, 0)

    # Second run of the same score: everything replays, nothing re-executes.
    result, verdict, replayed = await score()
    assert result == {"value": 42}
    assert verdict.method == "int-test"
    assert (effect_runs, policy_runs) == (1, 1)
    assert replayed >= 3  # fetch + freeze + judge

    # The recorded DAG is inspectable from Python.
    async with Conductor(
        str(repo), "pyintegration", actor="human", h5i_bin=H5I, score_digest=None
    ) as c:
        trace = await c.trace()
        assert "step fetch#1" in trace
        assert "python was here" in trace
        events = await c.events()
        assert any(e["kind"] == "orch_step" for e in events)


async def test_cross_process_ensemble(repo: Path):
    """Two scripted `sh` boxes play the agents (the orchestra_subprocess.rs
    scenario, driven from Python): work → freeze → mutual APPROVE reviews."""
    h5i_cli(repo, "team", "create", "sub")
    for i in (1, 2):
        h5i_cli(repo, "env", "create", f"w{i}", "--isolation", "workspace")
        h5i_cli(repo, "team", "add-env", "sub", f"env/human/w{i}",
                "--as", f"worker{i}", "--runtime", "claude")

    def box_shell(env_id: str, script: str) -> None:
        out = subprocess.run(
            [H5I, "env", "shell", env_id, "--", "sh", "-c", script],
            cwd=repo,
            env={**os.environ, "H5I_AGENT": "human", "H5I": H5I},
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )
        assert out.returncode == 0, f"box turn in {env_id} failed: {out.stderr}"

    async def on_turn(turn: TurnContext) -> None:
        if turn.kind in ("work", "revise"):
            script = (
                f"echo {turn.agent_id} > {turn.agent_id}.txt && "
                f"git add {turn.agent_id}.txt && git commit -qm work && "
                "printf 'candidate' > sum.txt && "
                '"$H5I" team agent submit --summary-file sum.txt'
            )
        elif turn.kind == "review":
            script = (
                "printf 'APPROVE looks good' > rev.txt && "
                f'"$H5I" team review submit --reviewer {turn.agent_id} '
                f"--target {turn.target} --file rev.txt"
            )
        else:
            return
        await asyncio.to_thread(box_shell, turn.env_id, script)

    async with Conductor(
        str(repo), "sub", actor="human", on_turn=on_turn,
        poll_interval=0.2, turn_timeout=90, h5i_bin=H5I, score_digest=None,
    ) as c:
        agents = await c.roster()
        assert sorted(a.id for a in agents) == ["worker1", "worker2"]
        outcome = await patterns.ensemble(c, "make a file", agents, rounds=1)

    assert len(outcome.artifacts) == 2, "both cross-process submissions observed"
    assert len(outcome.reviews) == 2
    assert all(patterns.approves(r) for r in outcome.reviews)
    assert outcome.rounds_run == 1  # full approval → single cycle
