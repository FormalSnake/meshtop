//! Status, logs and (opt-in) restarts for the registry's `[[services]]` on
//! the host running meshtop: launchd agents on macOS, systemd user units on
//! Linux. Anything not listed in the registry is out of reach on purpose.

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::paths;
use crate::registry::{Registry, Service};

#[derive(Clone, Debug, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub unit: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub last_exit: Option<String>,
    pub restart: bool,
    pub detail: Option<String>,
}

pub fn here(reg: &Registry, host: &str) -> Vec<Service> {
    reg.manifest.services.iter().filter(|s| s.host.as_deref().is_none_or(|h| h == host)).cloned().collect()
}

fn find(reg: &Registry, host: &str, name: &str) -> Result<Service> {
    here(reg, host).into_iter().find(|s| s.name == name).with_context(|| format!("no service {name} in the registry for {host}"))
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn status(s: &Service) -> ServiceStatus {
    let mut st = ServiceStatus {
        name: s.name.clone(),
        unit: s.unit.clone(),
        running: false,
        pid: None,
        last_exit: None,
        restart: s.restart,
        detail: None,
    };
    if cfg!(target_os = "macos") {
        let out = Command::new("launchctl").arg("print").arg(format!("gui/{}/{}", uid(), s.unit)).output();
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                // Top-level keys are indented by exactly one tab; nested
                // blocks (endpoints, sockets) repeat `state =` deeper in.
                for line in text.lines().filter(|l| l.starts_with('\t') && !l.starts_with("\t\t")) {
                    let line = line.trim();
                    if let Some(v) = line.strip_prefix("state = ") {
                        st.running = v == "running";
                    } else if let Some(v) = line.strip_prefix("pid = ") {
                        st.pid = v.parse().ok();
                    } else if let Some(v) = line.strip_prefix("last exit code = ") {
                        st.last_exit = Some(v.to_string());
                    }
                }
            }
            Ok(o) => st.detail = Some(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => st.detail = Some(e.to_string()),
        }
    } else {
        let out = Command::new("systemctl")
            .args(["--user", "show", "-p", "ActiveState,MainPID,ExecMainStatus,SubState", &s.unit])
            .output();
        match out {
            Ok(o) => {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    match line.split_once('=') {
                        Some(("ActiveState", v)) => st.running = v == "active",
                        Some(("MainPID", v)) => st.pid = v.parse().ok().filter(|p| *p != 0),
                        Some(("ExecMainStatus", v)) => st.last_exit = Some(v.to_string()),
                        Some(("SubState", v)) => st.detail = Some(v.to_string()),
                        _ => {}
                    }
                }
            }
            Err(e) => st.detail = Some(e.to_string()),
        }
    }
    st
}

/// The last `lines` lines of each configured log, or the journal on Linux
/// when no log file is configured.
pub fn logs(reg: &Registry, host: &str, name: &str, lines: usize) -> Result<String> {
    let s = find(reg, host, name)?;
    let lines = lines.clamp(1, 2000);
    if s.logs.is_empty() {
        if cfg!(target_os = "macos") {
            bail!("{name} has no log file configured");
        }
        let o = Command::new("journalctl").args(["--user", "-u", &s.unit, "--no-pager", "-n", &lines.to_string()]).output()?;
        return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
    }
    let mut out = String::new();
    for log in &s.logs {
        let path = paths::expand(log);
        out.push_str(&format!("==> {} <==\n", path.display()));
        let o = Command::new("tail").arg("-n").arg(lines.to_string()).arg(&path).output()?;
        out.push_str(&String::from_utf8_lossy(&o.stdout));
        out.push_str(&String::from_utf8_lossy(&o.stderr));
    }
    Ok(out)
}

pub fn restart(reg: &Registry, host: &str, name: &str) -> Result<ServiceStatus> {
    let s = find(reg, host, name)?;
    if !s.restart {
        bail!("{name} is not marked restart = true in registry.toml");
    }
    let o = if cfg!(target_os = "macos") {
        Command::new("launchctl").args(["kickstart", "-k"]).arg(format!("gui/{}/{}", uid(), s.unit)).output()?
    } else {
        Command::new("systemctl").args(["--user", "restart", &s.unit]).output()?
    };
    if !o.status.success() {
        bail!("restart failed: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    Ok(status(&s))
}
