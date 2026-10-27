use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use sysinfo::System;

use crate::model::Agent;

/// Process names of the coding agents meshtop recognises outside Herdr.
const AGENT_NAMES: &[&str] = &["claude", "codex", "opencode", "gemini", "amp", "droid", "cursor-agent", "kimi", "qwen"];

#[derive(Deserialize)]
struct HerdrList {
    result: HerdrResult,
}

#[derive(Deserialize)]
struct HerdrResult {
    agents: Vec<HerdrAgent>,
}

#[derive(Deserialize)]
struct HerdrAgent {
    agent: String,
    agent_status: String,
    name: Option<String>,
    cwd: Option<String>,
    terminal_title_stripped: Option<String>,
}

/// Coding agents on this machine. Agents in a Herdr session come with their
/// state and task title from `herdr agent list`; any other agent process is
/// listed by name and working directory, with "working" meaning it is using
/// CPU right now.
pub fn read(herdr: Option<&Path>, sys: &System) -> Vec<Agent> {
    // Agent processes per working directory, so Herdr's entries can borrow
    // their CPU and memory and the rest are listed on their own.
    let mut by_cwd: HashMap<PathBuf, Vec<(&str, f32, u64)>> = HashMap::new();
    for p in sys.processes().values() {
        if p.thread_kind().is_some() {
            continue;
        }
        let name = p.name().to_str().unwrap_or_default();
        let Some(kind) = AGENT_NAMES.iter().find(|n| **n == name) else { continue };
        let cwd = p.cwd().map(Path::to_path_buf).unwrap_or_default();
        by_cwd.entry(cwd).or_default().push((kind, p.cpu_usage(), p.memory()));
    }
    for procs in by_cwd.values_mut() {
        procs.sort_by(|a, b| b.1.total_cmp(&a.1));
    }

    let mut agents = Vec::new();
    for h in herdr.map(herdr_agents).unwrap_or_default() {
        let cwd = h.cwd.unwrap_or_default();
        let (cpu, mem) = by_cwd
            .get_mut(Path::new(&cwd))
            .and_then(|procs| procs.iter().position(|(k, ..)| *k == h.agent).map(|i| procs.remove(i)))
            .map(|(_, cpu, mem)| (cpu, mem))
            .unwrap_or_default();
        agents.push(Agent {
            kind: h.agent,
            status: h.agent_status,
            name: h.name,
            title: h.terminal_title_stripped.filter(|t| !t.is_empty()),
            cwd,
            cpu,
            mem,
            herdr: true,
        });
    }

    for (cwd, procs) in by_cwd {
        for (kind, cpu, mem) in procs {
            agents.push(Agent {
                kind: kind.into(),
                status: if cpu >= 2.0 { "working" } else { "idle" }.into(),
                name: None,
                title: None,
                cwd: cwd.to_string_lossy().into(),
                cpu,
                mem,
                herdr: false,
            });
        }
    }
    agents
}

fn herdr_agents(bin: &Path) -> Vec<HerdrAgent> {
    let Ok(out) = Command::new(bin).args(["agent", "list"]).output() else { return Vec::new() };
    serde_json::from_slice::<HerdrList>(&out.stdout).map(|l| l.result.agents).unwrap_or_default()
}
