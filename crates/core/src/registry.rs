//! `registry.toml` at the root of the agent-config checkout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Manifest {
    pub targets: Vec<Target>,
    pub external: Vec<External>,
    pub memory: MemoryViews,
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Target {
    pub name: String,
    pub dir: String,
    #[serde(default)]
    pub mode: InstallMode,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InstallMode {
    /// A symlink to the registry copy: edits are live without a sync.
    #[default]
    Link,
    /// A real copy, for tools that do not follow symlinked skill dirs.
    Copy,
}

/// A skill root meshtop reads for the duplicate report but never writes.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct External {
    pub name: String,
    pub glob: String,
    /// Targets whose tool also loads these; empty means every target.
    #[serde(default)]
    pub targets: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryViews {
    pub claude_view: Option<String>,
    pub codex_view: Option<String>,
    pub max_index_lines: usize,
}

impl Default for MemoryViews {
    fn default() -> Self {
        Self { claude_view: None, codex_view: None, max_index_lines: 80 }
    }
}

/// A service on the host running meshtop that the CLI and web UI may inspect,
/// and restart when `restart` is set.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Service {
    pub name: String,
    /// launchd label or systemd user unit.
    pub unit: String,
    #[serde(default)]
    pub logs: Vec<String>,
    #[serde(default)]
    pub restart: bool,
    /// Only shown on this host (registry host name). Unset: everywhere.
    pub host: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Registry {
    pub root: PathBuf,
    pub manifest: Manifest,
}

impl Registry {
    pub fn open(root: Option<PathBuf>) -> Result<Self> {
        let root = root.unwrap_or_else(paths::registry);
        let file = root.join("registry.toml");
        let manifest = match std::fs::read_to_string(&file) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!("no registry at {} (set MESHTOP_REGISTRY or clone agent-config there)", root.display())
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
        };
        Ok(Self { root, manifest })
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.root.join("archive")
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    pub fn hosts_file(&self) -> PathBuf {
        self.root.join("hosts.toml")
    }

    /// `git rev-parse --short HEAD` plus `-dirty` when the tree has edits.
    pub fn revision(&self) -> Option<String> {
        git_revision(&self.root)
    }
}

pub fn git_revision(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--short", "HEAD"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let mut rev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dirty = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        rev.push_str("-dirty");
    }
    Some(rev)
}
