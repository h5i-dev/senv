"""The Conductor and Agent handles — the define-by-run surface.

A score is an ordinary async Python program. There is no graph builder and no
``compile()`` step: ``if``, ``for`` and ``asyncio.gather`` *are* the
orchestration language, and the DAG is whatever the journal recorded. Every
effectful operation is journaled on the git-backed team event log by the
`h5i orchestra serve` child this class drives, so a killed score resumes
without re-running completed agent turns — just run the same file again.

    async with Conductor(".", "fix-auth") as c:
        claude = await c.hire("claude", runtime="claude")
        codex = await c.hire("codex", runtime="codex")
        a, b = await asyncio.gather(claude.work(task), codex.work(task))
        await c.freeze()
        ra, rb = await asyncio.gather(codex.review(a), claude.review(b))
        await c.verify(a, ["cargo", "test", "--quiet"])
        verdict = await c.judge()
"""

from __future__ import annotations

import asyncio
import hashlib
import inspect
import sys
from collections.abc import Awaitable, Callable, Iterable, Sequence
from pathlib import Path
from typing import Any, Self

from . import policy as _policy
from ._errors import BridgeClosedError, OrchestraError, ProtocolError
from ._rpc import Bridge, resolve_h5i_bin
from ._types import (
    ApplyResult,
    Artifact,
    CompareRow,
    GateAnswer,
    Review,
    Run,
    TurnContext,
    Verdict,
    Verification,
)
from ._watch import SessionWatcher

__all__ = ["PROTOCOL_VERSION", "Agent", "Conductor", "Scope"]

PROTOCOL_VERSION = 1
_SDK_VERSION = "0.1.0"

#: sentinel — hash the score file (``sys.argv[0]``) as the run's provenance.
_AUTO = object()

OnTurn = Callable[[TurnContext], "Awaitable[None] | None"]
Parse = Callable[[Any], Any]


def _score_digest_auto() -> str | None:
    """sha256 of the entry-point script — the SDK analog of the Rust eDSL
    hashing the score binary, so ``h5i team trace`` provenance points at the
    Python file that actually drove the run."""
    try:
        entry = Path(sys.argv[0])
        if entry.is_file():
            return hashlib.sha256(entry.read_bytes()).hexdigest()
    except OSError:
        pass
    return None


class Conductor:
    """The handle a score drives its run through.

    Launching creates the team run if it does not exist and resumes it
    (replaying the journal) if it does. Use as an async context manager, or
    call :meth:`launch` / :meth:`close` yourself.

    Parameters mirror the Rust ``ConductorBuilder``:

    - ``launcher``: ``"attach"`` (default — resident sessions pick turns out
      of their inboxes), ``"resident"`` (the bridge brings tmux sessions up
      itself), ``"herdr"`` (this process brings each seat up as a pane in a
      running `herdr <https://herdr.dev>`_ session — visible in herdr's
      sidebar with per-agent status, no tmux or viewer terminals needed), or
      ``"client"`` (every turn is delivered to ``on_turn`` in *this*
      process — how tests script agents, and how a score can spawn its own
      runtimes). Passing ``on_turn`` implies ``"client"``.
    - ``isolation``: the run's default sandbox tier for hired agents' envs
      (``"workspace"``, ``"process"``, ``"supervised"``, ``"container"``, …).
      Every :meth:`hire` inherits it unless it passes its own. An explicit
      tier is fail-closed — hire errors if the host cannot enforce it, never
      silently downgrades; ``None`` auto-picks per env, like
      ``h5i env create``.
    - ``turn_timeout``/``poll_interval``: seconds (floats fine).
    - ``score_digest``: provenance digest recorded at launch. Defaults to the
      sha256 of ``sys.argv[0]``; pass ``None`` to record nothing, or your own
      string.
    - ``watch``: auto-open a viewer on each agent's resident tmux session as
      it comes up, instead of hunting for ``tmux attach -t …`` by hand.
      ``True`` picks the best available surface (a window linked into your
      current tmux session, a Windows Terminal tab under WSL, or a GUI
      terminal); a string is a command template, e.g.
      ``watch="kitty -e tmux attach -t {session}"``. Defaults to on for
      ``launcher="resident"`` (set ``watch=False`` to silence); viewer
      failures never fail the score — they degrade to a printed attach hint.
    """

    def __init__(
        self,
        repo: str = ".",
        run: str | None = None,
        *,
        title: str | None = None,
        base: str | None = None,
        max_rounds: int | None = None,
        actor: str | None = None,
        launcher: str | None = None,
        isolation: str | None = None,
        on_turn: OnTurn | None = None,
        poll_interval: float | None = None,
        turn_timeout: float | None = None,
        score_digest: Any = _AUTO,
        h5i_bin: str | None = None,
        watch: bool | str | None = None,
    ):
        if not run:
            raise TypeError("Conductor(...) requires a run id: Conductor(repo, run)")
        if launcher == "client" and on_turn is None:
            raise TypeError('launcher="client" requires on_turn=...')
        if on_turn is not None and launcher not in (None, "client"):
            raise TypeError(f'on_turn=... implies launcher="client", not {launcher!r}')
        if launcher == "herdr":
            # The herdr launcher is client-mode with a built-in turn handler:
            # the engine dispatches `launcher.on_turn` here, and we make sure
            # the seat's pane is up (see `_herdr.HerdrLauncher`).
            from ._herdr import HerdrLauncher

            on_turn = HerdrLauncher(h5i_bin=h5i_bin)
            launcher = "client"
        self._repo = str(Path(repo).resolve())
        self._run = run
        self._title = title
        self._base = base
        self._max_rounds = max_rounds
        self._actor = actor
        self._launcher = "client" if on_turn is not None else (launcher or "attach")
        self._isolation = isolation
        self._on_turn = on_turn
        self._poll_interval = poll_interval
        self._turn_timeout = turn_timeout
        self._score_digest = score_digest
        self._h5i_bin = h5i_bin
        self._watch = self._launcher == "resident" if watch is None else watch
        self._watch_task: asyncio.Task | None = None
        self._session_watcher: SessionWatcher | None = None
        self._bridge: Bridge | None = None
        self._run_id: str | None = None
        self._actor_resolved: str | None = None
        self._replayed_steps = 0
        self.h5i_version: str | None = None
        self.capabilities: tuple[str, ...] = ()

    # ── lifecycle ───────────────────────────────────────────────────────────

    async def __aenter__(self) -> Self:
        await self.launch()
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()

    async def _spawn_bridge(self) -> Bridge:
        """Bring the transport up — the seam tests (and embedders) override
        to speak the same protocol over something other than a subprocess."""
        argv = [resolve_h5i_bin(self._h5i_bin), "orchestra", "serve"]
        return await Bridge.spawn(argv, cwd=self._repo, on_request=self._serve_request)

    async def launch(self) -> Conductor:
        """Spawn the bridge, shake hands, and open (or resume) the run."""
        if self._bridge is not None:
            return self
        bridge = await self._spawn_bridge()
        try:
            try:
                hello = await bridge.request(
                    "initialize",
                    {
                        "protocol_version": PROTOCOL_VERSION,
                        "client": "h5i.orchestra (python)",
                        "client_version": _SDK_VERSION,
                    },
                )
            except BridgeClosedError as e:
                raise BridgeClosedError(
                    f"{e} — is this h5i build too old to have `h5i orchestra serve`?"
                ) from e
            if hello.get("protocol_version") != PROTOCOL_VERSION:
                raise ProtocolError(
                    f"server speaks protocol {hello.get('protocol_version')}, "
                    f"this SDK speaks {PROTOCOL_VERSION}"
                )
            self.h5i_version = hello.get("h5i_version")
            self.capabilities = tuple(hello.get("capabilities") or ())

            params: dict[str, Any] = {"repo": self._repo, "run": self._run}
            if self._title is not None:
                params["title"] = self._title
            if self._base is not None:
                params["base"] = self._base
            if self._max_rounds is not None:
                params["max_rounds"] = self._max_rounds
            if self._actor is not None:
                params["actor"] = self._actor
            params["launcher"] = self._launcher
            if self._poll_interval is not None:
                params["poll_interval_ms"] = int(self._poll_interval * 1000)
            if self._turn_timeout is not None:
                params["turn_timeout_ms"] = int(self._turn_timeout * 1000)
            digest = (
                _score_digest_auto() if self._score_digest is _AUTO else self._score_digest
            )
            if digest is not None:
                params["score_digest"] = digest
            launched = await bridge.request("conductor.launch", params)
        except BaseException:
            await bridge.notify_close(graceful_timeout=1.0)
            raise
        self._bridge = bridge
        self._run_id = launched.get("run_id")
        self._actor_resolved = launched.get("actor")
        self._replayed_steps = int(launched.get("replayed_steps") or 0)
        if self._watch and self._run_id:
            self._session_watcher = SessionWatcher(
                self._run_id,
                template=self._watch if isinstance(self._watch, str) else None,
            )
            self._watch_task = asyncio.ensure_future(self._session_watcher.run())
        return self

    async def close(self) -> None:
        """Shut the bridge down. The run's journal stays durable — re-running
        the score resumes it."""
        watch_task, self._watch_task = self._watch_task, None
        if watch_task is not None:
            watch_task.cancel()
            try:
                await watch_task
            except asyncio.CancelledError:
                pass  # a viewer must never mask the real shutdown path
        bridge, self._bridge = self._bridge, None
        if bridge is not None:
            await bridge.notify_close()

    @property
    def run_id(self) -> str:
        if self._run_id is None:
            raise OrchestraError("conductor is not launched")
        return self._run_id

    @property
    def actor(self) -> str | None:
        return self._actor_resolved

    @property
    def replayed_steps(self) -> int:
        """Journaled steps loaded at launch — what a resume will replay."""
        return self._replayed_steps

    # ── agents ──────────────────────────────────────────────────────────────

    async def hire(
        self,
        name: str,
        *,
        runtime: str | None = None,
        model: str | None = None,
        effort: str | None = None,
        profile: str | None = None,
        isolation: str | None = None,
        env: str | None = None,
    ) -> Agent:
        """Hire an agent into the run: create (or bind ``env``) its sandboxed
        env and enroll it on the roster. Journaled — a resume rebinds.

        ``effort`` records a reasoning-effort override on the roster seat
        (codex sessions launch with ``-c model_reasoning_effort=<effort>``,
        which wins over the box's ``config.toml``). Runtimes without an
        effort knob fail closed at launch rather than silently ignoring it.

        ``isolation`` requests a sandbox tier for the created env
        (``"workspace"``, ``"process"``, ``"supervised"``, ``"container"``, …),
        defaulting to the conductor's run-level ``isolation``. An explicit
        tier is fail-closed — hire errors if the host cannot enforce it,
        never silently downgrades; pass ``"auto"`` to override a run-level
        tier back to auto-picking.
        """
        if isolation is None:
            isolation = self._isolation
        params: dict[str, Any] = {"name": name}
        if runtime is not None:
            params["runtime"] = runtime
        if model is not None:
            params["model"] = model
        if effort is not None:
            params["effort"] = effort
        if profile is not None:
            params["profile"] = profile
        if isolation is not None:
            params["isolation"] = isolation
        if env is not None:
            params["env"] = env
        seat = await self._request("agent.hire", params)
        return Agent(self, seat["agent_id"], seat["env_id"])

    async def roster(self) -> list[Agent]:
        """Bind every enrolled roster seat — how a driver picks up a team
        whose agents were enrolled elsewhere (not journaled)."""
        seats = await self._request("conductor.roster", {})
        return [Agent(self, s["agent_id"], s["env_id"]) for s in seats]

    # ── run state (reads; never journaled) ──────────────────────────────────

    async def status(self) -> Run:
        return Run.from_raw(await self._request("conductor.status", {}))

    async def events(self) -> list[dict]:
        """The raw event log — the audit trail the journal lives on."""
        return await self._request("conductor.events", {})

    async def compare(self) -> list[CompareRow]:
        rows = await self._request("conductor.compare", {})
        return [CompareRow.from_raw(r) for r in rows]

    async def trace(self, *, dot: bool = False) -> str:
        """Render the recorded orchestration DAG — the define-by-run payoff:
        the graph is a *view over the journal*, never a prerequisite."""
        fmt = "dot" if dot else "text"
        return await self._request("conductor.trace", {"format": fmt})

    # ── effectful operations (journaled) ────────────────────────────────────

    async def note(self, text: str) -> None:
        """Append a human-readable note to the run's event log."""
        await self._request("conductor.note", {"text": text})

    async def freeze(self) -> Run:
        """Seal the open round — no cross-agent influence before every first
        attempt is frozen. Idempotent under resume."""
        return Run.from_raw(await self._request("conductor.freeze", {}))

    async def verify(
        self,
        artifact: Artifact,
        command: Sequence[str],
        *,
        isolation: str | None = None,
        sealed_from: Artifact | str | None = None,
    ) -> Verification:
        """Neutrally re-execute ``command`` against the artifact owner's
        latest submission in a fresh sandboxed worktree — never the author's
        box.

        By default the command runs against the candidate's own submitted
        tree — appropriate when tests are part of the deliverable under
        review. ``sealed_from`` applies a **sealed overlay** instead: pass
        the sealing agent's ``Artifact`` (or a submission id / team agent
        id), and that submission's base..commit diff is overlaid over the
        candidate before the command runs, so the candidate cannot weaken
        checks it was handed — its edits to sealed paths are discarded and
        recorded as ``Verification.sealed_overridden`` tamper evidence.
        Typically the sealed content is an independently designed test set;
        the mechanism is content-agnostic (golden files, scoring harnesses).
        The sealing agent must differ from the candidate's owner
        (self-sealing fails closed), and the built-in verdict refuses to
        compare candidates verified against divergent sealed sets."""
        if isinstance(command, (str, bytes)):
            raise TypeError(
                "verify(command=...) takes an argv sequence like "
                '["cargo", "test", "--quiet"], not a shell string'
            )
        params: dict[str, Any] = {
            "artifact": artifact.to_payload(),
            "command": list(command),
        }
        if isolation is not None:
            params["isolation"] = isolation
        if sealed_from is not None:
            params["sealed_from"] = (
                sealed_from.id if isinstance(sealed_from, Artifact) else sealed_from
            )
        return Verification.from_raw(await self._request("conductor.verify", params))

    async def judge(self, policy: _policy.Policy = _policy.tests_then_smallest_diff) -> Verdict:
        """Decide and record a verdict.

        ``policy`` is a built-in (evaluated in the binary) or any Python
        callable ``(Run) -> Verdict`` — sync or async. Either way the verdict
        is journaled and lands in the event log through the same path as
        ``h5i team finalize``.
        """
        if isinstance(policy, _policy.BuiltinPolicy):
            raw = await self._request("conductor.judge", {"policy": policy.name})
            return Verdict.from_raw(raw)
        if not callable(policy):
            raise TypeError(f"policy must be a BuiltinPolicy or callable, got {policy!r}")
        begun = await self._request("conductor.judge_begin", {})
        if begun.get("replayed"):
            return Verdict.from_raw(begun["verdict"])
        token = begun["token"]
        try:
            decided = policy(Run.from_raw(begun["run"]))
            if inspect.isawaitable(decided):
                decided = await decided
        except BaseException:
            await self._abort("conductor.judge_abort", token)
            raise
        payload = decided.to_payload() if isinstance(decided, Verdict) else decided
        if not isinstance(payload, dict):
            await self._abort("conductor.judge_abort", token)
            raise TypeError(
                f"a policy must return a Verdict (or verdict dict), got {decided!r}"
            )
        committed = await self._request(
            "conductor.judge_commit", {"token": token, "verdict": payload}
        )
        return Verdict.from_raw(committed)

    async def apply(self, artifact: Artifact, *, force: bool = False) -> ApplyResult:
        """Apply an artifact onto the current branch, gated on an
        auto-applicable verdict selecting it. ``force=True`` is the explicit
        human-pick form — use only after your own gate."""
        raw = await self._request(
            "conductor.apply", {"artifact": artifact.to_payload(), "force": force}
        )
        return ApplyResult.from_raw(raw)

    async def step(self, label: str, fn: Callable[[], Any]) -> Any:
        """Run an arbitrary effect exactly once, journaling its JSON-serializable
        result — the universal escape hatch. On resume a completed step
        returns its recorded result without re-executing ``fn``.

        ``fn`` may be sync or async. Steps that run concurrently must carry
        distinct labels (see :meth:`scope` for loops); the journal fails
        closed otherwise. A step result must stay under the journal's inline
        cap (~64 KB) — route bulk data through ``h5i capture`` and journal
        the capture id.
        """
        begun = await self._request("conductor.step_begin", {"label": label})
        if begun.get("replayed"):
            return begun.get("result")
        token = begun["token"]
        try:
            value = fn()
            if inspect.isawaitable(value):
                value = await value
        except BaseException:
            # Release the label so an in-process retry can re-run the step.
            await self._abort("conductor.step_abort", token)
            raise
        return await self._request(
            "conductor.step_commit", {"token": token, "result": value}
        )

    def scope(self, prefix: str) -> Scope:
        """A label namespace for steps in parallel loops:
        ``c.scope(f"item/{i}").step("fetch", …)`` journals as
        ``item/<i>/fetch#1``. Scopes nest."""
        return Scope(self, prefix)

    async def patched(self, change_id: str) -> bool:
        """Migration marker for resuming an in-flight run with a changed
        score: ``False`` while replaying steps journaled before the marker
        existed, ``True`` on fresh execution, the recorded value forever
        after."""
        return await self._request("conductor.patched", {"change_id": change_id})

    async def gate(self, question: str, *, to: str | None = None) -> GateAnswer:
        """Ask a human a durable question (over ``h5i msg``). A score that
        times out waiting can simply exit; re-running it resumes the wait on
        the already-delivered question."""
        params: dict[str, Any] = {"question": question}
        if to is not None:
            params["to"] = to
        return GateAnswer.from_raw(await self._request("gate.ask", params))

    async def preflight(
        self,
        *,
        live: Iterable[Agent] | None = None,
        min_isolation: str | None = None,
        clean_worktree: bool = False,
    ) -> None:
        """Fail the predictable ways up front — dead sessions, weak isolation,
        dirty apply target — instead of at minute 30. All configured checks
        run; failures are reported together."""
        params: dict[str, Any] = {
            "live": [{"agent": a.id, "env_id": a.env_id} for a in live or ()],
            "clean_worktree": clean_worktree,
        }
        if min_isolation is not None:
            params["min_isolation"] = min_isolation
        await self._request("conductor.preflight", params)

    # ── plumbing ────────────────────────────────────────────────────────────

    async def _request(self, method: str, params: dict) -> Any:
        if self._bridge is None:
            raise OrchestraError(
                "conductor is not launched — use `async with Conductor(...)` "
                "or await launch() first"
            )
        return await self._bridge.request(method, params)

    async def _turn_request(
        self, method: str, params: dict, agent_id: str, env_id: str
    ) -> Any:
        """A `_request` that tells the session watcher a turn is in flight for
        ``agent_id`` — so a resident session that dies on startup (or mid-turn)
        is warned about instead of the score hanging silently."""
        watcher = self._session_watcher
        if watcher is None or self._launcher != "resident":
            return await self._request(method, params)
        watcher.expect(agent_id, env_id)
        try:
            return await self._request(method, params)
        finally:
            watcher.unexpect(agent_id)

    async def _abort(self, method: str, token: str) -> None:
        try:
            await self._request(method, {"token": token})
        except OrchestraError:
            pass  # the original exception matters more

    async def _serve_request(self, method: str, params: dict) -> Any:
        if method != "launcher.on_turn":
            raise OrchestraError(f"unexpected server request {method!r}")
        if self._on_turn is None:
            raise OrchestraError("no on_turn handler installed")
        outcome = self._on_turn(TurnContext.from_raw(params))
        if inspect.isawaitable(outcome):
            await outcome
        return {}


class Scope:
    """A step-label namespace (see :meth:`Conductor.scope`)."""

    def __init__(self, conductor: Conductor, prefix: str):
        self._conductor = conductor
        self._prefix = prefix

    def scope(self, sub: str) -> Scope:
        return Scope(self._conductor, f"{self._prefix}/{sub}")

    async def step(self, label: str, fn: Callable[[], Any]) -> Any:
        return await self._conductor.step(f"{self._prefix}/{label}", fn)


class Agent:
    """A hired agent: a roster seat bound to a sandboxed env.

    Handles are cheap and freely shareable — turns compose with plain
    ``asyncio`` concurrency (``gather`` different agents' turns; run one
    agent's turns sequentially, as one resident session serves them).
    """

    def __init__(self, conductor: Conductor, agent_id: str, env_id: str):
        self._conductor = conductor
        self.id = agent_id
        self.env_id = env_id

    def __repr__(self) -> str:
        return f"Agent({self.id!r}, env={self.env_id!r})"

    async def work(
        self,
        task: str,
        *,
        materials: Sequence[Artifact] | None = None,
        expect_independent: bool = False,
    ) -> Artifact:
        """One work turn: deliver ``task``, wait for the agent's submission.

        ``materials`` grants the worker visibility of teammate artifacts and
        stamps the result non-independent with influence edges (post-freeze
        only). ``expect_independent=True`` fails unless the artifact comes
        back stamped independent — protects arena/ensemble first attempts.
        """
        params: dict[str, Any] = {
            "agent": self.id,
            "env_id": self.env_id,
            "task": task,
            "expect_independent": expect_independent,
        }
        if materials:
            params["materials"] = [m.to_payload() for m in materials]
        raw = await self._conductor._turn_request(
            "agent.work", params, self.id, self.env_id
        )
        return Artifact.from_raw(raw)

    async def ask(
        self,
        prompt: str,
        *,
        parse: Parse | None = None,
        attempts: int = 3,
    ) -> Any:
        """Ask the agent for *data* instead of code: the reply must be a JSON
        value (the binary already re-asks up to 3× if the reply isn't JSON at
        all).

        ``parse`` optionally converts/validates the value (raise ``ValueError``
        to reject); a rejected reply is re-asked with the parse error attached,
        up to ``attempts`` times. Each re-ask is its own journaled step, so a
        resume replays the same conversation deterministically.
        """
        from ._errors import AskParseError

        current = prompt
        value: Any = None
        last_error: Exception | None = None
        for _ in range(max(1, attempts)):
            value = await self._conductor._turn_request(
                "agent.ask",
                {"agent": self.id, "env_id": self.env_id, "prompt": current},
                self.id,
                self.env_id,
            )
            if parse is None:
                return value
            try:
                return parse(value)
            except (ValueError, TypeError, KeyError) as e:
                last_error = e
                current = (
                    f"Your previous reply could not be used ({e}).\n\n{prompt}\n\n"
                    "(Reply again with ONLY the corrected JSON value.)"
                )
        raise AskParseError(
            f"agent '{self.id}' did not produce a parseable reply in "
            f"{attempts} attempts (last error: {last_error})",
            last_value=value,
        )

    async def reflect(self, artifact: Artifact) -> Review:
        """Critique your OWN submission — the self-feedback turn (Self-Refine's
        FEEDBACK step).

        Recorded as a ``reflection_submitted`` event, deliberately distinct
        from peer review: it never counts as review/quorum evidence, and it
        creates no cross-agent influence edge, so a revision addressing it
        stays stamped independent. The returned :class:`Review` has
        ``reviewer == target == <agent>`` and composes with :meth:`revise`
        unchanged (``review.approved`` uses the same APPROVE first-line
        convention). Reflecting on a teammate's artifact fails closed — that
        is a :meth:`review`.
        """
        raw = await self._conductor._turn_request(
            "agent.reflect",
            {
                "agent": self.id,
                "env_id": self.env_id,
                "artifact": artifact.to_payload(),
            },
            self.id,
            self.env_id,
        )
        return Review.from_raw(raw)

    async def review(self, artifact: Artifact) -> Review:
        """Review a teammate's artifact (scoped read grant → posted review)."""
        raw = await self._conductor._turn_request(
            "agent.review",
            {
                "reviewer": self.id,
                "env_id": self.env_id,
                "artifact": artifact.to_payload(),
            },
            self.id,
            self.env_id,
        )
        return Review.from_raw(raw)

    async def revise(self, artifact: Artifact, review: Review) -> Artifact:
        """Address a review and re-submit. Completes on any new submission —
        an agent that finds nothing to fix re-submits as-is."""
        raw = await self._conductor._turn_request(
            "agent.revise",
            {
                "agent": self.id,
                "env_id": self.env_id,
                "artifact": artifact.to_payload(),
                "review": review.to_payload(),
            },
            self.id,
            self.env_id,
        )
        return Artifact.from_raw(raw)
