"""Auto-attach viewers for resident agent sessions — ``Conductor(watch=True)``.

The ``"resident"`` launcher brings each agent up in a *detached* tmux session
named ``h5i-orch-<run>-<agent>``, which normally means hunting for the right
``tmux attach -t …`` in a second terminal. Watching removes the hunt: a
background task polls for those sessions and opens an interactive viewer on
each one as it appears (and again if it dies and comes back).

How a viewer is opened, in order of preference:

1. an explicit command template — ``watch="kitty --title {session} tmux
   attach -t {session}"`` or ``$H5I_TERMINAL``. ``{session}`` is replaced
   with the session name; a template without the placeholder gets
   ``tmux attach-session -t <session>`` appended as trailing argv (the
   ``terminal -e``-style convention);
2. already inside tmux (``$TMUX``): the agent's window is **linked** into
   your current session — no nested clients, switch to it like any other
   window (it is removed when the agent session ends);
3. inside herdr (``$HERDR_ENV`` with the ``herdr`` binary available): a
   herdr pane per agent running ``tmux attach``, so seats land in herdr's
   own sidebar (with its per-agent working/blocked/done status);
4. WSL with Windows Terminal on PATH: a new ``wt.exe`` tab per agent;
5. a GUI terminal on ``$DISPLAY``/``$WAYLAND_DISPLAY`` (wezterm, kitty,
   alacritty, gnome-terminal, konsole, foot, x-terminal-emulator, xterm);
6. otherwise: a hint line on stderr with the exact attach command.

A broken viewer never fails the score — every opener error degrades to the
stderr hint.
"""

from __future__ import annotations

import asyncio
import os
import shlex
import shutil
import sys
from collections.abc import Callable, Mapping
from typing import Any, NamedTuple

__all__ = ["Opener", "SessionWatcher", "resolve_opener", "session_prefix"]


def session_prefix(run_id: str) -> str:
    """The resident launcher's session-name prefix for a run (mirrors the
    Rust side's ``h5i-orch-{run_id}-{agent_id}``)."""
    return f"h5i-orch-{run_id}-"


def _attach_argv(session: str) -> list[str]:
    return ["tmux", "attach-session", "-t", session]


def template_argv(template: str, session: str) -> list[str]:
    """Expand a user command template into argv for one session."""
    words = shlex.split(template)
    if any("{session}" in w for w in words):
        return [w.replace("{session}", session) for w in words]
    return words + _attach_argv(session)


def wt_argv(session: str, distro: str | None = None) -> list[str]:
    """A new Windows Terminal tab attached to ``session`` (WSL interop)."""
    wsl = ["wsl.exe"] + (["-d", distro] if distro else [])
    return ["wt.exe", "-w", "0", "new-tab", "--title", session, *wsl, "-e", *_attach_argv(session)]


#: GUI terminals we know how to hand an attach command, most specific first.
_GUI_TERMINALS: tuple[tuple[str, Callable[[str], list[str]]], ...] = (
    ("wezterm", lambda s: ["wezterm", "start", "--", *_attach_argv(s)]),
    ("kitty", lambda s: ["kitty", "--title", s, *_attach_argv(s)]),
    ("alacritty", lambda s: ["alacritty", "--title", s, "-e", *_attach_argv(s)]),
    ("gnome-terminal", lambda s: ["gnome-terminal", "--title", s, "--", *_attach_argv(s)]),
    ("konsole", lambda s: ["konsole", "-p", f"tabtitle={s}", "-e", *_attach_argv(s)]),
    ("foot", lambda s: ["foot", "--title", s, *_attach_argv(s)]),
    ("x-terminal-emulator", lambda s: ["x-terminal-emulator", "-e", *_attach_argv(s)]),
    ("xterm", lambda s: ["xterm", "-T", s, "-e", *_attach_argv(s)]),
)


class Opener(NamedTuple):
    """How viewers are opened: spawn argv, link a tmux window, or just hint."""

    name: str
    kind: str  # "spawn" | "tmux-link" | "hint"
    argv: Callable[[str], list[str]] | None = None


def resolve_opener(
    template: str | None = None,
    *,
    env: Mapping[str, str] = os.environ,
    which: Callable[[str], str | None] = shutil.which,
) -> Opener:
    """Pick the best way to surface agent sessions in this environment."""
    template = template or env.get("H5I_TERMINAL")
    if template:
        name = shlex.split(template)[0]
        return Opener(name, "spawn", lambda s: template_argv(template, s))
    if env.get("TMUX"):
        return Opener("tmux link-window", "tmux-link")
    if env.get("HERDR_ENV") == "1" and which("herdr"):
        return Opener("herdr pane", "herdr")
    if which("wt.exe") and which("wsl.exe"):
        distro = env.get("WSL_DISTRO_NAME")
        return Opener("wt.exe", "spawn", lambda s: wt_argv(s, distro))
    if env.get("DISPLAY") or env.get("WAYLAND_DISPLAY"):
        for name, build in _GUI_TERMINALS:
            if which(name):
                return Opener(name, "spawn", build)
    return Opener("hint", "hint")


class SessionWatcher:
    """Polls tmux for a run's agent sessions and opens a viewer on each.

    Sessions appear lazily (the resident launcher brings one up on an
    agent's *first turn*), so the watcher runs for the whole score. A
    session that vanishes and comes back gets a fresh viewer.
    """

    def __init__(
        self,
        run_id: str,
        *,
        template: str | None = None,
        opener: Opener | None = None,
        poll_interval: float = 1.0,
        grace: float = 15.0,
        spawn_gap: float = 0.5,
        echo: Callable[[str], None] | None = None,
    ):
        self._prefix = session_prefix(run_id)
        self._opener = opener or resolve_opener(template)
        self._poll_interval = poll_interval
        self._grace = grace
        self._spawn_gap = spawn_gap
        self._echo = echo or (lambda line: print(line, file=sys.stderr, flush=True))
        self._open_now: set[str] = set()
        #: session name → pending-turn record (env id, started-at, warned yet).
        self._expected: dict[str, dict[str, Any]] = {}
        # herdr-opener state (see _open_herdr_pane), built lazily.
        self._herdr_client: Any = None
        self._herdr_last_pane: str | None = None

    def _agent(self, session: str) -> str:
        return session[len(self._prefix):] or session

    # ── pending-turn tracking (fed by Agent turn calls) ─────────────────────

    def expect(self, agent_id: str, env_id: str) -> None:
        """A turn for ``agent_id`` is in flight — its session should exist
        (or appear within the grace period). Warns loudly otherwise: the
        classic silent failure is a session that dies on startup because the
        env behind it is gone."""
        self._expected[f"{self._prefix}{agent_id}"] = {
            "env": env_id,
            "since": asyncio.get_running_loop().time(),
            "warned": False,
        }

    def unexpect(self, agent_id: str) -> None:
        """The turn finished (either way) — stop holding it to the deadline."""
        self._expected.pop(f"{self._prefix}{agent_id}", None)

    def _check_expected(self, current: set[str]) -> None:
        now = asyncio.get_running_loop().time()
        for session, pending in self._expected.items():
            if session in current:
                pending["warned"] = False  # it's up; re-arm for a later death
                continue
            if pending["warned"] or now - pending["since"] <= self._grace:
                continue
            pending["warned"] = True
            self._echo(
                f"[h5i] WARNING: agent '{self._agent(session)}' has a turn in flight "
                f"but its tmux session ({session}) has not appeared in {int(self._grace)}s "
                f"— it may be dying on startup. Check the env is alive: "
                f"h5i env shell {pending['env']} -- true"
            )

    async def run(self) -> None:
        """Poll until cancelled (or until tmux turns out not to exist)."""
        how = (
            f"each opens in {self._opener.name} as its first turn starts"
            if self._opener.kind != "hint"
            else "attach commands are printed as each comes up"
        )
        self._echo(f"[h5i] watching for agent sessions ({self._prefix}*) — {how}")
        while await self.poll_once():
            await asyncio.sleep(self._poll_interval)

    async def poll_once(self) -> bool:
        """One poll step; returns False when polling can never succeed."""
        names = await self._list_sessions()
        if names is None:
            self._echo(
                "[h5i] tmux not found — agent sessions cannot be watched "
                "(the resident launcher needs tmux)"
            )
            return False  # no tmux binary — sessions will never appear
        current = {n for n in names if n.startswith(self._prefix)}
        for session in sorted(self._open_now - current):
            pending = self._expected.get(session)
            if pending is not None:
                pending["warned"] = True  # this line already says it all
                self._echo(
                    f"[h5i] WARNING: agent '{self._agent(session)}' session ended "
                    f"while its turn is still pending ({session}) — the runtime may "
                    f"have crashed. Check the env is alive: "
                    f"h5i env shell {pending['env']} -- true"
                )
            else:
                self._echo(
                    f"[h5i] agent '{self._agent(session)}' session ended ({session})"
                )
        self._open_now &= current  # a vanished session may come back
        for i, session in enumerate(sorted(current - self._open_now)):
            self._open_now.add(session)
            if i:
                # Several agents often come up in one poll (gathered first
                # turns). Rapid-fire viewer spawns race — Windows Terminal
                # drops tabs dispatched near-simultaneously — so give each
                # viewer a beat to register before the next.
                await asyncio.sleep(self._spawn_gap)
            await self._open(session)
        self._check_expected(current)
        return True

    async def _list_sessions(self) -> list[str] | None:
        try:
            proc = await asyncio.create_subprocess_exec(
                "tmux",
                "list-sessions",
                "-F",
                "#{session_name}",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
        except FileNotFoundError:
            return None
        out, _ = await proc.communicate()
        if proc.returncode != 0:
            return []  # no tmux server yet — keep polling
        return out.decode(errors="replace").splitlines()

    async def _open(self, session: str) -> None:
        opener = self._opener
        agent = self._agent(session)
        try:
            if opener.kind == "spawn":
                assert opener.argv is not None
                await self._spawn(opener.argv(session))
                self._echo(
                    f"[h5i] agent '{agent}' session up — opened in {opener.name} "
                    f"(or: tmux attach -t {session})"
                )
                return
            if opener.kind == "tmux-link":
                await self._link_window(session)
                self._echo(
                    f"[h5i] agent '{agent}' session up — linked as window "
                    f"'{session}' in your current tmux session"
                )
                return
            if opener.kind == "herdr":
                pane_id = await self._open_herdr_pane(session)
                self._echo(
                    f"[h5i] agent '{agent}' session up — opened in herdr "
                    f"pane {pane_id} (or: tmux attach -t {session})"
                )
                return
        except Exception as e:  # noqa: BLE001  # a broken viewer must not fail the score
            self._echo(
                f"[h5i] agent '{agent}' session up, but its viewer failed ({e}) — "
                f"attach with: tmux attach -t {session}"
            )
            return
        self._echo(
            f"[h5i] agent '{agent}' session up — view it with: tmux attach -t {session}"
        )

    async def _spawn(self, argv: list[str]) -> None:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
            start_new_session=True,
        )
        # Two kinds of viewer command: dispatchers (wt.exe, wezterm cli) exit
        # as soon as the tab is registered — wait for that, so consecutive
        # opens don't race, and a fast non-zero exit means the viewer failed
        # (degrade to the attach hint). Long-lived terminals (kitty, xterm)
        # outlive the timeout; leave them reaping in the background.
        waiter = asyncio.ensure_future(proc.wait())
        done, _ = await asyncio.wait({waiter}, timeout=2.0)
        if waiter in done and waiter.result() != 0:
            raise RuntimeError(f"viewer command exited with status {waiter.result()}")

    async def _open_herdr_pane(self, session: str) -> str:
        """A herdr pane running ``tmux attach`` on the session: the first
        viewer splits right of this pane, later ones grow a column down."""
        from ._herdr import HerdrClient, resolve_herdr_bin

        if self._herdr_client is None:
            self._herdr_client = HerdrClient(resolve_herdr_bin())
        anchor, direction = (
            (self._herdr_last_pane, "down")
            if self._herdr_last_pane
            else (os.environ.get("HERDR_PANE_ID"), "right")
        )
        pane = await self._herdr_client.split(pane=anchor, direction=direction)
        pane_id: str = pane["pane_id"]
        await self._herdr_client.rename(pane_id, session)
        await self._herdr_client.run(
            pane_id, f"tmux attach-session -t {session}"
        )
        self._herdr_last_pane = pane_id
        return pane_id

    async def _link_window(self, session: str) -> None:
        proc = await asyncio.create_subprocess_exec(
            "tmux",
            "list-windows",
            "-t",
            session,
            "-F",
            "#{window_id}",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await proc.communicate()
        window_ids = out.decode(errors="replace").split()
        if proc.returncode != 0 or not window_ids:
            raise RuntimeError(f"no windows found in tmux session '{session}'")
        link = await asyncio.create_subprocess_exec(
            "tmux",
            "link-window",
            "-d",
            "-a",
            "-s",
            window_ids[0],
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        if await link.wait() != 0:
            raise RuntimeError("tmux link-window failed")
