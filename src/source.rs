use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::collect::Collector;
use crate::config::{Host, Rule};
use crate::model::Snapshot;

const RETRY: Duration = Duration::from_secs(3);

pub enum Update {
    Snapshot(usize, Box<Snapshot>),
    Down(usize, String),
    /// A plan usage reading for account `n`; `None` while signed out.
    Usage(usize, Option<Result<Box<crate::usage::Usage>, String>>),
}

/// Running ssh children, killed on exit so no agent outlives the TUI.
#[derive(Clone, Default)]
pub struct Children(Arc<Mutex<Vec<Option<Child>>>>);

impl Children {
    pub fn kill_all(&self) {
        for child in self.0.lock().unwrap().iter_mut().flatten() {
            let _ = child.kill();
        }
    }
}

pub fn spawn_local(index: usize, rules: Vec<Rule>, interval: Duration, tx: Sender<Update>) {
    thread::spawn(move || {
        let mut collector = Collector::new(rules);
        let mut wait = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL;
        loop {
            thread::sleep(wait);
            wait = interval;
            if tx.send(Update::Snapshot(index, Box::new(collector.sample()))).is_err() {
                return;
            }
        }
    });
}

pub fn spawn_remote(index: usize, host: Host, interval: Duration, tx: Sender<Update>, children: Children) {
    let slot = {
        let mut list = children.0.lock().unwrap();
        list.push(None);
        list.len() - 1
    };
    thread::spawn(move || {
        loop {
            let reason = match run_remote(index, &host, interval, &tx, &children, slot) {
                Ok(reason) => reason,
                Err(e) => e.to_string(),
            };
            if tx.send(Update::Down(index, reason)).is_err() {
                return;
            }
            thread::sleep(RETRY);
        }
    });
}

fn run_remote(
    index: usize,
    host: &Host,
    interval: Duration,
    tx: &Sender<Update>,
    children: &Children,
    slot: usize,
) -> anyhow::Result<String> {
    let mut child = Command::new("ssh")
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            "-o",
            "LogLevel=ERROR",
            host.destination(),
        ])
        .arg(format!("{} agent --interval {}", host.command, interval.as_secs_f64()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    children.0.lock().unwrap()[slot] = Some(child);

    let err = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    for line in BufReader::new(stdout).lines() {
        let line = line?;
        match serde_json::from_str::<Snapshot>(&line) {
            Ok(snap) => {
                if tx.send(Update::Snapshot(index, Box::new(snap))).is_err() {
                    return Ok(String::new());
                }
            }
            Err(e) => return Ok(format!("bad agent output: {e}")),
        }
    }

    if let Some(mut child) = children.0.lock().unwrap()[slot].take() {
        let _ = child.wait();
    }
    let stderr = err.join().unwrap_or_default();
    Ok(stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("connection closed").trim().to_string())
}
