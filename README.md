<h1 align="center">senv</h1>

<p align="center"><strong>A security boundary for Python environments.</strong></p>

<p align="center">
  Persistent, sandboxed Python environments powered by
  <a href="https://github.com/astral-sh/uv">uv</a> and
  <a href="https://github.com/h5i-dev/h5i">h5i</a>.
</p>

---

`venv` and `uv` isolate *dependencies*. They do not isolate *code*. A package
installed in a virtualenv can still read your SSH keys, upload your environment
variables, and run arbitrary code from a `setup.py` at install time — a
virtualenv is a `PATH` convention, not a security boundary.

senv keeps uv's workflow and puts an OS-level boundary underneath it:

```bash
senv sync                 # installs reach PyPI and nothing else
senv run pytest           # your code runs with no network and a read-only environment
senv shell                # same boundary, interactive
senv status               # exactly what is enforced right now
```

Nothing about your project changes. A senv project **is** a uv project:
`pyproject.toml` and `uv.lock` stay where they are, teammates without senv keep
running `uv sync`, and opting out means typing `uv` again.

## What it actually enforces

Installing and running are different trust problems, so they get different
policies.

| | `senv sync` / `add` / `lock` | `senv run` / `shell` |
|---|---|---|
| **Network** | PyPI + your extra indexes, nothing else | denied by default |
| **Your source** | read-only — a build backend cannot edit it | read-write, it's your code |
| **The environment** | writable (this is what installs it) | **read-only** |
| **Credentials** | none; secrets can never be scoped here | only what you declare |
| **Limits** | CPU, file size, wall clock; memory and processes on Linux | same, minus the wall clock (see below) |

Two of those deserve a note.

**Installs are sandboxed too.** Supply-chain attacks fire at install time — a
malicious sdist's build backend runs as you, during `pip install`, before
anyone imports anything. senv resolves dependencies in a staging copy holding
your manifests and none of your source, with egress pinned to package
registries at the packet level.

**The environment is read-only while your code runs.** A compromised package
cannot patch itself on disk to survive into the next run. It lives outside your
project tree with `.venv` symlinked to it, so there is no writable parent to
reach it through — and there is no writable bytecode cache either, since a
`.pyc` CPython prefers over the source would be a writable copy of the very
code being protected. Bytecode is compiled during installation, inside the
environment, where your code cannot rewrite it.

## Install

```bash
cargo install --git https://github.com/h5i-dev/senv
```

Or download a binary from [Releases](https://github.com/h5i-dev/senv/releases).

senv needs [uv](https://docs.astral.sh/uv/) on `PATH`. On Linux, enforcing the
install boundary's registry allowlist also needs `slirp4netns` and `nftables`
(`sudo apt install slirp4netns nftables` on Debian/Ubuntu). On macOS there is
nothing to install: the boundary is Seatbelt, which ships with the OS. Run
`senv doctor` — it reports exactly what this machine can enforce and what to
install if something is missing.

## Use it

### Start, or adopt what you have

```bash
senv init                 # in an existing uv project: adopts pyproject.toml as-is
senv init --python 3.13   # in an empty directory: creates one
```

In a project that already has a `.venv`, senv leaves it alone — those packages
were installed outside the boundary, so senv will not pretend otherwise. It
builds its own environment and tells you; `senv init --replace-venv` swaps the
link over when you're ready.

### Day to day

| habit | senv |
|---|---|
| `uv sync` / `uv lock` | `senv sync` / `senv lock` |
| `uv add requests` | `senv add requests` |
| `uv run pytest` | `senv run pytest` |
| `source .venv/bin/activate` | `senv shell` |
| any other uv command | `senv uv -- <args>` |

Flags pass through to uv, and the command's exit code passes back out — `senv
run pytest` is a drop-in for `uv run pytest` in CI.

### When something is blocked

The first week of any sandbox is denials. senv answers each one with the exact
change that would allow it:

```
senv blocked 1 operation(s):
  • network access to api.stripe.com
    allow it with: senv allow api.stripe.com
```

```bash
senv allow api.stripe.com        # records it in senv.toml
senv run --allow-net api.stripe.com pytest   # just this once
senv report --suggest            # a policy stanza covering everything blocked so far
```

`report --suggest` prints; it never writes. Widening a boundary stays a
decision you make and commit.

Some denials are deliberate, and senv says so rather than offering to undo
them — writing to the environment at run time, or reading `~/.ssh`, are the
guarantees, not bugs.

## The policy lives in your project, so senv watches it

`senv.toml` sits in your project directory — which the run phase grants
read-write, because that is the point of the run phase. So the policy file is
writable by exactly the code it governs. A package that runs once could
otherwise rewrite it and own every later command.

senv keeps a snapshot of the policy you accepted **outside** the sandbox, and
compares before it runs anything:

- narrowed, or unchanged → nothing to say, it just runs;
- **widened** → refused, with the change named:

```
senv: senv.toml grants more than senv recorded, so nothing was run
  the policy on disk is wider than the one you last accepted:
    + [run] net: deny → unrestricted
    + [run.env] pass: added AWS_SECRET_ACCESS_KEY
  senv.toml lives in your project, which your code can write — so a change it
  did not make is a change worth looking at before running anything.
  → if you made this change, run `senv trust` to accept it; if you did not,
    inspect senv.toml and your recent dependencies first
```

`senv trust` accepts the current file as the baseline. Editing your own policy
costs one extra command; a package editing it for you costs the attack.

The same applies the first time senv sees a project: if its `senv.toml` already
grants more than the defaults, senv shows you what and waits for `senv trust`.
That is the right moment to read a policy that arrived with someone else's
code — and it is what stops a package from manufacturing a fresh project in a
subdirectory to escape its own baseline.

Two settings get extra treatment because they reach outside the sandbox:
a secret with a `command:` source needs `[env] allow-command-secrets = true`
(the command runs on the host), and `[env] uv` is refused outright if it points
inside your project or senv's state — a binary the sandbox can rewrite must
never become senv's own toolchain.

## Configuration

`senv.toml` is optional. With no config you get the fail-closed defaults above;
the file appears the first time you widen something.

```toml
[env]
python = "3.13"
isolation = "auto"              # auto | process | supervised | container | microvm
allow-command-secrets = false   # true lets a secret source run host code OUTSIDE the sandbox

[install]
extra-indexes = ["download.pytorch.org"]
cache = "project"               # per-project by default; "shared" trades isolation for disk

[run]
net = "deny"                    # "deny" | "host" | ["api.example.com", "*.s3.amazonaws.com"]

[run.fs]
read = ["~/datasets"]           # extra read-only grants
write = []                      # the project and a scratch dir are already writable

[run.resources]
mem = "4G"
wall = "30m"                    # "none" for a dev server

[secrets.OPENAI_API_KEY]
source = "env:OPENAI_API_KEY"   # or file:… / command:… (command: needs the gate below)
phases = ["run"]                # never the install phase — that runs build backends
```

Unknown keys are an error, not a shrug: a misspelled key in a security policy
would otherwise read as enforced while enforcing nothing.

## Inspecting the boundary

```bash
senv status      # tier, network, grants, limits, and the policy digest per phase
senv report      # what ran, what was denied, what was redacted
senv trust       # accept the current senv.toml as the baseline
senv doctor      # what this host can enforce
senv gc          # state for projects that no longer exist (dry run unless --prune)
```

Every command's execution is appended to a receipt log in senv's state
directory — outside every grant senv issues, so the code it records cannot
rewrite it. Declared secret values are stripped before anything is written, and
h5i's credential scanner takes a second pass for keys nobody declared.

Each phase's resolved policy is hashed, and that digest is stamped into every
receipt: "which policy was actually enforced when this ran" stays answerable
afterwards.

## How it works

senv links [`h5i-sandbox`](https://github.com/h5i-dev/h5i) as a library and
compiles `senv.toml` into its policy model. h5i supplies the confinement; senv
supplies the Python-shaped policy on top of it.

- **Linux**: Landlock (filesystem allowlist), seccomp-bpf (syscall deny-list),
  namespaces, rlimits/cgroups. Egress allowlists use a private network
  namespace with nftables rules pinned to resolved addresses — enforced by
  address, so a program that ignores `HTTPS_PROXY` still cannot get out.
- **macOS**: Seatbelt, with the differences reported honestly by `senv doctor`
  and `senv status`. There is no syscall filter (Darwin has no seccomp) and no
  enforceable memory or process cap (no cgroups, and `RLIMIT_AS` does not bind
  the mmap'd heap CPython uses). Egress allowlists take a different route:
  Seatbelt leaves the box exactly one destination, the loopback port of senv's
  DNS-pinned allowlist proxy, and denies name resolution outright. So the
  allowlist holds against any client — but a client that ignores `HTTPS_PROXY`
  reaches *nothing* rather than reaching its host directly. `net = "deny"` is a
  real deny on both platforms.
- Stronger tiers (rootless Podman containers, microVMs with their own kernel)
  are available by setting `[env] isolation`.

senv never downgrades silently. If a host cannot enforce what a policy asks
for, the command is refused and told what would fix it.

## Limits

Stated plainly, because a boundary you misjudge is worse than one you don't
have:

- The default tiers share your kernel. Landlock and seccomp are a real
  boundary, not a hypervisor — for VM-grade isolation, use `isolation = "microvm"`.
- senv narrows the blast radius of a malicious package; it does not detect one.
  Verifying what you install is uv's lockfile hashes and your own judgement.
- Denials are inferred from what a program printed when it was refused, so
  `senv report` is a strong lead and not an audit log. Container-tier runs get a
  real per-request egress tally; the kernel tiers do not.
- Code that stays inside the policy — corrupting files in your own project,
  reaching a host you allowlisted — is within policy. That is what the policy
  is for.
- The wall-clock limit applies to installs but **not** to `senv run` / `senv
  shell`: the interactive path hands the terminal to the child and waits
  without a deadline. CPU time and file size are rlimits and apply everywhere.
  Memory and process count are a per-run cgroup, so they apply on Linux and
  **not on macOS**, which has none. `senv status` marks every limit this host
  does not actually enforce, and `senv doctor` answers it for the machine.
- On macOS, `senv run` startup depends on which interpreter built the
  environment. senv compiles bytecode during the install so the run phase needs
  no writable cache; Apple's system Python ships with `sys.pycache_prefix`
  preset to `~/Library/Caches/com.apple.python` and caches outside the
  environment instead, so imports recompile every run. `senv status` says so
  when it happens — `senv init --python 3.13` builds on a managed interpreter
  that does not.
- senv detects a policy widened behind your back; it cannot prevent the write.
  "Policy" includes `pyproject.toml`'s `[build-system]` and `[tool.uv]` tables,
  which decide what code an install runs — changing either needs `senv trust`.
  A package can still edit the rest of your project, so review dependency
  changes you did not make.
- Linux and macOS. Windows via WSL2.

See [DESIGN.md](DESIGN.md) for the threat model and the reasoning behind each
decision.

## License

Apache-2.0.
