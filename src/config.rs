use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Seconds between samples.
    pub interval: f64,
    /// Samples kept for the history graphs.
    pub history: usize,
    pub hosts: Vec<Host>,
    pub activity: Vec<Rule>,
    /// Labels for server ports in the graph, e.g. `"22000" = "syncthing"`.
    pub ports: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub name: String,
    /// ssh destination, defaults to `name`.
    pub ssh: Option<String>,
    /// meshtop on the remote side, run as `<command> agent --interval <n>`.
    #[serde(default = "default_command")]
    pub command: String,
    /// Free text shown next to the name, e.g. "builder" or "client".
    pub role: Option<String>,
    /// The machine's own hostname when it differs from `name`, so the host
    /// meshtop runs on is sampled in process instead of over ssh.
    pub hostname: Option<String>,
}

/// Counts processes into one activity label. A process matches when every
/// field that is set matches. With `user` set, the count is the number of
/// distinct matching users instead, so `user = "nixbld"` counts running nix
/// builds rather than their processes.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub label: String,
    /// Exact process names.
    #[serde(default)]
    pub name: Vec<String>,
    /// Substring of the full command line.
    pub cmd: Option<String>,
    /// Prefix of the owning user name.
    pub user: Option<String>,
}

fn default_command() -> String {
    "meshtop".into()
}

impl Default for Config {
    fn default() -> Self {
        Self { interval: 1.0, history: 120, hosts: Vec::new(), activity: default_rules(), ports: BTreeMap::new() }
    }
}

fn default_rules() -> Vec<Rule> {
    let rule = |label: &str, name: &[&str], cmd: Option<&str>, user: Option<&str>| Rule {
        label: label.into(),
        name: name.iter().map(|s| s.to_string()).collect(),
        cmd: cmd.map(Into::into),
        user: user.map(Into::into),
    };
    vec![
        rule("claude", &["claude"], None, None),
        rule("codex", &["codex"], None, None),
        rule("nix build", &[], None, Some("nixbld")),
        rule("nix build", &[], None, Some("_nixbld")),
        rule("serving builds", &[], Some("nix-daemon --stdio"), None),
        rule("rustc", &["rustc"], None, None),
        rule("mosh", &["mosh-server"], None, None),
    ]
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("meshtop/config.toml")
}

pub fn load(explicit: Option<PathBuf>) -> Result<Config> {
    let path = explicit.clone().unwrap_or_else(path);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

impl Config {
    pub fn port_label(&self, port: u16) -> Option<&str> {
        self.ports.get(&port.to_string()).map(String::as_str)
    }
}

impl Host {
    pub fn destination(&self) -> &str {
        self.ssh.as_deref().unwrap_or(&self.name)
    }

    pub fn is_local(&self, local_hostname: &str) -> bool {
        let short = |s: &str| s.split('.').next().unwrap_or(s).to_ascii_lowercase();
        let local = short(local_hostname);
        short(&self.name) == local || self.hostname.as_deref().is_some_and(|h| short(h) == local)
    }
}
