//! Where meshtop keeps the registry (editable, in git) and its own state
//! (observed, per host, never in git).

use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// `~/x` to `$HOME/x`; anything else unchanged.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

/// The agent-config checkout: `$MESHTOP_REGISTRY`, else
/// `~/Developer/ecosystem/agent-config`.
pub fn registry() -> PathBuf {
    std::env::var_os("MESHTOP_REGISTRY")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("Developer/ecosystem/agent-config"))
}

/// `$MESHTOP_STATE`, else `$XDG_STATE_HOME/meshtop`, else `~/.local/state/meshtop`.
pub fn state() -> PathBuf {
    if let Some(dir) = std::env::var_os("MESHTOP_STATE") {
        return PathBuf::from(dir);
    }
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("meshtop")
}

/// Writes through a temp file and rename, so a reader never sees half a file
/// and a crash leaves either the old or the new content.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `MESHTOP_STATE` is process-global, so tests that set it take turns.
#[cfg(test)]
pub static TEST_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
