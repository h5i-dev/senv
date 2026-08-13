# senv — Design Document

> **senv** — A security boundary for Python environments.
> Persistent, sandboxed Python environments powered by [uv](https://github.com/astral-sh/uv) and [h5i](https://github.com/h5i-dev/h5i).

Status: draft v0.1 · 2026-08-13

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
| Malicious sdist build backend / `setup.py` | `senv sync` / `add` | install phase runs sandboxed: egress limited to package registries, writes limited to venv + wheel cache, no secrets in env |
| Malicious package code at runtime (typosquats, hijacked releases) | `senv run` / `shell` | network deny by default; fs writes confined to the project; `~/.ssh`, `~/.aws`, cloud credentials unreadable |
| Credential/env exfiltration | both | env allowlist (`PATH`, `HOME`, `LANG`, `TERM` by default); secrets only via explicit declaration; secret-pattern redaction in logs |
| Environment self-modification / persistence (malware editing the venv at runtime) | run | venv is mounted **read-only** at run time; only install boxes may write it |
| Resource exhaustion (fork bombs, memory balloons, runaway jobs) | both | rlimits + cgroups: memory, process count, wall clock (default 30 min), file size |
| Tampering with the enforced policy | — | resolved policy is digested; the digest is pinned in state and stamped into every execution receipt |

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

senv inherits h5i's fail-closed philosophy: when a host cannot enforce a
policy, the operation is **refused with an explanation**, never silently
weakened.

---

## 3. CLI surface

uv-shaped, minimal, no container vocabulary:

```
senv init [--python <ver>]     create senv.toml, .senv/, and the venv (sandboxed)
senv add <pkg>...              uv add, inside the install boundary
senv remove <pkg>...           uv remove, inside the install boundary
senv sync                      uv sync, inside the install boundary
senv lock                      uv lock, inside the install boundary
senv run <cmd> [args...]       run a command inside the run boundary
senv shell                     interactive confined shell (commands recorded)
senv status                    enforced policy, isolation tier, policy digest
senv report                    what ran, what was denied, what was redacted
senv allow <host>              add an egress host to the run policy
senv doctor                    probe host capabilities; explain available tiers
senv gc                        prune stale caches and state
senv uv -- <args...>           escape hatch: arbitrary uv command, install boundary
```

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
only *adds* files (`senv.toml` — optional, `.senv/` — gitignored); it never
changes the format or location of `pyproject.toml`, `uv.lock`, or `.venv`.
Consequences:

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
│  phase policies (install / run)                │
│  venv + cache lifecycle (drives uv)            │
│  receipts & reporting                          │
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

1. **Phase-specific policies** (install vs. run — §5), senv's core idea.
2. **venv lifecycle**: driving uv, keeping the venv read-only at run time,
   interpreter installs.
3. **`senv.toml`**: a Python-project-shaped config surface that compiles down
   to h5i profiles, so users never write h5i policy by hand.
4. **Wheel/interpreter caches** keyed by lockfile hash (borrowing h5i's
   refresh-box pattern — §8).

---

## 5. The two security phases

senv's central design decision: **installation and execution are different
trust problems and get different policies.** Existing tools sandbox one or
neither; supply-chain attacks overwhelmingly fire at install time (build
backends, `setup.py`, post-install hooks), while data exfiltration fires at
run time.

### Phase A — install boundary (`sync` / `add` / `remove` / `lock` / `init`)

Runs `uv` confined. Modeled directly on h5i's cache-refresh box: the install
runs *alone* — no agent, no app code of yours — with exactly one writable
window and network narrowed to registries.

| Dimension | Policy |
|---|---|
| filesystem read | system paths, the project directory, the uv binary, shared caches (ro) |
| filesystem write | `.venv/`, the senv wheel cache, `uv.lock`, `pyproject.toml` (for `add`/`remove`) — nothing else |
| network | egress allowlist: `pypi.org`, `files.pythonhosted.org`, plus configured extra indexes; interpreter downloads additionally allow `github.com` + `objects.githubusercontent.com` during `init --python` only |
| env | minimal allowlist; **no secrets, ever** — index credentials go through h5i's authenticated-egress proxy so tokens never enter the box |
| resources | mem 4G, procs 256, wall 30m, fsize caps (h5i defaults) |

A malicious build script can therefore: burn CPU inside its limits, write junk
into the venv it is building, and talk to PyPI. It cannot read your SSH keys,
phone home to an attacker host, or touch anything outside the venv.

### Phase B — run boundary (`run` / `shell`)

| Dimension | Policy (default) |
|---|---|
| filesystem read | system paths, the project, `.venv/` (**read-only**), shared caches (ro) |
| filesystem write | the project directory and a scratch dir — not the venv |
| network | **deny** (opt into hosts via `senv allow` / `senv.toml`) |
| env | `PATH`, `HOME`, `LANG`, `TERM`, `COLORTERM` + declared secrets only |
| resources | same defaults; all configurable |

The read-only venv is a deliberate security property: runtime code cannot
patch installed packages to persist across runs. Bytecode caching still works —
senv sets `PYTHONPYCACHEPREFIX` to a scratch directory so a read-only venv
costs nothing.

`net = "deny"` uses a network namespace and works at the lightest tier on
every Linux kernel — the common case (run untrusted code with no network) has
no exotic host requirements. Only egress *allowlists* need a stronger tier
(§6).

Secrets follow h5i's broker model: declared by id in config, sourced from
`env:`/`file:`/`command:`, injected only into runs that name them, never
written to logs (fingerprint only), and scrubbed from receipts by the
redaction scanner.

---

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

Project root, checked in. Users write Python-project vocabulary; senv compiles
it to h5i `Profile`s (inheriting the builtin fail-closed base, so an omitted
key means "safe default", and an explicit empty list means "empty").

```toml
[env]
python = "3.13"
isolation = "auto"            # auto | process | supervised | container | microvm

[install]
# extra registries beyond pypi.org / files.pythonhosted.org
extra-indexes = ["download.pytorch.org"]

[run]
net = "deny"                  # "deny" | "host" | ["api.example.com", ".s3.amazonaws.com"]

[run.fs]
write = ["$PROJECT"]          # $PROJECT and a scratch dir; venv is always ro at run time
read  = ["~/datasets"]        # extra ro grants

[run.env]
pass = ["PATH", "HOME", "LANG", "TERM", "MY_APP_MODE"]

[run.resources]
mem = "4G"
wall = "30m"
procs = 256

[secrets.OPENAI_API_KEY]
source = "env:OPENAI_API_KEY" # or file:… / command:…
inject = "env"
```

Layering: built-in defaults ← `senv.toml` ← per-invocation flags
(`senv run --allow-net api.example.com -- python app.py`). Every layer can
narrow; widening beyond `senv.toml` requires a flag that `report` records.
A `[tool.senv]` table in `pyproject.toml` may later be accepted as an
alternative home; `senv.toml` is primary in v1 to keep the schema honest.

---

## 8. State, integrity, and caching

```
project/
  senv.toml                  # policy source (checked in)
  pyproject.toml, uv.lock    # uv's domain (checked in)
  .venv/                     # real uv-managed venv → IDEs/LSPs just work
  .senv/                     # gitignored
    policy.resolved.toml     # resolved install+run policies, sha256-digested
    receipt.jsonl            # append-only: what ran, denials, redactions, digest
    scratch/                 # run-phase writable scratch (incl. pycache prefix)
~/.cache/senv/
  uv/<lock-hash>/            # wheel cache, keyed by hash of lockfile contents
  python/                    # uv-managed interpreters
```

- **The venv lives at `.venv/`**, exactly where uv puts it, so editors,
  language servers, and `PATH` conventions work unmodified. senv's guarantee
  is not *where* the venv is but *who may write it*: only install boxes.
- **Integrity**: the resolved policy digest is pinned and stamped into every
  receipt line. When `senv.toml` changes, senv re-resolves and prints a policy
  diff before the next confined run. `senv status` shows the digest; receipts
  make "what policy was actually enforced when this ran" answerable later.
- **Receipts** reuse h5i's model: append-only JSONL written from outside the
  box's write grants, secret-scrubbed before write, denials recorded as
  structured findings. `senv report` renders them.
- **Caching** borrows h5i's warm-cache design: the shared wheel cache is keyed
  by lockfile-content hash, offered read-only to run boxes, and writable only
  inside install boxes — a stale or poisoned-by-another-project cache is never
  handed out, and run-time code can never seed the cache.

---

## 9. Distribution and naming

- **Name: `senv`** — short, clean CLI (`senv sync`, `senv run pytest`), repo
  `h5i-dev/senv`. Known collisions (an old env-var manager named senv, a
  dormant Poetry-like project) are weak; no installable `senv` exists on PyPI
  today. Tagline disambiguates: *"A security boundary for Python
  environments."*
- **Distribution: Rust binary** via GitHub Releases, `cargo install`, and
  Homebrew — sidestepping the PyPI name question entirely. A PyPI shim
  (`pip install senv` fetching the binary) is a later, optional convenience;
  if pursued, name availability must be verified at upload time first.
- senv depends on `h5i-sandbox` via a pinned git tag until h5i publishes to
  crates.io.

---

## 10. Roadmap

**v0.1 — MVP, the boundary works**
`init` / `sync` / `add` / `remove` / `run` / `status` / `doctor`.
Install boundary on Linux `supervised` + macOS Seatbelt; run boundary with
`net=deny` at the `process` tier. `senv.toml` → Profile compilation, resolved
digest, `.venv` read-only at run time. `init` adopts existing uv projects
(`pyproject.toml` / `uv.lock` / `.venv` detection, sandboxed-rebuild offer).

**v0.2 — legible security**
`shell` (recorded interactive sessions), `report` (incl. `--suggest` policy
stanzas from recorded denials), receipts, `allow`, shared lock-keyed wheel
cache, `gc`, denial messages with suggested fixes.

**v0.3 — tiers and reach**
`container` and `microvm` tiers, authenticated private indexes via the egress
auth proxy, `[tool.senv]` in pyproject, CI mode (`--json` everywhere,
non-interactive refusals).

**Later**
`senv exec` shims (`.venv/bin/python` transparently confined), per-dependency
policy experiments, non-Python ecosystems if h5i's ecosystem table grows.

---

## 11. Decisions and open questions

Decided:

- **Degraded-install UX**: explicit `install.net = "host"` is allowed as the
  escape hatch on hosts that can't enforce egress allowlists — never chosen
  silently, always warned in `status` and stamped into receipts.
  Refusal-with-no-path would lose users to plain uv, which is strictly worse.
- **h5i version coupling**: depend on `h5i-sandbox` via a git dependency
  pinned to a tag or commit hash. Revisit crates.io publishing only if the
  pinning workflow becomes painful.

Open:

1. **`uv.lock` writes in phase A.** `add`/`lock` must write
   `pyproject.toml`/`uv.lock` in the project root, slightly widening the
   install write set. Acceptable (they're data files senv can diff in
   receipts), but worth a second look versus staging them in the box and
   copying out after validation.
2. **Watch/dev-server workflows.** Long-running `senv run` with wall-clock
   limits: default 30m is right for tasks, wrong for `senv run
   uvicorn`. Likely a `[run.resources] wall = "none"`? h5i deliberately
   refuses unbounded walls — needs a decision with h5i's model in mind.
