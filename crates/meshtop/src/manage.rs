//! The management subcommands: hosts, skills, memory, jobs, services. Each
//! prints text by default and JSON with `--json`, from the same core calls
//! the web UI makes.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use meshtop_core::registry::Registry;
use meshtop_core::{config, hosts, jobs, memory, paths, services, skills, worktrees};

pub(crate) fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// How long ago a millisecond timestamp was.
pub(crate) fn ago(at_ms: u64) -> String {
    span(paths::now_ms().saturating_sub(at_ms))
}

pub(crate) fn span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

// ---- hosts ---------------------------------------------------------------

#[derive(Subcommand)]
pub enum HostsCmd {
    /// Which OS each host is running now, as which machine, since which
    /// boot, and what it can take. Probes only observations older than
    /// `--max-age`; the cache is shared by every caller.
    Status {
        hosts: Vec<String>,
        /// Reuse an observation this many seconds old.
        #[arg(long, default_value_t = 30)]
        max_age: u64,
        /// Never probe; report the cache.
        #[arg(long)]
        cached: bool,
        /// Per-probe deadline in seconds.
        #[arg(long, default_value_t = 8)]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// Block until HOST is ready, bounded by `--timeout`. Exits 0 ready,
    /// 1 blocked (rejected key, wrong machine), 75 deadline, 130 cancelled.
    /// Never powers a host on.
    Wait {
        host: String,
        /// Required OS: linux, macos or windows.
        #[arg(long)]
        os: Option<String>,
        /// Required role in the running OS, e.g. `builder`.
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        heavy: bool,
        /// A command that must exit 0 on the host, e.g. `systemctl is-active foo`.
        #[arg(long)]
        check: Option<String>,
        /// Seconds to wait in total.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Seconds between probes; tailscale coming back online probes sooner.
        #[arg(long, default_value_t = 30)]
        interval: u64,
        #[arg(long)]
        json: bool,
    },
    /// The first host that can take a role right now, e.g. `builder`.
    Pick {
        role: String,
        /// A heavy job (full build): skips hosts restricted from them.
        #[arg(long)]
        heavy: bool,
        #[arg(long, default_value_t = 30)]
        max_age: u64,
        #[arg(long, default_value_t = 8)]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// Keep a host out of `pick` for a while (gaming, a demo, a reinstall).
    Hold {
        host: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        hours: Option<f64>,
    },
    Release { host: String },
}

pub const HOSTS_SCHEMA: &str = "meshtop.hosts/1";

fn print_host(s: &hosts::HostStatus) {
    let age = s.age_ms.map(|a| format!(" ({} ago)", span(a))).unwrap_or_default();
    let now = match (&s.os, &s.via) {
        (Some(os), Some(via)) => format!("ready {os} via {via}"),
        _ => s.reach.as_str().to_string(),
    };
    println!("{:<8} {now}{age}", s.name);
    if s.reach != hosts::Reach::Ready {
        println!("         {}", s.detail);
    }
    if let Some(l) = s.last_known.as_ref().filter(|_| s.os.is_none()) {
        println!("         last ready {} {} ago", l.os, ago(l.at));
    }
    if !s.eligible.is_empty() {
        println!("         can take {}", s.eligible.join(", "));
    }
    if !s.restrictions.is_empty() {
        println!("         restricted {}", s.restrictions.join(", "));
    }
    if let Some(h) = &s.hold {
        println!("         held: {}", h.reason);
    }
}

pub fn hosts_cmd(cmd: HostsCmd) -> Result<i32> {
    let reg = Registry::open(None)?;
    let file = hosts::load(&reg)?;
    let known = |h: &str| -> Result<()> {
        if file.hosts.contains_key(h) {
            Ok(())
        } else {
            bail!("{h} is not in {}", reg.hosts_file().display())
        }
    };
    match cmd {
        HostsCmd::Status { hosts: only, max_age, cached, timeout, json } => {
            for h in &only {
                known(h)?;
            }
            let st = hosts::status_all(&file, &only, Duration::from_secs(max_age), Duration::from_secs(timeout), cached);
            if json {
                print_json(&serde_json::json!({ "schema": HOSTS_SCHEMA, "at": paths::now_ms(), "hosts": st }))?;
            } else {
                st.iter().for_each(print_host);
            }
            Ok(0)
        }
        HostsCmd::Wait { host, os, role, heavy, check, timeout, interval, json } => {
            known(&host)?;
            let def = &file.hosts[&host];
            let probe_limit = Duration::from_secs(8);
            let want = hosts::Want { os, role, heavy, check };
            let observe = |max_age: Duration| {
                let c = hosts::observe(&host, def, max_age, probe_limit).unwrap_or_else(|_| hosts::cached(&host));
                hosts::status_from(&host, def, &c, max_age.max(Duration::from_secs(interval)), &hosts::holds())
            };
            let last = std::cell::RefCell::new(None::<hosts::HostStatus>);
            let observe_and_keep = |max_age| {
                let s = observe(max_age);
                *last.borrow_mut() = Some(s.clone());
                if !json {
                    let why = hosts::meets(&s, &want).err().map(|(_, w)| w).unwrap_or_else(|| "ready".into());
                    eprintln!("{host}: {why}");
                }
                s
            };
            let wake = || last.borrow().as_ref().is_some_and(hosts::tailscale_wake);
            let check = |s: &hosts::HostStatus| match &want.check {
                Some(c) => hosts::run_check(s, c, Duration::from_secs(30)),
                None => Ok(()),
            };
            let w = hosts::Waiter { observe: &observe_and_keep, wake: &wake, check: &check, cancel: hosts::cancel_on_signal() };
            let r = hosts::wait(&w, &want, Duration::from_secs(timeout), Duration::from_secs(interval.max(1)));
            if json {
                print_json(&serde_json::json!({ "schema": HOSTS_SCHEMA, "host": host, "want": want, "result": r }))?;
            } else {
                match &r.end {
                    hosts::WaitEnd::Ready => print_host(&r.status),
                    hosts::WaitEnd::Blocked(why) => eprintln!("{host} blocked, waiting will not help: {why}"),
                    hosts::WaitEnd::Deadline(why) => eprintln!("{host} not ready after {}: {why}", span(r.waited_ms)),
                    hosts::WaitEnd::Cancelled(why) => eprintln!("cancelled after {}: {why}", span(r.waited_ms)),
                }
            }
            Ok(r.exit_code())
        }
        HostsCmd::Pick { role, heavy, max_age, timeout, json } => {
            let st = hosts::status_all(&file, &[], Duration::from_secs(max_age), Duration::from_secs(timeout), false);
            match hosts::pick(&st, &role, heavy) {
                Ok(s) => {
                    if json {
                        print_json(&serde_json::json!({ "schema": HOSTS_SCHEMA, "host": s }))?;
                    } else {
                        println!("{}", s.name);
                    }
                    Ok(0)
                }
                Err(why) => {
                    eprintln!("no host can take {role} right now:");
                    for (h, r) in why {
                        eprintln!("  {h}: {r}");
                    }
                    Ok(1)
                }
            }
        }
        HostsCmd::Hold { host, reason, hours } => {
            known(&host)?;
            hosts::hold(&host, &reason, hours)?;
            println!("held {host}: {reason}");
            Ok(0)
        }
        HostsCmd::Release { host } => {
            println!("{}", if hosts::release(&host)? { format!("released {host}") } else { format!("{host} had no hold") });
            Ok(0)
        }
    }
}

// ---- skills --------------------------------------------------------------

#[derive(Subcommand)]
pub enum SkillsCmd {
    /// Registry skills with their descriptions.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Every entry in each discovery dir and whether meshtop owns it.
    Status {
        #[arg(long)]
        target: Option<String>,
        /// Include foreign and current entries.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install the registry into the discovery dirs.
    Sync {
        #[arg(long)]
        target: Option<String>,
        /// Replace foreign entries named like a registry skill (moved to a backup).
        #[arg(long)]
        adopt: bool,
        /// Overwrite copies edited in place (moved to a backup).
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Pull the registry (fast-forward only), sync every target and rewrite the
    /// memory views. What a timer runs: it never adopts or overwrites edits.
    Update,
    /// Find backups and plugin duplicates in discovery; `--fix` moves them out.
    Doctor {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// `diff -ru` between the registry and a target's copy.
    Diff {
        name: String,
        #[arg(long, default_value = "claude")]
        target: String,
    },
    /// Undo a sync or doctor run (default: the latest).
    Rollback { generation: Option<String> },
    /// Past sync and doctor runs, newest first.
    Generations,
    /// Open the registry SKILL.md in $EDITOR.
    Edit { name: String },
}

fn print_generation(g: &skills::Generation, dry_run: bool) {
    if g.changes.is_empty() {
        println!("nothing to do");
    }
    let would = if dry_run { "would be " } else { "" };
    for c in &g.changes {
        let b = c.backup.as_ref().map(|b| format!(" (backup {})", b.display())).unwrap_or_default();
        let action = if c.action.starts_with("skipped") { c.action.clone() } else { format!("{would}{}", c.action) };
        println!("{:<8} {:<32} {action}{b}", c.target, c.name);
    }
    if dry_run && !g.changes.is_empty() {
        println!("dry run: nothing was written");
    }
    if g.changes.iter().any(|c| c.backup.is_some()) {
        println!("generation {} (undo: meshtop skills rollback {})", g.id, g.id);
    }
}

pub fn skills_cmd(cmd: SkillsCmd) -> Result<i32> {
    let reg = Registry::open(None)?;
    match cmd {
        SkillsCmd::List { json } => {
            let cat = skills::catalog(&reg)?;
            if json {
                return print_json(&cat).map(|_| 0);
            }
            for s in cat {
                let d: String = s.description.chars().take(90).collect();
                println!("{:<30} {d}", s.name);
            }
        }
        SkillsCmd::Status { target, all, json } => {
            let mut out = Vec::new();
            for t in reg.manifest.targets.iter().filter(|t| target.as_ref().is_none_or(|n| *n == t.name)) {
                out.push(skills::target_status(&reg, t)?);
            }
            if json {
                return print_json(&out).map(|_| 0);
            }
            let mut issues = 0;
            for t in out {
                println!("{} ({}, {:?})", t.target, t.dir.display(), t.mode);
                for e in t.entries {
                    let quiet = matches!(e.state, skills::EntryState::Current | skills::EntryState::Foreign | skills::EntryState::Excluded);
                    if quiet && !all {
                        continue;
                    }
                    issues += usize::from(!quiet);
                    println!("  {:<32} {:?}{}", e.name, e.state, e.detail.map(|d| format!(": {d}")).unwrap_or_default());
                }
            }
            return Ok(i32::from(issues > 0));
        }
        SkillsCmd::Sync { target, adopt, force, dry_run } => {
            let g = skills::sync(&reg, target.as_deref(), &skills::SyncOptions { adopt, force, dry_run })?;
            print_generation(&g, dry_run);
        }
        SkillsCmd::Update => {
            let out = Command::new("git").arg("-C").arg(&reg.root).args(["pull", "--ff-only", "-q"]).output()?;
            if !out.status.success() {
                bail!("git pull in {} failed: {}", reg.root.display(), String::from_utf8_lossy(&out.stderr).trim());
            }
            let reg = Registry::open(None)?;
            let g = skills::sync(&reg, None, &skills::SyncOptions { adopt: false, force: false, dry_run: false })?;
            print_generation(&g, false);
            export_views(&reg)?;
        }
        SkillsCmd::Doctor { fix, dry_run } => {
            if fix {
                print_generation(&skills::doctor_fix(&reg, dry_run)?, dry_run);
                return Ok(0);
            }
            let mut found = 0;
            for t in &reg.manifest.targets {
                for e in skills::target_status(&reg, t)?.entries {
                    if matches!(e.state, skills::EntryState::Backup | skills::EntryState::Duplicate | skills::EntryState::Conflict | skills::EntryState::Edited) {
                        found += 1;
                        println!("{:<8} {:<32} {:?}{}", t.name, e.name, e.state, e.detail.map(|d| format!(": {d}")).unwrap_or_default());
                    }
                }
            }
            // Registry skills a loaded plugin also ships: two copies in the
            // skill list, worth a decision but not an automatic move.
            for (name, ext) in skills::external(&reg, Some("claude")) {
                if reg.skills_dir().join(&name).is_dir() {
                    println!("registry {name:<32} also shipped by {}", ext[0].0);
                }
            }
            if found == 0 {
                println!("no backups, duplicates or conflicts in discovery");
            }
            return Ok(i32::from(found > 0));
        }
        SkillsCmd::Diff { name, target } => print!("{}", skills::diff(&reg, &name, &target)?),
        SkillsCmd::Rollback { generation } => print_generation(&skills::rollback(&reg, generation.as_deref())?, false),
        SkillsCmd::Generations => {
            for g in skills::generations() {
                println!("{}  {} changes  registry {}", g.id, g.changes.len(), g.revision.unwrap_or_else(|| "?".into()));
            }
        }
        SkillsCmd::Edit { name } => {
            let file = reg.skills_dir().join(&name).join("SKILL.md");
            if !file.is_file() {
                bail!("no registry skill {name}");
            }
            let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
            Command::new(editor).arg(&file).status()?;
            println!("edited {}; link targets see it now, run `meshtop skills sync` for copy targets", file.display());
        }
    }
    Ok(0)
}

// ---- memory --------------------------------------------------------------

#[derive(Subcommand)]
pub enum MemoryCmd {
    List {
        #[arg(long)]
        scope: Option<String>,
        /// Include retired and expired entries.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Show { id: String },
    /// Active entries containing every term.
    Search {
        #[arg(required = true)]
        terms: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Record a confirmed, reusable fact. Not for guesses or task state.
    Add(AddArgs),
    /// Mark an entry as checked today.
    Verify { id: String },
    /// Retire an entry that is no longer true.
    Forget {
        id: String,
        #[arg(long)]
        reason: String,
    },
    /// Rewrite the generated views that Claude and Codex load.
    Export,
    /// Expired entries and ones nobody verified for `--days`.
    Stale {
        #[arg(long, default_value_t = 90)]
        days: i64,
    },
}

#[derive(Args)]
pub struct AddArgs {
    /// kebab-case, e.g. g815-is-the-builder.
    #[arg(long)]
    id: String,
    /// global, host:<name>, repo:<name> or tool:<claude|codex>.
    #[arg(long)]
    scope: String,
    /// fact, preference, procedure or reference.
    #[arg(long, default_value = "fact")]
    kind: String,
    /// Who said it or what shows it: "user, 2026-10-27", a commit, a thread.
    #[arg(long)]
    source: String,
    /// YYYY-MM-DD after which it is dropped from views.
    #[arg(long)]
    expires: Option<String>,
    /// The change that would make it wrong.
    #[arg(long)]
    invalidate_when: Option<String>,
    /// Supersede an existing id.
    #[arg(long)]
    replace: bool,
    /// Skip the git commit in the registry.
    #[arg(long)]
    no_commit: bool,
    #[arg(required = true)]
    text: Vec<String>,
}

pub(crate) fn local_host(reg: &Registry) -> String {
    let file = hosts::load(reg).unwrap_or_default();
    file.hosts
        .iter()
        .find(|(n, d)| hosts::is_local(n, d))
        .map(|(n, _)| n.clone())
        .unwrap_or_else(|| sysinfo::System::host_name().unwrap_or_default().split('.').next().unwrap_or("").to_ascii_lowercase())
}

/// Commits one registry path, leaving everything else in that tree alone.
pub(crate) fn commit_path(reg: &Registry, path: &std::path::Path, msg: &str) -> Result<()> {
    if !reg.root.join(".git").exists() {
        return Ok(());
    }
    let rel = path.strip_prefix(&reg.root).unwrap_or(path);
    let ok = |c: &mut Command| c.status().map(|s| s.success()).unwrap_or(false);
    if !ok(Command::new("git").arg("-C").arg(&reg.root).args(["add", "--"]).arg(rel)) {
        bail!("git add {} failed", rel.display());
    }
    let out = Command::new("git").arg("-C").arg(&reg.root).args(["commit", "-q", "-m", msg, "--"]).arg(rel).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        if !err.contains("nothing to commit") && !err.contains("no changes added") {
            bail!("git commit failed: {}", err.trim());
        }
    }
    Ok(())
}

pub(crate) fn export_views(reg: &Registry) -> Result<()> {
    for e in memory::export(reg, &local_host(reg))? {
        if e.changed {
            println!("wrote {} ({} entries)", e.path.display(), e.entries);
        }
    }
    Ok(())
}

pub fn memory_cmd(cmd: MemoryCmd) -> Result<i32> {
    let reg = Registry::open(None)?;
    let today = memory::today();
    match cmd {
        MemoryCmd::List { scope, all, json } => {
            let mems: Vec<_> = memory::all(&reg)
                .into_iter()
                .filter(|m| all || m.active(&today))
                .filter(|m| scope.as_ref().is_none_or(|s| m.meta.scope == *s))
                .collect();
            if json {
                return print_json(&mems).map(|_| 0);
            }
            for m in mems {
                let flag = if m.active(&today) { "" } else { " [inactive]" };
                println!("{:<36} {:<18} {}{flag}", m.meta.id, m.meta.scope, m.summary());
            }
        }
        MemoryCmd::Show { id } => {
            let m = memory::get(&reg, &id)?;
            print!("{}", std::fs::read_to_string(&m.path)?);
        }
        MemoryCmd::Search { terms, json } => {
            let mems = memory::all(&reg);
            let hits = memory::search(&mems, &terms, &today);
            if json {
                return print_json(&hits).map(|_| 0);
            }
            for m in &hits {
                println!("{} [{}]\n  {}", m.meta.id, m.meta.scope, m.body.replace('\n', "\n  "));
            }
            return Ok(i32::from(hits.is_empty()));
        }
        MemoryCmd::Add(a) => {
            let m = memory::add(
                &reg,
                memory::New {
                    id: a.id,
                    scope: a.scope,
                    kind: a.kind,
                    source: a.source,
                    expires: a.expires,
                    invalidate_when: a.invalidate_when,
                    body: a.text.join(" "),
                },
                a.replace,
            )?;
            if !a.no_commit {
                commit_path(&reg, &m.path, &format!("memory: add {}", m.meta.id))?;
            }
            println!("saved {}", m.meta.id);
            export_views(&reg)?;
        }
        MemoryCmd::Verify { id } => {
            let m = memory::verify(&reg, &id)?;
            commit_path(&reg, &m.path, &format!("memory: verify {id}"))?;
            println!("verified {id}");
        }
        MemoryCmd::Forget { id, reason } => {
            let m = memory::forget(&reg, &id, &reason)?;
            commit_path(&reg, &m.path, &format!("memory: retire {id}"))?;
            println!("retired {id}");
            export_views(&reg)?;
        }
        MemoryCmd::Export => export_views(&reg)?,
        MemoryCmd::Stale { days } => {
            let mems = memory::all(&reg);
            let st = memory::stale(&mems, &today, days);
            for (m, why) in &st {
                println!("{:<36} {why}", m.meta.id);
            }
            return Ok(i32::from(!st.is_empty()));
        }
    }
    Ok(0)
}

// ---- jobs ----------------------------------------------------------------

#[derive(Subcommand)]
pub enum JobCmd {
    /// Start a detached, recoverable job. Starting an existing id reports it
    /// instead of running it again, so a retry after a lost reply is safe.
    Start {
        /// Stable id; pass one whenever the caller may retry.
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        owner: Option<String>,
        /// Resource this job writes (a checkout, a store path). Repeatable.
        #[arg(long = "lock")]
        locks: Vec<String>,
        /// URL to POST the result to when the job ends.
        #[arg(long)]
        notify: Option<String>,
        /// Run on another host over ssh.
        #[arg(long)]
        host: Option<String>,
        /// Read the job spec as JSON from stdin (used for remote starts).
        #[arg(long, hide = true)]
        spec: bool,
        #[arg(long)]
        json: bool,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    Status {
        id: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Wait for the end, up to `--timeout` seconds. Exit 0 succeeded,
    /// 1 failed/blocked/cancelled, 74 lost (outcome unknown), 75 still
    /// running at the deadline, 76 the host could not be asked, 77 needs
    /// authorization: refused a password, key or permission, or silent
    /// behind a macOS dialog (that job keeps running until cancelled).
    Wait {
        id: String,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Logs {
        id: String,
        #[arg(short = 'n', long, default_value_t = 60)]
        lines: usize,
        #[arg(long)]
        host: Option<String>,
    },
    List {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Cancel {
        id: String,
        #[arg(long)]
        host: Option<String>,
    },
    /// Mark a job's completion notice as handled.
    Ack {
        id: String,
        #[arg(long)]
        host: Option<String>,
    },
    /// Retry undelivered notifications.
    Reconcile,
    #[command(name = "__supervise", hide = true)]
    Supervise { id: String },
}

/// ssh destination and remote meshtop for a registry or config host.
fn remote(host: &str) -> (String, String) {
    let command = config::load(None)
        .ok()
        .and_then(|c| c.hosts.into_iter().find(|h| h.name == host).map(|h| h.command))
        .unwrap_or_else(|| "meshtop".into());
    let dest = Registry::open(None)
        .ok()
        .and_then(|r| hosts::load(&r).ok())
        .and_then(|f| f.hosts.get(host).cloned())
        .and_then(|d| d.modes.into_iter().find(|m| m.os != "windows").map(|m| m.ssh))
        .unwrap_or_else(|| host.to_string());
    (dest, command)
}

pub(crate) struct RemoteOut {
    /// `None`: the connection failed or stalled, so nothing is known.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `meshtop <args>` on `host`, bounded by `limit`, capturing output.
pub(crate) fn remote_call(host: &str, args: &[String], stdin: Option<String>, limit: Duration) -> Result<RemoteOut> {
    let (dest, command) = remote(host);
    for a in args {
        if !a.chars().all(|c| c.is_ascii_alphanumeric() || "-_.=:/".contains(c)) {
            bail!("argument {a:?} is not safe to pass over ssh");
        }
    }
    let mut child = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3", "-o", "LogLevel=ERROR"])
        .arg(&dest)
        .arg(format!("{command} {}", args.join(" ")))
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
    }
    let read = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = r.read_to_string(&mut s);
            s
        })
    };
    let out = read(Box::new(child.stdout.take().unwrap()));
    let err = read(Box::new(child.stderr.take().unwrap()));
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break Some(st);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let (stdout, mut stderr) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    let code = status.and_then(|s| s.code()).filter(|c| *c != 255);
    if status.is_none() {
        stderr = "no reply in time".into();
    }
    Ok(RemoteOut { code, stdout, stderr })
}

/// `remote_call` for the CLI: output passes through, and a connection that
/// fails or stalls is EXIT_TRANSPORT, because then nothing is known.
fn run_remote(host: &str, args: &[String], stdin: Option<String>, limit: Duration) -> Result<i32> {
    let r = remote_call(host, args, stdin, limit)?;
    print!("{}", r.stdout);
    match r.code {
        None if hosts::classify_ssh_error(&r.stderr) == hosts::Reach::AuthFailed => {
            eprintln!("{host} refused the ssh login ({}); nothing ran there.", r.stderr.trim());
            Ok(jobs::EXIT_AUTH)
        }
        None => {
            eprintln!("could not reach {host} ({}); the job's state is unknown. Ask again with the same command.", r.stderr.trim());
            Ok(jobs::EXIT_TRANSPORT)
        }
        Some(code) => {
            eprint!("{}", r.stderr);
            Ok(code)
        }
    }
}

fn state_code(s: &jobs::Status) -> i32 {
    match s.state {
        _ if s.auth.is_some() => jobs::EXIT_AUTH,
        jobs::State::Succeeded => 0,
        jobs::State::Lost => jobs::EXIT_LOST,
        jobs::State::Starting | jobs::State::Running => jobs::EXIT_STILL_RUNNING,
        _ => 1,
    }
}

fn print_status(s: &jobs::Status) {
    let code = s.outcome.as_ref().and_then(|o| o.code).map(|c| format!(" exit {c}")).unwrap_or_default();
    println!("{} {:?}{code} on {} (owner {}, {} ago)", s.id, s.state, s.record.host, s.record.spec.owner, ago(s.record.created));
    println!("  {}", s.record.spec.cmd.join(" "));
    if let Some(q) = s.quiet_ms
        && q > 60_000 {
            println!("  no output for {}; still running while its lock is held", span(q));
        }
    if let Some(d) = &s.detail {
        println!("  {d}");
    }
    if let Some(d) = &s.delivery {
        let st = if d.acked.is_some() { "acked" } else if d.delivered.is_some() { "delivered" } else { "undelivered" };
        println!("  notify {st} after {} attempts{}", d.attempts, d.last_error.as_ref().map(|e| format!(": {e}")).unwrap_or_default());
    }
}

pub fn job_cmd(cmd: JobCmd) -> Result<i32> {
    let flag = |b: bool, f: &str| if b { vec![f.to_string()] } else { vec![] };
    match cmd {
        JobCmd::Supervise { id } => {
            jobs::supervise(&id)?;
            Ok(0)
        }
        JobCmd::Start { id, cwd, owner, locks, notify, host, spec, json, cmd } => {
            let spec = if spec {
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                serde_json::from_str(&text).context("parsing job spec")?
            } else {
                jobs::Spec { id: id.unwrap_or_default(), cmd, cwd, owner: owner.unwrap_or_default(), locks, notify, ..Default::default() }
            };
            if let Some(h) = host {
                let mut spec = spec;
                if spec.id.is_empty() {
                    // Named here, so a retry after a lost reply hits the same job.
                    spec.id = jobs::new_id(&spec.cmd);
                    eprintln!("job id {}", spec.id);
                }
                let mut args = vec!["job".to_string(), "start".into(), "--spec".into()];
                args.extend(flag(json, "--json"));
                return run_remote(&h, &args, Some(serde_json::to_string(&spec)?), Duration::from_secs(30));
            }
            let s = jobs::start(spec, &std::env::current_exe()?)?;
            if json { print_json(&s)? } else { print_status(&s) }
            Ok(if s.finished() { state_code(&s) } else { 0 })
        }
        JobCmd::Status { id, host, json } => {
            if let Some(h) = host {
                let mut a = vec!["job".into(), "status".into(), id];
                a.extend(flag(json, "--json"));
                return run_remote(&h, &a, None, Duration::from_secs(20));
            }
            let s = jobs::status(&id)?;
            if json { print_json(&s)? } else { print_status(&s) }
            Ok(state_code(&s))
        }
        JobCmd::Wait { id, timeout, host, json } => {
            if let Some(h) = host {
                let mut a = vec!["job".into(), "wait".into(), id, "--timeout".into(), timeout.to_string()];
                a.extend(flag(json, "--json"));
                return run_remote(&h, &a, None, Duration::from_secs(timeout + 30));
            }
            let s = jobs::wait(&id, Duration::from_secs(timeout))?;
            if json { print_json(&s)? } else { print_status(&s) }
            Ok(state_code(&s))
        }
        JobCmd::Logs { id, lines, host } => {
            if let Some(h) = host {
                return run_remote(&h, &["job".into(), "logs".into(), id, "-n".into(), lines.to_string()], None, Duration::from_secs(20));
            }
            println!("{}", jobs::tail(&id, lines)?);
            Ok(0)
        }
        JobCmd::List { host, json } => {
            if let Some(h) = host {
                let mut a = vec!["job".to_string(), "list".into()];
                a.extend(flag(json, "--json"));
                return run_remote(&h, &a, None, Duration::from_secs(20));
            }
            let all = jobs::list();
            if json {
                print_json(&all)?;
            } else {
                for s in all.iter().take(30) {
                    println!("{:<40} {:<10} {:>4}  {}", s.id, format!("{:?}", s.state).to_lowercase(), ago(s.record.created), s.record.spec.cmd.join(" "));
                }
            }
            Ok(0)
        }
        JobCmd::Cancel { id, host } => {
            if let Some(h) = host {
                return run_remote(&h, &["job".into(), "cancel".into(), id], None, Duration::from_secs(20));
            }
            let s = jobs::cancel(&id)?;
            print_status(&s);
            Ok(0)
        }
        JobCmd::Ack { id, host } => {
            if let Some(h) = host {
                return run_remote(&h, &["job".into(), "ack".into(), id], None, Duration::from_secs(20));
            }
            jobs::ack(&id)?;
            println!("acked {id}");
            Ok(0)
        }
        JobCmd::Reconcile => {
            for id in jobs::reconcile() {
                println!("retried notify for {id}");
            }
            Ok(0)
        }
    }
}

// ---- services ------------------------------------------------------------

#[derive(Subcommand)]
pub enum ServicesCmd {
    Status {
        #[arg(long)]
        json: bool,
    },
    Logs {
        name: String,
        #[arg(short = 'n', long, default_value_t = 80)]
        lines: usize,
    },
    /// Only for services marked `restart = true` in registry.toml.
    Restart { name: String },
}

pub fn services_cmd(cmd: ServicesCmd) -> Result<i32> {
    let reg = Registry::open(None)?;
    let host = local_host(&reg);
    match cmd {
        ServicesCmd::Status { json } => {
            let st: Vec<_> = services::here(&reg, &host).iter().map(services::status).collect();
            if json {
                return print_json(&st).map(|_| 0);
            }
            let w = st.iter().map(|s| s.name.len()).max().unwrap_or(0);
            for s in &st {
                let pid = s.pid.map(|p| format!(" pid {p}")).unwrap_or_default();
                let state = if s.running { "running" } else { "stopped" };
                let at = s.at.as_deref().map(|a| format!(" on {a}")).unwrap_or_default();
                println!("{:<w$} {state}{pid}{at}  last exit {}", s.name, s.last_exit.as_deref().unwrap_or("-"));
                if let (false, Some(d)) = (s.running, &s.detail) {
                    println!("{:<w$} {d}", "");
                }
            }
            Ok(i32::from(st.iter().any(|s| !s.running)))
        }
        ServicesCmd::Logs { name, lines } => {
            print!("{}", services::logs(&reg, &host, &name, lines)?);
            Ok(0)
        }
        ServicesCmd::Restart { name } => {
            let s = services::restart(&reg, &host, &name)?;
            println!("{} {}", s.name, if s.running { "running" } else { "not running" });
            Ok(i32::from(!s.running))
        }
    }
}

// ---- worktrees -----------------------------------------------------------

#[derive(Subcommand)]
pub enum WorktreesCmd {
    /// Linked worktrees with owner, state and whether they can go.
    List {
        /// Only repos whose path contains this.
        #[arg(long)]
        repo: Option<String>,
        /// Include main checkouts.
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 24)]
        min_idle_hours: u64,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove every worktree that is clean, integrated, idle and unowned,
    /// re-checking each one first. Branches stay.
    Clean {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long, default_value_t = 24)]
        min_idle_hours: u64,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        host: Option<String>,
    },
    /// Create `<repo>-wt/<name>` when the main checkout cannot take the work.
    Add {
        repo: PathBuf,
        name: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        base: Option<String>,
        /// The collision: who is in the main checkout, or which builds clash.
        #[arg(long)]
        reason: String,
        #[arg(long)]
        owner: Option<String>,
        /// The task or T3 thread this belongs to.
        #[arg(long)]
        task: Option<String>,
    },
    /// Remove a worktree once its work is integrated.
    Done {
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        min_idle_hours: u64,
    },
}

pub(crate) fn scan(repo: &Option<String>, min_idle_hours: u64) -> (worktrees::Scan, Vec<worktrees::Worktree>) {
    let cx = worktrees::Scan::gather(min_idle_hours * 3_600_000);
    let all = worktrees::inventory(&worktrees::default_roots(), &cx)
        .into_iter()
        .filter(|w| repo.as_ref().is_none_or(|r| w.repo.to_string_lossy().contains(r.as_str())))
        .collect();
    (cx, all)
}

fn owner_label(w: &worktrees::Worktree) -> String {
    match &w.owner {
        Some(worktrees::Owner::T3 { title, archived, .. }) => format!("t3 \"{title}\"{}", if *archived { " (archived)" } else { "" }),
        Some(worktrees::Owner::Meshtop(m)) => format!("{} ({})", m.owner, m.task.as_deref().unwrap_or(&m.reason)),
        None => "unknown".into(),
    }
}

pub fn worktrees_cmd(cmd: WorktreesCmd) -> Result<i32> {
    let home = paths::home();
    let short = |p: &std::path::Path| p.strip_prefix(&home).map(|r| format!("~/{}", r.display())).unwrap_or_else(|_| p.display().to_string());
    match cmd {
        WorktreesCmd::List { repo, all, min_idle_hours, host, json } => {
            if let Some(h) = host {
                let mut a = vec!["worktrees".into(), "list".into(), "--min-idle-hours".into(), min_idle_hours.to_string()];
                if let Some(r) = repo {
                    a.extend(["--repo".into(), r]);
                }
                a.extend(flag_args(&[(all, "--all"), (json, "--json")]));
                return run_remote(&h, &a, None, Duration::from_secs(120));
            }
            let (_, list) = scan(&repo, min_idle_hours);
            let list: Vec<_> = list.into_iter().filter(|w| all || !w.main).collect();
            if json {
                return print_json(&list).map(|_| 0);
            }
            let mut last = PathBuf::new();
            for w in &list {
                if w.repo != last {
                    println!("{}", short(&w.repo));
                    last = w.repo.clone();
                }
                let v = match w.verdict {
                    worktrees::Verdict::Removable => "removable".to_string(),
                    worktrees::Verdict::Prunable => "prunable".to_string(),
                    worktrees::Verdict::Keep => format!("keep: {}", w.reasons.join("; ")),
                };
                let age = if w.last_activity > 0 { ago(w.last_activity) } else { "-".into() };
                println!("  {:<48} {:<28} {:>4}  {:<20} {v}", short(&w.path), w.branch.as_deref().unwrap_or("(detached)"), age, owner_label(w));
            }
            let count = |v: worktrees::Verdict| list.iter().filter(|w| w.verdict == v).count();
            println!(
                "{} worktrees: {} removable, {} prunable, {} kept",
                list.len(),
                count(worktrees::Verdict::Removable),
                count(worktrees::Verdict::Prunable),
                count(worktrees::Verdict::Keep)
            );
            Ok(0)
        }
        WorktreesCmd::Clean { repo, min_idle_hours, dry_run, host } => {
            if let Some(h) = host {
                let mut a = vec!["worktrees".into(), "clean".into(), "--min-idle-hours".into(), min_idle_hours.to_string()];
                if let Some(r) = repo {
                    a.extend(["--repo".into(), r]);
                }
                a.extend(flag_args(&[(dry_run, "--dry-run")]));
                return run_remote(&h, &a, None, Duration::from_secs(300));
            }
            let (cx, list) = scan(&repo, min_idle_hours);
            let (mut done, mut failed) = (0, 0);
            for w in list.iter().filter(|w| w.verdict != worktrees::Verdict::Keep) {
                let what = if w.verdict == worktrees::Verdict::Prunable { "prune" } else { "remove" };
                if dry_run {
                    println!("would {what} {} ({})", short(&w.path), w.branch.as_deref().unwrap_or("detached"));
                    continue;
                }
                match worktrees::remove(w, &cx, false) {
                    Ok(()) => {
                        done += 1;
                        println!("{what}d {} (branch {} kept)", short(&w.path), w.branch.as_deref().unwrap_or("-"));
                    }
                    Err(e) => {
                        failed += 1;
                        println!("kept {}: {e}", short(&w.path));
                    }
                }
            }
            let kept = list.iter().filter(|w| w.verdict == worktrees::Verdict::Keep && !w.main).count();
            println!("{done} cleaned, {failed} kept on re-check, {kept} kept with work or an owner");
            Ok(i32::from(failed > 0))
        }
        WorktreesCmd::Add { repo, name, branch, base, reason, owner, task } => {
            let owner = owner.or_else(|| std::env::var("USER").ok()).unwrap_or_default();
            let marker = worktrees::Marker { owner, task, reason, created: 0 };
            let path = worktrees::add(&repo, &name, branch.as_deref().unwrap_or(&name), base.as_deref(), marker)?;
            println!("{}", path.display());
            Ok(0)
        }
        WorktreesCmd::Done { path, min_idle_hours } => {
            let path = path.canonicalize()?;
            let (cx, list) = scan(&None, min_idle_hours);
            let w = list.iter().find(|w| w.path == path).with_context(|| format!("{} is not a linked worktree", path.display()))?;
            worktrees::remove(w, &cx, true)?;
            println!("removed {}; branch {} kept", short(&path), w.branch.as_deref().unwrap_or("-"));
            Ok(0)
        }
    }
}

fn flag_args(flags: &[(bool, &str)]) -> Vec<String> {
    flags.iter().filter(|(b, _)| *b).map(|(_, f)| f.to_string()).collect()
}
