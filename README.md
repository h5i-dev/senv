<h1 align="center">senv</h1>

<p align="center"><strong>Sandboxed Python environments, with the workflow of uv.</strong></p>

<p align="center">
  <a href="https://github.com/h5i-dev/senv/blob/main/LICENSE"><img alt="Apache-2.0" src="https://img.shields.io/github/license/h5i-dev/senv?color=blue"></a>
  <a href="https://github.com/h5i-dev/senv/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/h5i-dev/senv?style=social"></a>
</p>

**senv** adds an OS-level security boundary to Python environments. It keeps
the familiar [`uv`](https://github.com/astral-sh/uv) workflow while isolating
dependency installation and application execution from your credentials,
network, and the rest of your machine.

<table align="center">
<tr>
<td>📦 Install packages with registry-only network access</td>
<td>🔒 Run Python with no network by default</td>
</tr>
<tr>
<td>🧊 Keep the environment read-only while code runs</td>
<td>🧾 Review policy, denials, and execution receipts</td>
</tr>
</table>

```bash
senv sync                 # install dependencies inside the install sandbox
senv run pytest           # run code with no network and a read-only environment
senv shell                # enter the same boundary interactively
senv status               # see exactly what is enforced on this machine
```

A senv project is still a uv project. It uses the same `pyproject.toml` and
`uv.lock`, and teammates without senv can continue using `uv` directly.

---

## Why senv?

`venv` and `uv` isolate dependencies, but they do not isolate code. A package
inside a virtual environment can still:

- read files such as `~/.ssh`;
- access environment variables and credentials;
- connect to arbitrary network destinations; and
- execute build code during installation.

A virtual environment is a `PATH` convention, not a security boundary. senv
puts a sandbox underneath the Python workflow, with separate policies for the
two moments that carry different risks:

- **Install:** package build code can reach approved registries, but not your
  source tree or credentials.
- **Run:** your project is writable, but the installed environment is
  read-only and network access is denied by default.

---

## Install

```bash
cargo install --git https://github.com/h5i-dev/senv
```

senv requires [`uv`](https://docs.astral.sh/uv/) on `PATH`.

- **Linux:** registry allowlisting during installation also requires
  `slirp4netns` and `nftables` (`sudo apt install slirp4netns nftables` on
  Debian/Ubuntu).
- **macOS:** no additional sandbox runtime is required; senv uses the built-in
  Seatbelt sandbox.
- **Windows:** use WSL2.

Check what your machine can enforce:

```bash
senv doctor
```

---

## Quick start

### Start a new project

```bash
mkdir my-project && cd my-project
senv init --python 3.13
senv add requests
senv run python -c 'import requests; print(requests.__version__)'
```

### Adopt an existing uv project

```bash
cd my-project
senv init                 # keeps pyproject.toml and uv.lock as-is
senv sync
senv run pytest
```

If the project already has a `.venv`, senv leaves it untouched because those
packages were installed outside its boundary. It creates a separate managed
environment and explains how to switch. Use `senv init --replace-venv` when
you are ready to replace the existing link.

---

## Everyday commands

| What you normally do | With senv |
| --- | --- |
| `uv sync` | `senv sync` |
| `uv lock` | `senv lock` |
| `uv add requests` | `senv add requests` |
| `uv run pytest` | `senv run pytest` |
| `source .venv/bin/activate` | `senv shell` |
| another uv command | `senv uv -- <args>` |

Flags are passed through to uv, and command exit codes pass back to the caller.
This makes commands such as `senv run pytest` suitable for CI as well as local
development.

---

## What senv enforces

Installing dependencies and running your code use different policies:

| | `senv sync` / `add` / `lock` | `senv run` / `shell` |
| --- | --- | --- |
| **Network** | PyPI and configured indexes only | denied by default |
| **Project source** | read-only | read-write |
| **Python environment** | writable for installation | **read-only** |
| **Credentials** | unavailable | only explicitly declared secrets |
| **Resource limits** | CPU, file size, wall clock; memory and processes on Linux | CPU and file size; memory and processes on Linux |

### Sandboxed installs

Supply-chain attacks can execute before a package is ever imported. For
example, an sdist build backend runs during installation with the user's
permissions.

senv resolves and installs dependencies in a staging area containing the
project manifests but none of the project source. Network access is restricted
to package registries and pinned at the network layer.

### A read-only runtime environment

While your code runs, installed packages cannot rewrite the environment to
persist into the next run. The managed environment lives outside the project,
and `.venv` points to it without exposing a writable parent directory.

senv also compiles bytecode during installation and disables writable bytecode
caches at runtime. This prevents a package from leaving behind a modified
`.pyc` file that Python could prefer over the protected source.

---

## Allow only what your program needs

When senv blocks an operation, it shows what happened and the narrowest command
that would permit it:

```text
senv blocked 1 operation(s):
  • network access to api.stripe.com
    allow it with: senv allow api.stripe.com
```

You can make the grant persistent or apply it to one command:

```bash
senv allow api.stripe.com
senv run --allow-net api.stripe.com pytest
senv report --suggest
```

`senv allow` records the change in `senv.toml`. `senv report --suggest` only
prints a proposed policy stanza; it never changes the policy for you.

Some denials are intentional guarantees, so senv does not suggest bypassing
them. Runtime code cannot make the environment writable or read `~/.ssh`.

---

## Policy changes require trust

The policy file, `senv.toml`, lives inside the project. Because runtime code can
write the project, a compromised dependency could try to widen that policy for
future commands.

senv therefore keeps the last accepted policy snapshot outside the sandbox:

- an unchanged or narrower policy runs normally;
- a wider policy is refused until you inspect and accept it with `senv trust`.

```text
senv: senv.toml grants more than senv recorded, so nothing was run
  the policy on disk is wider than the one you last accepted:
    + [run] net: deny → unrestricted
    + [run.env] pass: added AWS_SECRET_ACCESS_KEY
  → if you made this change, run `senv trust`; if you did not,
    inspect senv.toml and your recent dependencies first
```

The same check applies when senv first sees a project whose policy is wider
than the defaults. Changes to `pyproject.toml` sections that control install
behavior—`[build-system]` and `[tool.uv]`—also require trust.

Two settings receive additional protection:

- a `command:` secret source requires `allow-command-secrets = true`, because
  the command executes on the host;
- the configured uv is rejected if it lives inside the project or senv
  state, where sandboxed code could rewrite it.

---

## Configuration

`senv.toml` is optional. Without it, senv uses fail-closed defaults. The file is
created when you first add a grant.

```toml
[env]
python = "3.13"
isolation = "auto"              # auto | process | supervised | container | microvm
allow-command-secrets = false   # permit command: secret sources on the host

[install]
extra-indexes = ["download.pytorch.org"]
cache = "project"               # "shared" saves disk but trades away isolation

[run]
net = "deny"                    # "deny" | "host" | ["api.example.com", "*.s3.amazonaws.com"]

[run.fs]
read = ["~/datasets"]           # additional read-only paths
write = []                      # project and scratch space are already writable

[run.resources]
mem = "4G"
wall = "30m"                    # use "none" for a dev server

[secrets.OPENAI_API_KEY]
source = "env:OPENAI_API_KEY"   # env:… | file:… | command:…
phases = ["run"]                # secrets are never exposed to install-time build code
```

---

## Inspect the boundary

```bash
senv status      # resolved policy, isolation tier, grants, limits, and digest
senv report      # commands, denials, and redactions
senv trust       # accept the current policy as the new baseline
senv doctor      # enforcement available on this host
senv gc          # find stale project state; add --prune to remove it
```

Every command appends a receipt outside the sandbox. Declared secret values are
redacted, and h5i's credential scanner checks for additional keys. Each receipt
includes a digest of the policy that was actually enforced.

---

## How it works

senv uses [`h5i-sandbox`](https://github.com/h5i-dev/h5i) as its confinement
engine and compiles the Python-focused `senv.toml` into an h5i policy.

| Platform / tier | Enforcement |
| --- | --- |
| **Linux** | Landlock filesystem allowlists, seccomp-bpf syscall filtering, namespaces, and rlimits/cgroups. Network allowlists use a private namespace and nftables rules pinned to resolved addresses. |
| **macOS** | Seatbelt filesystem and network confinement. Allowed egress passes through a DNS-pinned loopback proxy; other name resolution is denied. |
| **Container** | Optional rootless Podman isolation. |
| **MicroVM** | Optional VM-grade isolation with a separate kernel. |

senv never silently downgrades. If the host cannot enforce the requested
policy, it refuses to run and explains what is missing. `senv doctor` and
`senv status` report platform-specific gaps—for example, macOS has no seccomp
and cannot enforce Linux cgroup memory or process limits.

---

## Security boundaries and limitations

- **The default tiers share the host kernel.** Landlock, seccomp, and Seatbelt
  provide OS-level isolation, not a hypervisor boundary. Use
  `isolation = "microvm"` when a separate kernel is required.
- **senv limits impact; it does not identify malicious packages.** Lockfile
  hashes and dependency review remain important.
- **Allowed actions remain allowed.** Code can modify files in the writable
  project and contact destinations you explicitly permit.
- **Run the environment through senv.** `source .venv/bin/activate`, an editor
  invoking `.venv/bin/python`, or any other host-side execution bypasses the
  runtime boundary. Use `senv run` or `senv shell`.
- **Install-time code can modify the environment.** Installation must write
  packages and scripts. The install sandbox protects your source, credentials,
  and non-registry network, but it cannot make the environment itself read-only.
- **Denial suggestions are hints, not proof.** Kernel-tier reports infer some
  denials from program output, which untrusted code can influence. Review every
  suggested grant before accepting it. Container-tier network requests have a
  direct per-request tally.
- **State must remain outside the project.** senv refuses
  `SENV_STATE_DIR` or `SENV_CACHE_DIR` locations inside the project because
  runtime code could then alter environments, receipts, or trusted baselines.
- **Runtime wall-clock limits are not currently enforced.** CPU and file-size
  rlimits apply everywhere; memory and process limits use Linux cgroups and do
  not apply on macOS.
- **macOS Python startup can vary by interpreter.** Apple's system Python may
  recompile imports on each run because its bytecode cache is outside the
  managed environment. `senv init --python 3.13` selects a managed interpreter
  without that behavior.
- **Policy tampering is detected, not prevented.** A dependency can edit files
  in the writable project; senv refuses a widened policy until you trust it.

For the full threat model and design rationale, see [DESIGN.md](DESIGN.md).

---

## License

Apache-2.0. See [LICENSE](LICENSE).
