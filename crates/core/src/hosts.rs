//! Durable host roles from `hosts.toml`, joined with what is observed right
//! now: which OS answered, as which machine, since which boot, how long ago.
//! Only the durable part lives in git; holds and observations live in the
//! state dir, shared by every CLI call, the web UI and waiting jobs, so a
//! host is probed once per freshness window however many agents ask.

use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::registry::Registry;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HostsFile {
    #[serde(default)]
    pub hosts: BTreeMap<String, HostDef>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HostDef {
    #[serde(default)]
    pub description: String,
    /// The machine's own hostname when it differs from the registry name.
    pub hostname: Option<String>,
    #[serde(default)]
    pub always_on: bool,
    #[serde(default)]
    pub modes: Vec<Mode>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Mode {
    /// macos, linux or windows.
    pub os: String,
    /// ssh destination that reaches the host while it runs this OS.
    pub ssh: String,
    #[serde(default)]
    pub roles: Vec<String>,
    /// Roles a task has to ask for by name, e.g. Windows testing on a
    /// gaming install.
    #[serde(default)]
    pub opt_in_roles: Vec<String>,
    #[serde(default)]
    pub restrictions: Vec<String>,
    /// The hostname this mode answers with, when it differs from the host's.
    pub hostname: Option<String>,
}

pub fn load(reg: &Registry) -> Result<HostsFile> {
    let path = reg.hosts_file();
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// A temporary restriction, e.g. "g815 is running a game tonight".
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Hold {
    pub host: String,
    pub reason: String,
    /// Unix ms; `None` until released.
    pub until: Option<u64>,
    pub created: u64,
}

fn holds_path() -> std::path::PathBuf {
    paths::state().join("holds.json")
}

pub fn holds() -> Vec<Hold> {
    let now = paths::now_ms();
    paths::read_json::<Vec<Hold>>(&holds_path())
        .unwrap_or_default()
        .into_iter()
        .filter(|h| h.until.is_none_or(|u| u > now))
        .collect()
}

pub fn hold(host: &str, reason: &str, hours: Option<f64>) -> Result<()> {
    let mut all = holds();
    all.retain(|h| h.host != host);
    let now = paths::now_ms();
    all.push(Hold {
        host: host.into(),
        reason: reason.into(),
        until: hours.map(|h| now + (h * 3_600_000.0) as u64),
        created: now,
    });
    paths::write_json(&holds_path(), &all)
}

pub fn release(host: &str) -> Result<bool> {
    let mut all = holds();
    let before = all.len();
    all.retain(|h| h.host != host);
    paths::write_json(&holds_path(), &all)?;
    Ok(all.len() != before)
}

/// How a host answered. Only `Ready` says anything about what runs there;
/// `Unreachable` is not proof the machine is off.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reach {
    /// Answered over ssh as the expected machine and OS.
    Ready,
    /// sshd answered and rejected our key or its host key changed.
    AuthFailed,
    /// Something answered, but not the expected machine or OS.
    Mismatch,
    /// On the network, but nothing accepts ssh.
    SshRefused,
    /// Connected, then no answer before the deadline.
    Timeout,
    /// Nothing answered: off, asleep, travelling or off the tailnet.
    Unreachable,
    /// No current observation.
    Unknown,
}

impl Reach {
    pub fn as_str(self) -> &'static str {
        match self {
            Reach::Ready => "ready",
            Reach::AuthFailed => "auth failed",
            Reach::Mismatch => "mismatch",
            Reach::SshRefused => "ssh refused",
            Reach::Timeout => "timeout",
            Reach::Unreachable => "unreachable",
            Reach::Unknown => "unknown",
        }
    }

    /// The most telling failure wins when no mode is ready.
    fn rank(self) -> u8 {
        match self {
            Reach::Ready => 0,
            Reach::AuthFailed => 1,
            Reach::Mismatch => 2,
            Reach::SshRefused => 3,
            Reach::Timeout => 4,
            Reach::Unreachable => 5,
            Reach::Unknown => 6,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModeProbe {
    pub os: String,
    pub ssh: String,
    pub reach: Reach,
    pub detail: String,
    pub hostname: Option<String>,
    pub boot_id: Option<String>,
}

/// One look at a host, written to the shared cache.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub at: u64,
    pub reach: Reach,
    pub os: Option<String>,
    pub via: Option<String>,
    pub hostname: Option<String>,
    pub boot_id: Option<String>,
    pub modes: Vec<ModeProbe>,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cached {
    pub latest: Option<Observation>,
    pub last_ready: Option<Observation>,
    /// When the boot id last changed between two ready observations.
    pub boot_changed: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastKnown {
    pub os: String,
    pub at: u64,
    pub hostname: Option<String>,
    pub boot_id: Option<String>,
}

/// Stable JSON shape, `schema` 1. Fields are only added, never renamed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostStatus {
    pub name: String,
    pub description: String,
    pub always_on: bool,
    pub local: bool,
    /// `unknown` when the newest observation is older than the asked-for age.
    pub reach: Reach,
    pub fresh: bool,
    pub observed: Option<u64>,
    pub age_ms: Option<u64>,
    /// The OS running now; only set from a fresh ready observation.
    pub os: Option<String>,
    pub via: Option<String>,
    pub hostname: Option<String>,
    pub boot_id: Option<String>,
    pub boot_changed: Option<u64>,
    /// The last time it was ready, which may be a different OS than now.
    pub last_known: Option<LastKnown>,
    pub modes: Vec<ModeProbe>,
    pub roles: Vec<String>,
    pub opt_in_roles: Vec<String>,
    pub restrictions: Vec<String>,
    /// Roles it can take right now: fresh, ready and not held.
    pub eligible: Vec<String>,
    pub hold: Option<Hold>,
    pub detail: String,
}

fn short(s: &str) -> String {
    s.split('.').next().unwrap_or(s).to_ascii_lowercase()
}

pub fn local_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        _ => "linux",
    }
}

pub fn is_local(name: &str, def: &HostDef) -> bool {
    let local = short(&sysinfo::System::host_name().unwrap_or_default());
    short(name) == local || def.hostname.as_deref().is_some_and(|h| short(h) == local)
}

const MARKER: &str = "meshtop-probe";

/// One round trip that names the OS, the machine and the boot. Valid in sh
/// and fish; PowerShell has neither, so Windows modes go through cmd.
fn probe_command(os: &str) -> String {
    if os == "windows" {
        format!("cmd /c \"echo {MARKER} & ver & hostname\"")
    } else {
        format!("echo {MARKER}; uname -s; hostname; cat /proc/sys/kernel/random/boot_id 2>/dev/null || sysctl -n kern.boottime 2>/dev/null")
    }
}

fn os_matches(os: &str, line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    match os {
        "macos" => l.contains("darwin"),
        "linux" => l.contains("linux"),
        "windows" => l.contains("windows"),
        _ => false,
    }
}

/// `sysctl kern.boottime` prints `{ sec = 1793..., usec = ... } Tue ...`.
fn boot_token(line: &str) -> String {
    match line.split("sec = ").nth(1).and_then(|r| r.split(',').next()) {
        Some(sec) => format!("sec={}", sec.trim()),
        None => line.trim().to_string(),
    }
}

/// Reads the probe's stdout. `expect` is the hostname the mode should
/// answer with; a different machine behind the alias is a mismatch.
pub fn parse_probe(mode: &Mode, expect: &str, stdout: &str) -> ModeProbe {
    let mut p = ModeProbe { os: mode.os.clone(), ssh: mode.ssh.clone(), reach: Reach::Mismatch, detail: String::new(), hostname: None, boot_id: None };
    let lines: Vec<&str> = stdout.lines().map(str::trim).skip_while(|l| *l != MARKER).skip(1).filter(|l| !l.is_empty()).collect();
    let Some(os_line) = lines.first() else {
        p.detail = format!("unexpected answer: {}", stdout.lines().find(|l| !l.trim().is_empty()).unwrap_or("nothing").trim());
        return p;
    };
    p.hostname = lines.get(1).map(|h| h.to_string());
    p.boot_id = lines.get(2).map(|b| boot_token(b));
    if !os_matches(&mode.os, os_line) {
        p.detail = format!("answered as {os_line}, expected {}", mode.os);
    } else if p.hostname.as_deref().is_some_and(|h| short(h) != short(expect)) {
        p.detail = format!("answered as {}, expected {expect}", p.hostname.as_deref().unwrap_or("?"));
    } else {
        p.reach = Reach::Ready;
        p.detail = format!("{} via {}", mode.os, mode.ssh);
    }
    p
}

/// ssh's own error text, mapped to what it says about the machine.
pub fn classify_ssh_error(stderr: &str) -> Reach {
    let e = stderr.to_ascii_lowercase();
    let has = |n: &[&str]| n.iter().any(|s| e.contains(s));
    if has(&["permission denied", "host key verification failed", "remote host identification has changed", "too many authentication failures", "no matching host key"]) {
        Reach::AuthFailed
    } else if has(&["connection refused"]) {
        Reach::SshRefused
    } else if has(&["timed out", "no route to host", "network is unreachable", "could not resolve", "name or service not known", "nodename nor servname", "host is down", "temporary failure in name resolution"]) {
        Reach::Unreachable
    } else if has(&["connection closed", "connection reset", "broken pipe", "kex_exchange_identification"]) {
        Reach::Timeout
    } else {
        Reach::Unreachable
    }
}

fn last_line(s: &str) -> String {
    s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

/// A tailnet peer's own view of whether it is online. Asking the local
/// tailscaled costs nothing, so a waiter can poll it and only ssh once the
/// peer comes back.
#[derive(Clone, Debug)]
pub struct Peer {
    pub online: bool,
    pub last_seen: Option<String>,
}

pub fn tailscale_peers() -> Option<Vec<(Vec<String>, Peer)>> {
    let out = run_bounded(Command::new("tailscale").args(["status", "--json"]), Duration::from_secs(3))?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut peers = Vec::new();
    for p in v.get("Peer")?.as_object()?.values() {
        let mut names: Vec<String> = Vec::new();
        if let Some(h) = p.get("HostName").and_then(|x| x.as_str()) {
            names.push(short(h));
        }
        if let Some(d) = p.get("DNSName").and_then(|x| x.as_str()) {
            names.push(d.trim_end_matches('.').to_ascii_lowercase());
            names.push(short(d));
        }
        for ip in p.get("TailscaleIPs").and_then(|x| x.as_array()).into_iter().flatten() {
            if let Some(ip) = ip.as_str() {
                names.push(ip.to_string());
            }
        }
        let seen = p.get("LastSeen").and_then(|x| x.as_str()).filter(|s| !s.starts_with("0001")).map(String::from);
        peers.push((names, Peer { online: p.get("Online").and_then(|x| x.as_bool()).unwrap_or(false), last_seen: seen }));
    }
    Some(peers)
}

/// The machine an ssh alias points at, per `ssh -G`.
fn ssh_hostname(alias: &str) -> String {
    run_bounded(Command::new("ssh").args(["-G", alias]), Duration::from_secs(3))
        .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().find_map(|l| l.strip_prefix("hostname ").map(|h| h.trim().to_ascii_lowercase())))
        .unwrap_or_else(|| alias.to_ascii_lowercase())
}

fn peer_for<'a>(peers: &'a [(Vec<String>, Peer)], alias: &str) -> Option<&'a Peer> {
    let target = ssh_hostname(alias);
    peers.iter().find(|(names, _)| names.iter().any(|n| *n == target || *n == short(&target))).map(|(_, p)| p)
}

struct Output {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs a child with a hard deadline; `None` when it could not start or
/// was killed at the deadline.
fn run_bounded(cmd: &mut Command, limit: Duration) -> Option<Output> {
    let o = run_until(cmd, limit)?;
    o.status.is_some().then_some(o)
}

fn run_until(cmd: &mut Command, limit: Duration) -> Option<Output> {
    use std::io::Read;
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
    let (mut so, mut se) = (child.stdout.take()?, child.stderr.take()?);
    let a = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let b = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st.code().unwrap_or(-1)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    Some(Output { status, stdout: a.join().unwrap_or_default(), stderr: b.join().unwrap_or_default() })
}

fn expected_hostname<'a>(name: &'a str, def: &'a HostDef, mode: &'a Mode) -> &'a str {
    mode.hostname.as_deref().or(def.hostname.as_deref()).unwrap_or(name)
}

pub fn probe_mode(name: &str, def: &HostDef, mode: &Mode, peers: Option<&[(Vec<String>, Peer)]>, timeout: Duration) -> ModeProbe {
    let base = |reach, detail: String| ModeProbe { os: mode.os.clone(), ssh: mode.ssh.clone(), reach, detail, hostname: None, boot_id: None };
    if let Some(p) = peers.and_then(|ps| peer_for(ps, &mode.ssh))
        && !p.online {
            let seen = p.last_seen.as_deref().map(|s| format!(", last seen {s}")).unwrap_or_default();
            return base(Reach::Unreachable, format!("tailscale reports {} offline{seen}", mode.ssh));
        }
    let mut cmd = Command::new("ssh");
    cmd.args(["-T", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "-o", "StrictHostKeyChecking=yes"])
        .arg("-o")
        .arg(format!("ConnectTimeout={}", timeout.as_secs().max(1)))
        .arg(&mode.ssh)
        .arg(probe_command(&mode.os));
    // ConnectTimeout covers the TCP connect only; a host that accepts and
    // then stalls would otherwise hold the probe forever.
    match run_until(&mut cmd, timeout + Duration::from_secs(3)) {
        None => base(Reach::Unknown, "could not run ssh".into()),
        Some(Output { status: None, .. }) => base(Reach::Timeout, format!("no answer from {} within {}s", mode.ssh, timeout.as_secs() + 3)),
        Some(Output { status: Some(255), stderr, .. }) => {
            let err = String::from_utf8_lossy(&stderr);
            base(classify_ssh_error(&err), last_line(&err))
        }
        Some(o) => parse_probe(mode, expected_hostname(name, def, mode), &String::from_utf8_lossy(&o.stdout)),
    }
}

fn local_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| format!("sec={}", sysinfo::System::boot_time()))
}

/// Probes now, without the cache. Every mode is tried so a dual-boot host
/// reports the OS it is really running and what the other alias said.
pub fn probe(name: &str, def: &HostDef, timeout: Duration) -> Observation {
    let at = paths::now_ms();
    if is_local(name, def) {
        let os = local_os().to_string();
        let probe = ModeProbe {
            os: os.clone(),
            ssh: "local".into(),
            reach: Reach::Ready,
            detail: "this machine".into(),
            hostname: sysinfo::System::host_name(),
            boot_id: Some(local_boot_id()),
        };
        return Observation { at, reach: Reach::Ready, os: Some(os), via: Some("local".into()), hostname: probe.hostname.clone(), boot_id: probe.boot_id.clone(), modes: vec![probe], detail: "this machine".into() };
    }
    let peers = tailscale_peers();
    let modes: Vec<ModeProbe> = std::thread::scope(|s| {
        let hs: Vec<_> = def.modes.iter().map(|m| s.spawn(|| probe_mode(name, def, m, peers.as_deref(), timeout))).collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    observation(at, modes)
}

pub fn observation(at: u64, modes: Vec<ModeProbe>) -> Observation {
    let best = modes.iter().min_by_key(|m| m.reach.rank());
    let reach = best.map_or(Reach::Unknown, |m| m.reach);
    let (os, via, hostname, boot_id, detail) = match best {
        Some(m) if m.reach == Reach::Ready => (Some(m.os.clone()), Some(m.ssh.clone()), m.hostname.clone(), m.boot_id.clone(), m.detail.clone()),
        _ => (None, None, None, None, modes.iter().map(|m| format!("{}: {}", m.os, m.detail)).collect::<Vec<_>>().join("; ")),
    };
    Observation { at, reach, os, via, hostname, boot_id, modes, detail }
}

fn cache_dir() -> PathBuf {
    paths::state().join("hosts")
}

pub fn cached(name: &str) -> Cached {
    paths::read_json(&cache_dir().join(format!("{name}.json"))).unwrap_or_default()
}

/// Records an observation, carrying the last ready one and boot changes.
pub fn record(name: &str, obs: Observation) -> Result<Cached> {
    let mut c = cached(name);
    if obs.reach == Reach::Ready {
        if let Some(prev) = &c.last_ready
            && prev.boot_id.is_some() && obs.boot_id.is_some() && prev.boot_id != obs.boot_id {
                c.boot_changed = Some(obs.at);
            }
        c.last_ready = Some(obs.clone());
    }
    c.latest = Some(obs);
    paths::write_json(&cache_dir().join(format!("{name}.json")), &c)?;
    Ok(c)
}

/// The newest observation no older than `max_age`, probing only when there
/// is none. Callers asking at once share one probe: the first takes the
/// host's lock and probes, the rest wait on the lock and read its result.
pub fn observe_with(name: &str, max_age: Duration, probe: &dyn Fn() -> Observation) -> Result<Cached> {
    let fresh = |c: &Cached| c.latest.as_ref().is_some_and(|o| paths::now_ms().saturating_sub(o.at) < max_age.as_millis() as u64);
    let c = cached(name);
    if fresh(&c) {
        return Ok(c);
    }
    std::fs::create_dir_all(cache_dir())?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(cache_dir().join(format!("{name}.lock")))?;
    // Held at most one probe long, which is bounded by its own deadline.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("locking the host cache");
    }
    let c = cached(name);
    if fresh(&c) {
        return Ok(c);
    }
    record(name, probe())
}

pub fn observe(name: &str, def: &HostDef, max_age: Duration, timeout: Duration) -> Result<Cached> {
    observe_with(name, max_age, &|| probe(name, def, timeout))
}

/// Joins the definition with the cache. Anything older than `max_age` is
/// reported as unknown with its last-known OS beside it, so a g815 seen in
/// Linux an hour ago is never treated as a Linux builder now.
pub fn status_from(name: &str, def: &HostDef, c: &Cached, max_age: Duration, holds: &[Hold]) -> HostStatus {
    let now = paths::now_ms();
    let latest = c.latest.as_ref();
    let age = latest.map(|o| now.saturating_sub(o.at));
    let fresh = age.is_some_and(|a| a < max_age.as_millis() as u64);
    let ready = fresh && latest.is_some_and(|o| o.reach == Reach::Ready);
    let os = if ready { latest.and_then(|o| o.os.clone()) } else { None };
    let mode = os.as_ref().and_then(|os| def.modes.iter().find(|m| &m.os == os));
    let hold = holds.iter().find(|h| h.host == name).cloned();
    let roles = mode.map(|m| m.roles.clone()).unwrap_or_default();
    let opt_in = mode.map(|m| m.opt_in_roles.clone()).unwrap_or_default();
    let eligible = if hold.is_none() { roles.iter().chain(&opt_in).cloned().collect() } else { vec![] };
    let detail = match latest {
        None => "never observed".into(),
        Some(o) if !fresh => format!("last observation {}s old: {}", age.unwrap_or(0) / 1000, o.detail),
        Some(o) => o.detail.clone(),
    };
    HostStatus {
        name: name.into(),
        description: def.description.clone(),
        always_on: def.always_on,
        local: is_local(name, def),
        reach: if fresh { latest.map_or(Reach::Unknown, |o| o.reach) } else { Reach::Unknown },
        fresh,
        observed: latest.map(|o| o.at),
        age_ms: age,
        via: if ready { latest.and_then(|o| o.via.clone()) } else { None },
        hostname: if ready { latest.and_then(|o| o.hostname.clone()) } else { None },
        boot_id: if ready { latest.and_then(|o| o.boot_id.clone()) } else { None },
        boot_changed: c.boot_changed,
        last_known: c.last_ready.as_ref().map(|o| LastKnown { os: o.os.clone().unwrap_or_default(), at: o.at, hostname: o.hostname.clone(), boot_id: o.boot_id.clone() }),
        modes: if fresh { latest.map(|o| o.modes.clone()).unwrap_or_default() } else { vec![] },
        os,
        roles,
        opt_in_roles: opt_in,
        restrictions: mode.map(|m| m.restrictions.clone()).unwrap_or_default(),
        eligible,
        hold,
        detail,
    }
}

/// Status of the named hosts (all when empty), probing in parallel only
/// those whose cached observation is older than `max_age`. `cached_only`
/// never probes.
pub fn status_all(file: &HostsFile, only: &[String], max_age: Duration, timeout: Duration, cached_only: bool) -> Vec<HostStatus> {
    let holds = holds();
    let picked: Vec<(&String, &HostDef)> = file.hosts.iter().filter(|(n, _)| only.is_empty() || only.contains(n)).collect();
    std::thread::scope(|s| {
        let hs: Vec<_> = picked
            .iter()
            .map(|(name, def)| {
                s.spawn(|| {
                    let c = if cached_only { cached(name) } else { observe(name, def, max_age, timeout).unwrap_or_else(|_| cached(name)) };
                    status_from(name, def, &c, max_age, &holds)
                })
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/// Why a host cannot take a role right now, or `None` when it can.
pub fn unsuitable(status: &HostStatus, role: &str, heavy: bool) -> Option<String> {
    if !status.fresh {
        let last = status.last_known.as_ref().map(|l| format!(", last ready in {}", l.os)).unwrap_or_default();
        return Some(format!("no fresh observation{last}"));
    }
    if status.reach != Reach::Ready {
        return Some(format!("{}: {}", status.reach.as_str(), status.detail));
    }
    if let Some(h) = &status.hold {
        return Some(format!("held: {}", h.reason));
    }
    if !status.roles.iter().chain(&status.opt_in_roles).any(|r| r == role) {
        return Some(format!("running {}, which has no role {role}", status.os.as_deref().unwrap_or("?")));
    }
    if heavy && status.restrictions.iter().any(|r| r == "no-heavy-builds") {
        return Some("no heavy builds".into());
    }
    None
}

/// The first host that can take `role`, in hosts.toml order (alphabetical).
/// Opt-in roles only match when asked for by name, which is what `role` is.
pub fn pick<'a>(statuses: &'a [HostStatus], role: &str, heavy: bool) -> Result<&'a HostStatus, Vec<(String, String)>> {
    let mut why = Vec::new();
    for s in statuses {
        match unsuitable(s, role, heavy) {
            None => return Ok(s),
            Some(r) => why.push((s.name.clone(), r)),
        }
    }
    Err(why)
}

/// What a waiter needs from a host before it goes on.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Want {
    pub os: Option<String>,
    pub role: Option<String>,
    #[serde(default)]
    pub heavy: bool,
    /// A command that must exit 0 on the host, e.g. a service check.
    pub check: Option<String>,
}

/// `Err((needs_a_person, why))`. A wrong OS, a hold or an unreachable host
/// can change by itself; a rejected key or the wrong machine behind every
/// alias that could satisfy the wait cannot.
pub fn meets(s: &HostStatus, want: &Want) -> Result<(), (bool, String)> {
    if !s.fresh {
        return Err((false, "no fresh observation".into()));
    }
    if s.reach != Reach::Ready {
        let relevant: Vec<&ModeProbe> = s.modes.iter().filter(|m| want.os.as_ref().is_none_or(|os| &m.os == os)).collect();
        let permanent = !relevant.is_empty() && relevant.iter().all(|m| matches!(m.reach, Reach::AuthFailed | Reach::Mismatch));
        return Err((permanent, format!("{}: {}", s.reach.as_str(), s.detail)));
    }
    if let Some(os) = &want.os
        && s.os.as_ref() != Some(os) {
            return Err((false, format!("running {}, waiting for {os}", s.os.as_deref().unwrap_or("?"))));
        }
    if let Some(h) = &s.hold {
        return Err((false, format!("held: {}", h.reason)));
    }
    if let Some(role) = &want.role
        && let Some(why) = unsuitable(s, role, want.heavy) {
            return Err((false, why));
        }
    Ok(())
}

/// Runs `want.check` on the host through the mode that answered.
pub fn run_check(s: &HostStatus, cmd: &str, timeout: Duration) -> Result<(), String> {
    let mut c = if s.local {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    } else {
        let mut c = Command::new("ssh");
        c.args(["-T", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=8"]).arg(s.via.as_deref().unwrap_or(&s.name)).arg(cmd);
        c
    };
    match run_until(&mut c, timeout) {
        Some(Output { status: Some(0), .. }) => Ok(()),
        Some(Output { status: Some(code), stderr, stdout }) => {
            let text = String::from_utf8_lossy(&stderr).to_string() + &String::from_utf8_lossy(&stdout);
            Err(format!("check exited {code}: {}", last_line(&text)))
        }
        Some(Output { status: None, .. }) => Err(format!("check gave no answer within {}s", timeout.as_secs())),
        None => Err("check could not start".into()),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "end", content = "reason")]
pub enum WaitEnd {
    Ready,
    /// Waiting cannot help: a rejected key, the wrong machine.
    Blocked(String),
    Deadline(String),
    Cancelled(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaitResult {
    #[serde(flatten)]
    pub end: WaitEnd,
    pub status: HostStatus,
    pub waited_ms: u64,
}

impl WaitResult {
    pub fn exit_code(&self) -> i32 {
        match self.end {
            WaitEnd::Ready => 0,
            WaitEnd::Blocked(_) => 1,
            WaitEnd::Deadline(_) => crate::jobs::EXIT_STILL_RUNNING,
            WaitEnd::Cancelled(_) => 130,
        }
    }
}

pub struct Waiter<'a> {
    /// The current status, from an observation no older than the argument.
    pub observe: &'a dyn Fn(Duration) -> HostStatus,
    /// Cheap and local: true when it is worth probing before the interval
    /// is up, e.g. tailscale says an offline peer is back.
    pub wake: &'a dyn Fn() -> bool,
    pub check: &'a dyn Fn(&HostStatus) -> Result<(), String>,
    pub cancel: &'a AtomicBool,
}

/// Waits until the host meets `want`, the deadline passes, it is cancelled,
/// or the host answers in a way waiting cannot fix. Probes go through the
/// shared cache, so many waiters on one host cost one probe per interval.
pub fn wait(w: &Waiter, want: &Want, timeout: Duration, interval: Duration) -> WaitResult {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut max_age = interval;
    loop {
        let s = (w.observe)(max_age);
        let why = match meets(&s, want) {
            Ok(()) => match want.check.as_ref().map(|_| (w.check)(&s)) {
                None | Some(Ok(())) => return WaitResult { end: WaitEnd::Ready, status: s, waited_ms: start.elapsed().as_millis() as u64 },
                Some(Err(e)) => e,
            },
            Err((true, why)) => return WaitResult { end: WaitEnd::Blocked(why), status: s, waited_ms: start.elapsed().as_millis() as u64 },
            Err((false, why)) => why,
        };
        let next = (Instant::now() + interval).min(deadline);
        max_age = interval;
        let mut last_wake = Instant::now();
        loop {
            if w.cancel.load(Ordering::Relaxed) {
                return WaitResult { end: WaitEnd::Cancelled(why), status: s, waited_ms: start.elapsed().as_millis() as u64 };
            }
            if Instant::now() >= deadline {
                return WaitResult { end: WaitEnd::Deadline(why), status: s, waited_ms: start.elapsed().as_millis() as u64 };
            }
            if Instant::now() >= next {
                break;
            }
            if last_wake.elapsed() >= Duration::from_secs(3) {
                last_wake = Instant::now();
                if (w.wake)() {
                    max_age = Duration::ZERO;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100).min(next.saturating_duration_since(Instant::now())));
        }
    }
}

/// True when tailscale now reports online an alias the last observation
/// found offline, so the waiter probes now instead of at the next interval.
pub fn tailscale_wake(last: &HostStatus) -> bool {
    let offline: Vec<&str> = last.modes.iter().filter(|m| m.reach == Reach::Unreachable && m.detail.starts_with("tailscale")).map(|m| m.ssh.as_str()).collect();
    if offline.is_empty() {
        return false;
    }
    let Some(peers) = tailscale_peers() else { return false };
    offline.iter().any(|a| peer_for(&peers, a).is_some_and(|p| p.online))
}

static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

/// SIGINT and SIGTERM set the returned flag instead of killing the process,
/// so a wait can report how far it got.
pub fn cancel_on_signal() -> &'static AtomicBool {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    &CANCEL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TEST_ENV as ENV;
    use std::sync::atomic::AtomicUsize;

    fn g815() -> HostDef {
        toml::from_str(
            r#"
            hostname = "g815"
            [[modes]]
            os = "linux"
            ssh = "g815"
            roles = ["builder", "gpu"]
            [[modes]]
            os = "windows"
            ssh = "windows"
            hostname = "DESKTOP-VJMK52D"
            roles = ["gaming"]
            opt_in_roles = ["windows-test"]
            "#,
        )
        .unwrap()
    }

    fn mp(os: &str, ssh: &str, reach: Reach, detail: &str) -> ModeProbe {
        ModeProbe { os: os.into(), ssh: ssh.into(), reach, detail: detail.into(), hostname: None, boot_id: Some("b1".into()) }
    }

    fn cache(age_ms: u64, modes: Vec<ModeProbe>) -> Cached {
        let obs = observation(paths::now_ms() - age_ms, modes);
        Cached { last_ready: (obs.reach == Reach::Ready).then(|| obs.clone()), latest: Some(obs), boot_changed: None }
    }

    const MIN: Duration = Duration::from_secs(60);

    fn in_windows() -> HostStatus {
        let c = cache(0, vec![mp("linux", "g815", Reach::Unreachable, "tailscale reports g815 offline"), mp("windows", "windows", Reach::Ready, "windows via windows")]);
        status_from("g815", &g815(), &c, MIN, &[])
    }

    #[test]
    fn windows_boot_is_not_a_linux_builder() {
        let s = in_windows();
        assert_eq!((s.reach, s.os.as_deref()), (Reach::Ready, Some("windows")));
        let err = pick(std::slice::from_ref(&s), "builder", true).unwrap_err();
        assert!(err[0].1.contains("running windows"), "{err:?}");
        assert!(pick(std::slice::from_ref(&s), "windows-test", false).is_ok());
        let want = Want { os: Some("linux".into()), ..Default::default() };
        assert_eq!(meets(&s, &want), Err((false, "running windows, waiting for linux".into())));
    }

    #[test]
    fn stale_linux_state_is_unknown_not_linux() {
        let c = cache(10 * 60_000, vec![mp("linux", "g815", Reach::Ready, "linux via g815")]);
        let s = status_from("g815", &g815(), &c, MIN, &[]);
        assert_eq!((s.reach, s.fresh, s.os.as_deref()), (Reach::Unknown, false, None));
        assert_eq!(s.last_known.as_ref().map(|l| l.os.as_str()), Some("linux"));
        assert!(s.eligible.is_empty());
        let err = pick(std::slice::from_ref(&s), "builder", true).unwrap_err();
        assert!(err[0].1.contains("no fresh observation, last ready in linux"), "{err:?}");
    }

    #[test]
    fn linux_boot_builds_unless_held() {
        let c = cache(0, vec![mp("linux", "g815", Reach::Ready, "linux via g815"), mp("windows", "windows", Reach::Unreachable, "timed out")]);
        let s = status_from("g815", &g815(), &c, MIN, &[]);
        assert_eq!(pick(std::slice::from_ref(&s), "builder", true).unwrap().name, "g815");
        let hold = Hold { host: "g815".into(), reason: "gaming".into(), until: None, created: 0 };
        let held = status_from("g815", &g815(), &c, MIN, &[hold]);
        assert!(pick(std::slice::from_ref(&held), "builder", true).is_err());
        assert!(held.eligible.is_empty());
    }

    #[test]
    fn weak_client_refuses_heavy_builds() {
        let def: HostDef = toml::from_str(
            r#"
            [[modes]]
            os = "linux"
            ssh = "e1504g"
            roles = ["builder"]
            restrictions = ["no-heavy-builds"]
            "#,
        )
        .unwrap();
        let s = status_from("e1504g", &def, &cache(0, vec![mp("linux", "e1504g", Reach::Ready, "")]), MIN, &[]);
        assert!(pick(std::slice::from_ref(&s), "builder", true).is_err());
        assert!(pick(std::slice::from_ref(&s), "builder", false).is_ok());
    }

    #[test]
    fn probe_output_names_os_machine_and_boot() {
        let def = g815();
        let linux = &def.modes[0];
        let ok = parse_probe(linux, "g815", "motd line\nmeshtop-probe\nLinux\ng815\n1f2e-boot\n");
        assert_eq!((ok.reach, ok.hostname.as_deref(), ok.boot_id.as_deref()), (Reach::Ready, Some("g815"), Some("1f2e-boot")));
        let wrong_os = parse_probe(linux, "g815", "meshtop-probe\nDarwin\ng815\n");
        assert_eq!(wrong_os.reach, Reach::Mismatch);
        assert!(wrong_os.detail.contains("expected linux"), "{}", wrong_os.detail);
        let wrong_box = parse_probe(linux, "g815", "meshtop-probe\nLinux\ne1504g\nx\n");
        assert!(wrong_box.reach == Reach::Mismatch && wrong_box.detail.contains("expected g815"));
        assert_eq!(parse_probe(linux, "g815", "Welcome to fish\n").reach, Reach::Mismatch);
        let win = parse_probe(&def.modes[1], "DESKTOP-VJMK52D", "meshtop-probe\r\n\r\nMicrosoft Windows [Version 10.0.26100]\r\nDESKTOP-VJMK52D\r\n");
        assert_eq!(win.reach, Reach::Ready, "{}", win.detail);
        assert_eq!(boot_token("{ sec = 1793000000, usec = 12 } Tue Oct 27"), "sec=1793000000");
    }

    #[test]
    fn ssh_errors_say_what_failed() {
        assert_eq!(classify_ssh_error("kyan@g815: Permission denied (publickey)."), Reach::AuthFailed);
        assert_eq!(classify_ssh_error("Host key verification failed."), Reach::AuthFailed);
        assert_eq!(classify_ssh_error("ssh: connect to host g815 port 22: Connection refused"), Reach::SshRefused);
        assert_eq!(classify_ssh_error("ssh: connect to host e1504g port 22: Operation timed out"), Reach::Unreachable);
        assert_eq!(classify_ssh_error("ssh: Could not resolve hostname nope"), Reach::Unreachable);
        assert_eq!(classify_ssh_error("Connection closed by 100.64.0.3 port 22"), Reach::Timeout);
    }

    fn quiet_waiter<'a>(observe: &'a dyn Fn(Duration) -> HostStatus, cancel: &'a AtomicBool) -> Waiter<'a> {
        Waiter { observe, wake: &|| false, check: &|_| Ok(()), cancel }
    }

    fn e1504g(reach: Reach) -> HostStatus {
        let def: HostDef = toml::from_str("[[modes]]\nos = \"linux\"\nssh = \"e1504g\"\nroles = [\"dev-client\"]\n").unwrap();
        status_from("e1504g", &def, &cache(0, vec![mp("linux", "e1504g", reach, "x")]), MIN, &[])
    }

    #[test]
    fn wait_returns_once_the_host_comes_back() {
        let calls = AtomicUsize::new(0);
        let observe = |_| e1504g(if calls.fetch_add(1, Ordering::SeqCst) < 2 { Reach::Unreachable } else { Reach::Ready });
        let cancel = AtomicBool::new(false);
        let r = wait(&quiet_waiter(&observe, &cancel), &Want { os: Some("linux".into()), ..Default::default() }, Duration::from_secs(5), Duration::from_millis(20));
        assert_eq!((r.end.clone(), r.exit_code()), (WaitEnd::Ready, 0));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(r.waited_ms < 1000, "{}", r.waited_ms);
    }

    #[test]
    fn wait_gives_up_at_the_deadline_or_on_cancel() {
        let observe = |_| e1504g(Reach::Unreachable);
        let cancel = AtomicBool::new(false);
        let w = quiet_waiter(&observe, &cancel);
        let t = Instant::now();
        let r = wait(&w, &Want::default(), Duration::from_millis(300), Duration::from_millis(50));
        assert!(matches!(r.end, WaitEnd::Deadline(_)) && r.exit_code() == 75, "{:?}", r.end);
        assert!(t.elapsed() < Duration::from_secs(2));

        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(150));
                cancel.store(true, Ordering::SeqCst);
            });
            let t = Instant::now();
            let r = wait(&w, &Want::default(), Duration::from_secs(60), Duration::from_secs(30));
            assert!(matches!(r.end, WaitEnd::Cancelled(_)) && r.exit_code() == 130, "{:?}", r.end);
            assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        });
    }

    #[test]
    fn rejected_key_or_wrong_machine_stops_the_wait() {
        let cancel = AtomicBool::new(false);
        for reach in [Reach::AuthFailed, Reach::Mismatch] {
            let observe = |_| e1504g(reach);
            let r = wait(&quiet_waiter(&observe, &cancel), &Want::default(), Duration::from_secs(60), Duration::from_secs(30));
            assert!(matches!(r.end, WaitEnd::Blocked(_)) && r.exit_code() == 1, "{:?}", r.end);
        }
        // Windows refusing our key says nothing about whether Linux will boot.
        let c = cache(0, vec![mp("linux", "g815", Reach::Unreachable, "timed out"), mp("windows", "windows", Reach::AuthFailed, "denied")]);
        let s = status_from("g815", &g815(), &c, MIN, &[]);
        assert_eq!(meets(&s, &Want { os: Some("linux".into()), ..Default::default() }).unwrap_err().0, false);
        assert_eq!(meets(&s, &Want { os: Some("windows".into()), ..Default::default() }).unwrap_err().0, true);
    }

    #[test]
    fn wake_probes_before_the_interval_and_a_failing_check_keeps_waiting() {
        let calls = AtomicUsize::new(0);
        let observe = |max_age: Duration| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            e1504g(if n > 0 && max_age.is_zero() { Reach::Ready } else { Reach::Unreachable })
        };
        let cancel = AtomicBool::new(false);
        let w = Waiter { observe: &observe, wake: &|| true, check: &|_| Ok(()), cancel: &cancel };
        let r = wait(&w, &Want::default(), Duration::from_secs(20), Duration::from_secs(15));
        assert_eq!(r.end, WaitEnd::Ready);
        assert!(r.waited_ms < 5000, "{}", r.waited_ms);

        let observe = |_| e1504g(Reach::Ready);
        let w = Waiter { observe: &observe, wake: &|| false, check: &|_| Err("check exited 3: t3code not running".into()), cancel: &cancel };
        let r = wait(&w, &Want { check: Some("pgrep t3code".into()), ..Default::default() }, Duration::from_millis(200), Duration::from_millis(50));
        assert_eq!(r.end, WaitEnd::Deadline("check exited 3: t3code not running".into()));
    }

    fn with_state<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("meshtop-hosts-{}-{}", std::process::id(), paths::now_ms()));
        unsafe { std::env::set_var("MESHTOP_STATE", &dir) };
        let out = f();
        unsafe { std::env::remove_var("MESHTOP_STATE") };
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn concurrent_callers_share_one_probe() {
        with_state(|| {
            let probes = AtomicUsize::new(0);
            let probe = || {
                probes.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(200));
                observation(paths::now_ms(), vec![mp("linux", "e1504g", Reach::Ready, "")])
            };
            std::thread::scope(|s| {
                for _ in 0..8 {
                    s.spawn(|| assert_eq!(observe_with("e1504g", MIN, &probe).unwrap().latest.unwrap().reach, Reach::Ready));
                }
            });
            assert_eq!(probes.load(Ordering::SeqCst), 1);
            observe_with("e1504g", Duration::ZERO, &probe).unwrap();
            assert_eq!(probes.load(Ordering::SeqCst), 2);
        })
    }

    #[test]
    fn reboot_and_disconnect_are_recorded() {
        with_state(|| {
            let ready = |boot: &str| {
                let mut m = mp("linux", "e1504g", Reach::Ready, "");
                m.boot_id = Some(boot.into());
                observation(paths::now_ms(), vec![m])
            };
            assert!(record("e1504g", ready("a")).unwrap().boot_changed.is_none());
            assert!(record("e1504g", ready("a")).unwrap().boot_changed.is_none());
            let gone = record("e1504g", observation(paths::now_ms(), vec![mp("linux", "e1504g", Reach::Unreachable, "timed out")])).unwrap();
            assert_eq!(gone.last_ready.as_ref().and_then(|o| o.boot_id.as_deref()), Some("a"));
            let back = record("e1504g", ready("b")).unwrap();
            assert!(back.boot_changed.is_some());
            assert_eq!(back.last_ready.unwrap().boot_id.as_deref(), Some("b"));
        })
    }
}
