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
| **Limits** | memory, processes, wall clock, file size | same, configurable |

Two of those deserve a note.

**Installs are sandboxed too.** Supply-chain attacks fire at install time — a
malicious sdist's build backend runs as you, during `pip install`, before
anyone imports anything. senv resolves dependencies in a staging copy holding
your manifests and none of your source, with egress pinned to package
registries at the packet level.

**The environment is read-only while your code runs.** A compromised package
cannot patch itself on disk to survive into the next run. It lives outside your
project tree with `.venv` symlinked to it, so there is no writable parent to
reach it through.

## Install

```bash
cargo install --git https://github.com/h5i-dev/senv
```

Or download a binary from [Releases](https://github.com/h5i-dev/senv/releases).

senv needs [uv](https://docs.astral.sh/uv/) on `PATH`. On Linux, enforcing the
install boundary's registry allowlist also needs `slirp4netns` and `nftables`
(`sudo apt install slirp4netns nftables` on Debian/Ubuntu). Run `senv doctor` —
it reports exactly what this machine can enforce and what to install if
something is missing.

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

## Configuration

`senv.toml` is optional. With no config you get the fail-closed defaults above;
the file appears the first time you widen something.

```toml
[env]
python = "3.13"
isolation = "auto"              # auto | process | supervised | container | microvm

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
source = "env:OPENAI_API_KEY"   # or file:… / command:…
phases = ["run"]                # never the install phase — that runs build backends
```

Unknown keys are an error, not a shrug: a misspelled key in a security policy
would otherwise read as enforced while enforcing nothing.

## Inspecting the boundary

```bash
senv status      # tier, network, grants, limits, and the policy digest per phase
senv report      # what ran, what was denied, what was redacted
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
  (no syscall filter; memory caps are not enforceable).
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
- Linux and macOS. Windows via WSL2.

See [DESIGN.md](DESIGN.md) for the threat model and the reasoning behind each
decision.

## License

Apache-2.0.
