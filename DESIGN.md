# senv — Design Document

> **senv** — A security boundary for Python environments.
> Persistent, sandboxed Python environments powered by [uv](https://github.com/astral-sh/uv) and [h5i](https://github.com/h5i-dev/h5i).

Status: **implemented** — v0.1 · 2026-08-13

This document describes senv as built. Where implementation changed a decision,
the reasoning is recorded here rather than in a changelog: several of those
changes strengthened the guarantees, and the "why" is the part worth keeping.

---

## 1. Overview and positioning

`venv` and `uv` provide *logical* isolation — a separate interpreter path and
package set — but no *security* isolation. Code running inside a virtualenv can
read `~/.ssh`, exfiltrate environment variables, reach any network host, and
run arbitrary install-time build scripts. Containers and microVMs provide real
boundaries but abandon the virtualenv UX entirely.

senv fills the gap: a **secure Python environment manager** with uv's UX and an
OS-level security boundary underneath. Every phase that executes third-party
code — dependency resolution, sdist builds, install scripts, tests, the app
itself — runs inside an h5i sandbox with a filesystem allowlist, a network
egress policy, environment-variable filtering, and resource limits.

```bash
senv init --python 3.13
senv add requests
senv sync
senv run pytest
senv shell
```

### Positioning vs. alternatives

| | Python compat | Boundary | Notes |
|---|---|---|---|
| venv / uv | full | none | logical separation only |
| vsbox | high | filesystem-centric (Landlock/bwrap/audit hooks) | young; audit hooks bypassable via C extensions |
| Monty | Python subset | strong capability model | not a real CPython environment |
| Pyodide / WASI | partial | WASM runtime | no native extensions, FFI caveats |
| containers / microVMs | full | strong | heavyweight UX, user manages images |
| **senv** | **full (real CPython + uv)** | **fs + network + env + resources, tiered** | container-free UX by default, containers/microVMs as opt-in tiers |

The position — "uv-compatible workflow, sandbox both installs *and* runs,
without making the user think about containers" — is currently unoccupied.
vsbox is the nearest prior art but covers filesystem only; senv covers network
egress, secret filtering, and resource limits, and sandboxes the install phase
(where supply-chain attacks actually execute) as rigorously as the run phase.

### Non-goals

- Not a package manager. uv does resolution, locking, and installation; senv
  never reimplements any of it.
- Not a container platform. Container/microVM tiers are borrowed from h5i,
  never exposed as image plumbing the user must manage.
- Not a Python subset or restricted interpreter. Real CPython, real wheels,
  real native extensions.
- Not multi-language (v1). The h5i policy engine is language-agnostic, so the
  door stays open, but senv v1 is Python-only and says so.

---

## 2. Threat model

### In scope — what senv defends against

| Threat | Where it executes | senv mitigation |
|---|---|---|
| Malicious sdist build backend / `setup.py` | `senv sync` / `add` | install phase runs sandboxed: egress limited to package registries, your source read-only, writes limited to the environment + wheel cache, no secrets in env, and resolution staged away from your source entirely |
| Malicious package code at runtime (typosquats, hijacked releases) | `senv run` / `shell` | network deny by default; fs writes confined to the project; `~/.ssh`, `~/.aws`, cloud credentials unreadable |
| Credential/env exfiltration | both | env allowlist (`PATH`, `HOME`, `LANG`, `TERM` by default); secrets only via explicit declaration; secret-pattern redaction in logs |
| Environment self-modification / persistence (malware editing the venv at runtime) | run | the environment is granted **read-only** at run time and lives outside the project, so no writable grant contains it |
| Resource exhaustion (fork bombs, memory balloons, runaway jobs) | both | rlimits + cgroups: memory, process count, wall clock (default 30 min), file size |
| Tampering with the enforced policy | — | resolved policy is digested; the digest is stamped into every receipt, and the policy is recompiled each run rather than read back from disk |
| Tampering with the evidence | run | receipts live outside every grant senv issues, so the code they record cannot rewrite them |
| **Tampering with the policy** | run | `senv.toml` is inside the project, which the run phase can write. senv keeps a snapshot of the accepted policy outside the sandbox and refuses to execute anything under a **widened** one until `senv trust` accepts it (§5) |
| Escalating to unconfined host execution | run | the two paths that existed — a `command:` secret source and a project-supplied `[env] uv` — are gated and refused respectively (§5) |

### Out of scope — stated honestly

- **Kernel exploits at the `process` tier.** Landlock + seccomp + namespaces is
  a strong same-kernel boundary, not a hypervisor. Users who need
  VM-grade isolation select the `microvm` tier.
- **Malicious-but-in-scope behavior.** Code that corrupts files inside the
  project write grant, or abuses an egress host the user allowlisted, is
  within policy.
- **Artifacts leaving the boundary.** If `senv run` produces a wheel or script
  and the user executes it *outside* senv, senv makes no claim about it.
- **The registry itself.** senv trusts what uv verifies (lockfile hashes). It
  narrows the blast radius of a bad package; it cannot detect one.
- **A complete record of what was attempted.** Denials are inferred from what a
  refused program printed, so `senv report` is a strong lead, not an audit log.
  Only the container tier observes every request. Enforcement does not depend on
  this — a denial is enforced whether or not senv recognises the message.

senv inherits h5i's fail-closed philosophy: when a host cannot enforce a
policy, the operation is **refused with an explanation**, never silently
weakened.

---

## 3. CLI surface

uv-shaped, minimal, no container vocabulary:

```
senv init [--python V] [--replace-venv] [--no-sync]
                               create or adopt a project, then build its environment
senv add <pkg>...              resolve in a staging copy, write back, sync
senv remove <pkg>...           the same, in reverse
senv sync                      install the locked dependencies
senv lock [--check]            update (or verify) the lockfile
senv run <cmd> [args...]       run a command inside the run boundary
senv shell                     interactive confined session
senv status                    the policy that is enforced, per phase, with digests
senv report [--suggest]        what ran, what was denied, what was redacted
senv allow <host>...           let the run phase reach a host (writes senv.toml)
senv doctor                    what this host can enforce
senv gc [--prune] [--cache]    remove state for projects that no longer exist
senv uv -- <args...>           any uv command, inside the install boundary
```

Every command takes `--json` for tooling and `--project DIR` to act on a
project other than the one containing the working directory. The mutating
install commands take `--in-place` (§5). Exit codes pass through from the
confined command; senv's own failures use exit code 2.

Exit codes pass through from the confined command. `--json` on every
subcommand for tooling (mirrors h5i's convention).

Design rules:

- **No unsandboxed execution path.** There is no `--unsafe` that runs on the
  host. The weakest reachable state is the `process` tier with `net = "host"`,
  explicitly configured and loudly reported by `senv status`.
- **Denials are legible.** When the sandbox blocks something, senv prints what
  was blocked and the one-line config change that would allow it — a blocked
  `pip install` mid-run suggests adding the host or re-running `senv sync`.

### Migrating from uv / venv — zero-rewrite adoption

Adoption friction is a design constraint, not an afterthought: if switching
costs more than a minute, users stay on plain uv and get no boundary at all.

**Invariant: a senv project remains a valid uv project at all times.** senv
adds nothing to your project tree at all: its state lives outside it (§8), the
only new file is an optional `senv.toml`, and `.venv` keeps working as a
symlink. It never changes the format or location of `pyproject.toml`,
`uv.lock`, or `.venv`. Consequences:

- Teammates and CI without senv keep running plain `uv sync` / `uv run`
  against the same repo, unchanged. senv can be adopted by one person on a
  team without a team-wide decision.
- Opting out is deleting nothing — stop typing `senv` and you are back on uv.
- Zero required config. With no `senv.toml`, the fail-closed defaults apply;
  the file first appears when the user first widens something (`senv allow`
  writes it).

**`senv init` adopts, it does not scaffold.** In an existing project it
detects `pyproject.toml`, `uv.lock`, `.python-version`, and `.venv`, and takes
them as-is. An existing host-installed `.venv` poses a provenance question —
its bytes never passed through the install boundary — so `init` offers a
sandboxed rebuild (one `senv sync`, cheap thanks to uv's wheel cache and the
lockfile). Declining is allowed; `senv status` then reports the venv as
`host-installed (unverified)` until the first sandboxed sync.

**Muscle-memory mapping** — every habit has a same-shape equivalent:

| Habit | senv equivalent |
|---|---|
| `uv sync` / `uv lock` / `uv add X` | `senv sync` / `senv lock` / `senv add X` (same flags, passed through to uv) |
| `uv run pytest` | `senv run pytest` |
| `source .venv/bin/activate` + work | `senv shell` |
| any other uv command | `senv uv -- <args…>` |

**The first week of denials.** The realistic migration cost is not commands
but policy: an app that used to call external APIs, read `~/datasets`, or see
`AWS_PROFILE` now gets denied by default. Two mitigations, neither of which
weakens the boundary:

- Every denial prints the exact copy-pasteable `senv.toml` line (or `senv
  allow` command) that would permit it — already a design rule above.
- `senv report --suggest` aggregates the denials recorded in receipts across
  runs and emits a complete suggested policy stanza to review and paste.
  The boundary never auto-widens; suggestions are offline text, and applying
  them is always an explicit, diffable edit to a checked-in file.

There is deliberately no "observe mode" that runs unconfined to learn a
policy — that would mean running untrusted code without a boundary exactly
once, which is once too many. Denial-driven suggestion gets the same
information from *confined* runs.

---

## 4. Architecture

```
┌────────────────────────────────────────────────┐
│ senv (Rust binary)                             │
│                                                │
│  CLI / config (senv.toml → h5i Profile)        │
│  phase policies (provision / install / run)    │
│  venv + cache lifecycle (drives uv)            │
│  staging, receipts, denial analysis            │
├────────────────────────────────────────────────┤
│ h5i-sandbox (linked crate)                     │
│  Profile → resolve() → ResolvedPolicy → run()  │
│  tiers: process | supervised | container |     │
│         microvm   (Linux) · Seatbelt (macOS)   │
│  secrets broker · auth proxy · redaction rules │
└────────────────────────────────────────────────┘
                     │ spawns
                     ▼
          confined uv / python / shell
```

**senv links the `h5i-sandbox` crate directly; it does not shell out to the
`h5i` CLI and does not use `h5i box`.** Rationale:

- `h5i-sandbox` was deliberately extracted to compile independently of h5i's
  domain layer, depends only on `h5i-error`, and contains no git usage in its
  source. Its API (`Profile` → `resolve()` → `ResolvedPolicy` →
  `run_with_env()` / `run_interactive()`) takes a plain working-directory
  path — exactly what a venv needs.
- `h5i box` drags in the git-worktree lifecycle (worktrees, branches, manifest
  refs under `.git`). A Python project directory should not be required to be
  a git repository, and an environment is not a disposable worktree.
- Linking gives senv `capabilities_report()` for `senv doctor`, the secrets
  broker, the redaction scanner, and the read-only-bind machinery for free.

senv constructs `Profile` values in Rust (starting from h5i's fail-closed
builtin base and narrowing), rather than generating `.h5i/env.toml` files.
Until h5i publishes to crates.io, the dependency is a pinned git tag.

What senv adds on top of the engine:

1. **Phase-specific policies** (provision vs. install vs. run — §5), senv's
   core idea.
2. **venv lifecycle**: driving uv, keeping the venv read-only at run time,
   interpreter installs.
3. **`senv.toml`**: a Python-project-shaped config surface that compiles down
   to h5i profiles, so users never write h5i policy by hand.
4. **Wheel/interpreter caches** keyed by lockfile hash (borrowing h5i's
   refresh-box pattern — §8).

---

## 5. The three security phases

senv's central design decision: **installation and execution are different
trust problems and get different policies.** Existing tools sandbox one or
neither; supply-chain attacks overwhelmingly fire at install time (build
backends, `setup.py`, post-install hooks), while data exfiltration fires at
run time.

Implementation added a third, narrower phase in front of both.

### Phase 0 — provisioning (`uv python install`)

Downloading a Python interpreter reaches GitHub, not PyPI, and involves no
third-party code at all. Folding it into the install phase would have meant
widening that phase's egress permanently for something it needs once, so it is
its own phase: the only writable path is the shared interpreter directory,
egress is the python-build-standalone release hosts, and every other phase then
gets that directory **read-only**. The install phase runs with
`UV_PYTHON_DOWNLOADS=never`, so it cannot quietly reach for an interpreter
under a policy that never anticipated one.

senv enters this phase only when uv reports no suitable interpreter, and says
so on the way in.

### Phase 1 — install boundary (`sync` / `add` / `remove` / `lock` / `uv`)

Runs `uv` confined. Modeled directly on h5i's cache-refresh box: the install
runs *alone* — no agent, no app code of yours — with a narrow writable window
and network narrowed to registries.

| Dimension | Policy |
|---|---|
| filesystem read | system paths, the project (**read-only**), the uv binary, the interpreter dir |
| filesystem write | the environment, the wheel cache, a per-phase temp dir — nothing else |
| network | egress allowlist: `pypi.org`, `files.pythonhosted.org`, plus configured extra indexes |
| env | minimal allowlist; **no secrets, ever** — `Config::validate` refuses to let a secret be scoped to this phase, and the install profile would drop one anyway |
| resources | mem 8G, procs 512, wall 60m (a native wheel build is a compiler) |

Two changes from the original design, both narrowing:

**The project is read-only during installs.** The design gave the install phase
write access to the project because `uv.lock` and `pyproject.toml` live there.
Implementation split those apart instead — see staging below — so `senv sync`
now hands uv the project read-only and a build backend cannot edit your source
while installing. The predictable false refusal (a backend that insists on
writing `*.egg-info` into the source tree) is detected and reported with the
exact setting that permits it, `[install] project-writable = true`. In practice
modern setuptools and hatchling both build in a temp dir and need nothing.

**Manifest writes go through staging.** This resolves the design's first open
question. `lock` / `add` / `remove` copy `pyproject.toml`, `uv.lock`,
`.python-version` and the readme into a staging directory, run uv there, and
copy the result back after validating it parses. A build backend executing
during resolution therefore sees a directory containing your manifests and
**none of your source**.

The copy-back is checked, not trusted. A plain `senv lock` must not change
`pyproject.toml`; if the staged copy changed anyway, senv refuses to apply it
and says where the staged file is. For `add`/`remove`, which change it by
design, senv compares the parsed manifests and reports any key that moved
outside the dependency lists — `[build-system]` changing during a dependency
add is exactly the shape worth a sentence on someone's terminal.

Staging cannot work for every project: dynamic metadata, workspaces, and path
dependencies need the real tree. senv detects those from the manifest itself,
announces that it is resolving in-place and why, and records it in the receipt.
That is a deterministic consequence of declared configuration rather than a
silent fallback — and `--in-place` requests it explicitly.

A malicious build script can therefore: burn CPU inside its limits, write junk
into the environment it is building, and talk to PyPI. It cannot read your SSH
keys, edit your source, phone home to an attacker host, or see a credential.

### Phase 2 — run boundary (`run` / `shell`)

| Dimension | Policy (default) |
|---|---|
| filesystem read | system paths, the environment (**read-only**), the interpreter dir |
| filesystem write | the project directory, a scratch dir, a per-phase temp dir — not the environment |
| network | **deny** (opt into hosts via `senv allow` / `senv.toml` / `--allow-net`) |
| env | `PATH`, `HOME`, `LANG`, `TERM`, `COLORTERM` + declared secrets only |
| resources | mem 4G, procs 256, wall 30m; all configurable |

The read-only environment is a deliberate security property: runtime code
cannot patch installed packages to persist across runs. It is airtight because
the environment lives **outside** the project tree (§8) — there is no writable
parent to reach it through, so it does not depend on how Landlock resolves a
more specific rule inside a granted directory.

`TMPDIR` is set to a granted temp directory, because the default grants make
`/tmp` readable but not writable — without it every `tempfile.mkdtemp()` would
land in the project or fail.

There is deliberately **no writable bytecode cache**, and getting that wrong
was instructive. senv originally pointed `PYTHONPYCACHEPREFIX` at a writable
scratch directory so byte compilation kept working against a read-only
environment. But a `.pyc` is an *authoritative copy of the code*: CPython
loads it in preference to the source whenever its header matches the source's
mtime and size, both of which a package can read. So one execution could plant
bytecode that ran in place of a read-only module on every later import —
persistence straight through the door the read-only environment exists to
close. Verified: after the plant, `import idna` executed the attacker's module
body.

The fix keeps the speed and removes the hole: the install phase compiles
bytecode **into the environment** (`UV_COMPILE_BYTECODE`), where it is
read-only at run time, and the run phase is given no writable cache at all.
Writes to the environment's `__pycache__` fail and CPython ignores that, as it
always has. The general lesson is worth carrying: *a writable cache of
read-only code is not a cache, it is a second copy of the thing you were
protecting.*

`net = "deny"` uses an empty network namespace and works at the lightest tier
on every Linux kernel — the common case (run untrusted code with no network)
has no exotic host requirements. Only egress *allowlists* need a stronger tier
(§6), and senv escalates the tier automatically when a policy asks for one.

Secrets follow h5i's broker model: declared by id in config, sourced from
`env:`/`file:`/`command:`, injected only into the phases that name them, never
written to logs (fingerprint only), and scrubbed from receipts by the redaction
scanner. `inject = "file"` is refused with h5i's own message, since senv never
runs at the workspace tier.

### The policy is inside the sandbox

A security review of the implementation found the sharpest problem in the
design so far, and it is worth stating plainly rather than burying: **the
policy file lives in the project directory, and the run phase grants the
project read-write.** `senv.toml` is writable by exactly the code it governs.

Three working escalations followed from that, each needing only one execution
under `senv run`:

1. Write `[run] net = "host"` and `[run.env] pass = ["AWS_SECRET_ACCESS_KEY"]`.
   The next run has unrestricted network *and* the credential to send.
   (Verified: raw-IP connections succeeded on the following run.)
2. Write `[secrets.X] source = "command:…"`. The secrets broker runs that
   **on the host, outside the sandbox** — a full escape. senv made this worse
   than h5i intended by enabling h5i's `allow_command_extractors` gate
   automatically whenever a `command:` source appeared, turning a deliberate
   opt-in into an implicit one.
3. Drop a script into the project and point `[env] uv` at it. `senv doctor`
   executed the configured path to read its version: host execution out of a
   diagnostic command.

Moving the file elsewhere does not fix this. A policy that cannot sit beside
the code it governs will not be kept in version control, and any path the
sandbox can write has the same problem. What actually helps is **tamper
evidence plus a gate**:

- senv keeps a normalized snapshot of the security-relevant settings in
  `state.json`, outside every grant. Before compiling a policy it compares.
- **Narrowed or unchanged** → proceed silently, and re-baseline. An attacker
  gains nothing by narrowing, and a prompt that fires on safe edits is a prompt
  people learn to click through.
- **Widened** → refuse, name every widening, and require `senv trust`. Every
  command that executes anything is covered, `init` and `allow` included —
  `allow` re-baselines by design, so it must start from an accepted state or it
  would launder someone else's edit through the user's.
- A `command:` secret source additionally needs `[env] allow-command-secrets
  = true`, which is refused at config load without it and is itself a widening.
- A `[env] uv` that resolves inside the project or senv's state is **refused
  outright**, trusted or not: a binary the sandbox can rewrite between two
  commands must never be senv's own toolchain. A configured uv is never
  executed unconfined, so `doctor` reports it without running it.

This is tamper evidence, not prevention: senv cannot stop a package writing the
file, only refuse to act on the result until a human agrees. That is the
strongest honest guarantee available when the policy has to live next to the
code, and it converts a silent, permanent compromise into a refusal that names
what changed.

A second review pass, done independently against the hardened tree, found the
worst one — and it was a case of believing a guarantee rather than testing it.

**The install phase never had the project read-only.** senv listed the project
under `fs_read` and concluded it was read-only. But h5i grants the *working
directory* read-write implicitly: `$WORK` is in the builtin `fs_write`, and the
enforcement path unions that over any read grant naming the same path. The
correct switch is `ResolvedPolicy::work_readonly`, which senv never set. So
`senv sync` ran with your source writable — a build backend could edit
`conftest.py` or `senv.toml` — while `senv status` printed "the project,
read-only". Both are fixed, and the unit test that gave false assurance (it
asserted the project was absent from `fs_write`, which was true and irrelevant)
now asserts on the *resolved* policy instead.

The lesson generalises: **assert on what the engine will enforce, not on the
strings senv handed it.** Every other guarantee in this document was verified
by running a program under the policy and watching it fail; this one was not,
and it was the one that was wrong.

Two further findings from that pass:

- **A manufactured nested project bypassed the trust gate.** State is keyed by
  project root, and discovery takes the nearest marker walking up — so a package
  could create `tests/pyproject.toml` plus a hostile `tests/senv.toml`, and a
  user who later ran senv from that directory got a project with no recorded
  baseline, adopted silently. Verified as unconfined host execution. First
  sight is now fail-closed: a policy that grants more than senv's defaults is
  refused until `senv trust` accepts it, whether or not senv has seen the
  project before. A project nested inside another tracked project is also
  reported, since that is the shape of the attack (and a normal monorepo, so it
  warns rather than refuses).
- **Staging followed symlinks in both directions.** senv does those copies
  unconfined. A package replacing `README.md` with a link to `~/.ssh/id_ed25519`
  had senv copy the key into the staging directory — which is the install
  phase's own writable working directory, readable by every build backend that
  runs there. senv would have been carrying the credential across its own
  boundary. Sources are now checked with `symlink_metadata` and skipped;
  destinations are written with `O_NOFOLLOW`.

The recurring shape across all of these is worth naming: **senv's own
unconfined code touches paths inside the sandbox's write grant.** Anywhere that
happens — reading a config, copying a manifest, writing a lockfile, executing a
tool — the path is attacker-controlled input and has to be treated as such.

Two smaller findings from the same review, both fixed: `.python-version` and
`[env] python` are attacker-writable and became `uv` arguments, so a value that
is not version-shaped (`--mirror=https://evil`) is now refused rather than
forwarded; and program output quoted inside senv's own messages is stripped of
terminal control sequences, since a package could otherwise repaint senv's
framing to make a refusal read as an approval.

### Observing what was refused

At the kernel tiers there is no egress log to read, so senv infers denials from
what the program said when it was refused. `senv run` streams to the terminal,
which would leave nothing to inspect, so senv mirrors the child's **stderr**
through itself on the way out: fd 2 is redirected to a pipe that writes through
to the real terminal and into a bounded buffer. stdout is left alone, since it
is what most tools test with `isatty` to decide on colour. `senv shell` gets no
tee at all — the child owns that terminal.

Inference is conservative by construction. A host is accepted only from a URL,
from quotes, or after a phrase that names one, and never from a bare dotted
token, because in Python output `socket.gaierror` looks exactly like a
hostname. A refusal whose destination is never named is still recorded, as a
refusal with nothing to suggest. Denials against the environment or a
credential path are classified as **by design** and are never offered as
something to allow — a tool that helpfully suggested making the venv writable
would be undoing its own reason to exist.

## 6. Isolation tiers and platform matrix

senv exposes one knob, `isolation`, defaulting to `auto`:

| Tier (h5i) | Mechanism | senv usage |
|---|---|---|
| `process` | Landlock + seccomp-bpf + namespaces + rlimits (Linux) / Seatbelt (macOS) | default run tier; starts in <200 ms |
| `supervised` | + private netns, nftables pinned to resolved IPs, syscall-gated sockets | default install tier on Linux (egress allowlists need it) |
| `container` | rootless Podman, ro rootfs, DNS-pinned CONNECT proxy | opt-in; also the fallback when `supervised` is unavailable but Podman is |
| `microvm` | separate kernel via microsandbox | opt-in for hostile-code workloads |

Constraint inherited from h5i (fail-closed by design): **the Linux `process`
tier cannot enforce a domain allowlist** — its network modes are all-or-nothing
`deny`/`host`. Consequences:

- `senv run` with the default `net = "deny"` works everywhere, at the lightest
  tier. This is the common case and it is cheap.
- `senv sync` needs `supervised` (requires `slirp4netns`, `nft`, cgroup-v2
  delegation) or `container` (Podman). On macOS, Seatbelt enforces host
  allowlists at the base tier, so installs work out of the box.
- On hosts with neither (bare CI runners, some WSL2 setups), `senv sync` is
  **refused** with the `senv doctor` explanation. The user may explicitly
  configure `install.net = "host"` — accepted with a prominent warning in
  `status` and stamped into receipts, because an explicit, recorded downgrade
  is better than users abandoning the tool, but it is never chosen silently.

`senv doctor` wraps h5i's host probe and reports exactly which phases this
host can enforce, before anything fails mid-workflow.

Platform support follows h5i: Linux and macOS at launch. Windows only via
WSL2. macOS caveats surfaced by `status`: no seccomp equivalent, memory caps
not enforceable under Seatbelt.

---

## 7. Configuration — `senv.toml`

Project root, checked in, and entirely optional. Users write Python-project
vocabulary; senv compiles it to h5i `Profile`s by *narrowing* the builtin
fail-closed base, so a field senv forgets stays safe rather than becoming
empty.

```toml
[env]
python = "3.13"
isolation = "auto"            # auto | process | supervised | container | microvm
image = "..."                 # required by the container / microvm tiers
uv = "/opt/bin/uv"            # when uv is not on PATH

[install]
extra-indexes = ["download.pytorch.org"]
net = "registries"            # "registries" (default) | "host" (warned downgrade)
cache = "project"             # "project" (default) | "shared"
read = ["~/wheels"]           # extra read-only grants during installs
project-writable = false      # true only if a build backend must write your source
[install.resources]
mem = "8G"
wall = "60m"

[run]
net = "deny"                  # "deny" | "host" | ["api.example.com", "*.s3.amazonaws.com"]

[run.fs]
read  = ["~/datasets"]        # extra ro grants
write = []                    # the project and a scratch dir are already writable

[run.env]
pass = ["MY_APP_MODE"]        # added to senv's baseline, never replacing it

[run.resources]
mem = "4G"
wall = "30m"                  # "none" for a dev server
procs = 256

[secrets.OPENAI_API_KEY]
source = "env:OPENAI_API_KEY" # or file:… / command:…
inject = "env"
phases = ["run"]              # "run" and/or "shell" — never "install"
```

The schema is `deny_unknown_fields` throughout: a misspelled key in a security
policy must be an error, never a silently-ignored line that reads as though it
were enforced. Host patterns are validated where the user can see them, and a
single-label wildcard (`.com`) is refused outright — it is the one typo that
turns an allowlist into an open door.

Layering: built-in defaults ← `senv.toml` ← per-invocation flags
(`senv run --allow-net api.example.com -- python app.py`). Widening beyond
`senv.toml` requires a flag, and the flag is announced and recorded.

Two schema decisions worth stating. `[env] isolation = "workspace"` is
**rejected**: that h5i tier applies no confinement, and senv has no unconfined
execution path — someone who wants one should use uv. And a secret can never
name the install phase, because a credential visible to a dependency's build
backend is a credential handed to an attacker's `setup.py`.

`wall = "none"` resolves the design's second open question. h5i refuses to
express an unbounded wall clock, correctly — a confined command that can never
be killed is a resource leak with a policy file. senv expresses `none` as one
year: longer than any `uvicorn` session, still a real kill switch, still in the
digest. The dev-server case is served without weakening the engine.

---

## 8. State, integrity, and caching

```
project/                      # senv adds nothing here that was not already yours
  senv.toml                   # optional policy source (checked in)
  pyproject.toml, uv.lock     # uv's domain (checked in)
  .venv -> …/state/…/venv     # a symlink, so editors and language servers work

~/.local/state/senv/projects/<name>-<hash>/
  venv/                       # the environment itself
  cache/                      # this project's wheel cache (default)
  scratch/                    # run-phase writable scratch, incl. the pycache prefix
  tmp/{install,run,provision} # per-phase TMPDIR
  stage/                      # manifest staging for lock/add/remove
  receipt.jsonl               # append-only: what ran, denials, digests
  policy.{install,run}.toml   # the resolved policy, for inspection
  state.json                  # provenance, lock hash, last digests

~/.cache/senv/
  uv/                         # shared wheel cache (opt-in)
  python/                     # uv-managed interpreters, shared, provisioned alone
```

**Everything senv writes lives outside the project tree.** This changed during
implementation and it is the most consequential change in the document. The
original design put `.senv/` — receipts and resolved policies — inside the
project. But the run phase grants the project read-write, which is the whole
point: your code edits your files. Anything stored there is therefore writable
by the very code the boundary exists to contain. **Receipts a compromised
package can rewrite are not evidence**, and h5i keeps its own receipts outside
every box grant for exactly this reason. An integration test asserts that a
process under a senv run policy cannot write the receipt log.

Moving the environment out of tree came with the same move, and made the
read-only guarantee stronger rather than merely stated: with the venv outside
the project, no writable grant contains it. `.venv` remains a symlink so
editors, language servers, and `source .venv/bin/activate` all still resolve —
Landlock evaluates the resolved target, so a sandboxed process that replaces
the symlink gains nothing.

The side effect is the best thing about it for adoption: **senv adds no files
to your project tree**. A senv project is a uv project with an optional
`senv.toml`.

- **Provenance.** `state.json` records whether the environment was built inside
  the boundary (`sandboxed`) or adopted from a `.venv` that already existed
  (`host-installed`). `senv status` reports it. senv does not claim a boundary
  over bytes that never passed through one.
- **Integrity.** Each phase's resolved policy is sha256-digested and the digest
  is stamped into every receipt line, so "which policy was actually enforced
  when this ran" stays answerable. The `policy.*.toml` files are written for
  inspection and never read back as input: senv recompiles the policy on every
  invocation.
- **Receipts** reuse h5i's model: append-only JSONL, secret values stripped
  verbatim before writing, then h5i's credential scanner over what remains.
  Output is kept only when a command failed — a successful command's output is
  the user's business, not the log's.
- **Caching is per-project by default**, a change from the design's shared
  lock-keyed cache. A wheel cache is written *during* the install phase, which
  is when third-party build backends run; sharing one across projects means a
  compromised install in project A can reach project B. Per-project costs disk
  and is the fail-closed default; `cache = "shared"` is one line for anyone who
  wants the disk back. Interpreters stay shared because they are large — and
  are only ever written by the provisioning phase, which runs no third-party
  code at all, and are read-only everywhere else.

### Concurrent invocations

Two senv commands running at once is ordinary — a CI matrix, or a dev server
beside a manual sync — and it turned out to break tier selection. h5i's cgroup
probe proves delegation by creating a *fixed* scratch cgroup, writing to it and
removing it, so two processes probing simultaneously delete each other's
scratch directory. The loser concludes the host cannot delegate cgroups and
refuses the install with "this host cannot enforce a network allowlist" — on a
host that plainly can. Measured at roughly one failure in eight with eight
concurrent syncs.

senv serializes the probe behind a machine-global advisory lock and warms it
once per process, since h5i caches the result per process. Zero failures in 32
concurrent syncs afterwards, and an integration test holds the line.

The underlying race belongs upstream — the probe path wants a pid in it — and
this is a workaround, not a fix. It is worth stating because the failure mode
is exactly the one senv must never have: a boundary that reports itself
unavailable when it is available teaches people to turn it off.

---

## 9. Distribution and naming

- **Name: `senv`** — short, clean CLI (`senv sync`, `senv run pytest`), repo
  `h5i-dev/senv`. Known collisions (an old env-var manager named senv, a
  dormant Poetry-like project) are weak; no installable `senv` exists on PyPI
  today. Tagline disambiguates: *"A security boundary for Python
  environments."*
- **Distribution: Rust binary** via GitHub Releases, `cargo install`, and
  Homebrew — sidestepping the PyPI name question entirely, and avoiding the
  circularity of shipping a Python-install boundary through a Python install.
  A PyPI shim is a later, optional convenience.
- senv depends on `h5i-sandbox` by **git revision**, pinned to a commit rather
  than a branch: the policy-digest story depends on the enforcement code being
  the code that was reviewed.

---

## 10. Status and roadmap

**v0.1 — built.** Every command in §3, all three phases, `senv.toml`
compilation, tier auto-selection with fail-closed refusals, staging with
verified copy-back, out-of-tree state, receipts with redaction, denial
inference with `report --suggest`, provenance tracking, adoption of existing uv
projects. 61 unit tests and 19 integration tests; the integration suite drives
the real binary against the real kernel sandbox and asserts the guarantees
themselves — the environment is unwritable at run time, credentials are
unreachable, the network is denied, receipts cannot be tampered with from
inside.

**Next**

- Container and microVM tiers exercised in CI, not just reachable by config.
- Authenticated private indexes through h5i's credential proxy, so an index
  token never enters the install phase at all.
- `[tool.senv]` in `pyproject.toml` as an alternative config home.
- `senv exec` shims, so `.venv/bin/python` is confined even when invoked
  directly by an editor or a task runner.
- A per-request egress tally at the kernel tiers, which today only the
  container tier provides — this is the main gap between what `senv report`
  shows and what actually happened.

---

## 11. Decisions and open questions

Resolved during implementation:

- **Degraded installs**: explicit `[install] net = "host"` is the escape hatch
  on hosts that cannot enforce an egress allowlist — never chosen silently,
  warned in `status`, stamped into every receipt. Refusal with no path forward
  would lose users to plain uv, which is strictly worse.
- **h5i coupling**: a git dependency pinned to a commit hash. Revisit crates.io
  only if the pinning workflow becomes painful.
- **`uv.lock` writes during installs** (design open question 1): solved by
  staging, with a verified copy-back and an announced in-place fallback for
  projects that cannot be staged (§5).
- **Dev-server wall clocks** (design open question 2): `wall = "none"` compiles
  to a finite one-year limit, so h5i's "always a kill switch" invariant holds
  and the config still reads the way a user thinks (§7).
- **Where state lives**: outside the project, because the run phase grants the
  project read-write and receipts inside it would be rewritable by the code
  they record (§8).
- **Cache scope**: per-project by default; sharing is opt-in (§8).

Still open:

- **Egress evidence at the kernel tiers.** Denials are inferred from what a
  program printed, which is a strong lead and not an audit log. The supervised
  tier enforces by address but keeps no per-request tally. Closing this
  properly probably means asking h5i for one.
- **Non-Python ecosystems.** The policy engine is language-agnostic and the
  phase split (resolve/install/run) generalises. Whether senv should grow into
  that, or stay the Python tool whose name says so, is a positioning question
  rather than a technical one.
- **A shared-cache poisoning story.** Per-project caching sidesteps it rather
  than solving it. Content-addressed verification on cache reads would let the
  shared cache be the safe default again.
