//! Recoverable long jobs (builds, tests, applies) with deterministic state.
//!
//! Each job lives in `$STATE/jobs/<id>/`:
//! - `job.json`     what was asked, who asked, which boot and supervisor
//! - `log`          combined stdout and stderr, appended
//! - `result.json`  exit status, written once by the supervisor
//! - `notify.json`  webhook delivery attempts and the ack
//! - `lock`         flock held by the supervisor and inherited by the child
//!
//! A job is running exactly while someone holds `lock`. The kernel drops a
//! flock when the last holder exits, so a crashed supervisor can never leave a
//! stale "running" behind, and a child that outlives its supervisor still
//! counts as running. A job with no result and a free lock is `lost`: its
//! outcome is unknown, never assumed to be a failure or a success.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;

/// Exit code when the job has no result and nobody holds its lock.
pub const EXIT_LOST: i32 = 74;
/// Exit code for `wait` when the deadline passed while the job still runs.
pub const EXIT_STILL_RUNNING: i32 = 75;
/// Exit code when the remote host could not be asked at all.
pub const EXIT_TRANSPORT: i32 = 76;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Spec {
    pub id: String,
    pub cmd: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Who started it: a thread id, a person, a script.
    #[serde(default)]
    pub owner: String,
    /// Resources this job writes, e.g. a checkout path. Two jobs sharing a
    /// lock name never run at once; the second ends as `blocked`.
    #[serde(default)]
    pub locks: Vec<String>,
    /// URL that receives a JSON POST when the job ends.
    pub notify: Option<String>,
    /// Extra environment for the command.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Record {
    #[serde(flatten)]
    pub spec: Spec,
    pub host: String,
    pub created: u64,
    /// Seconds since the epoch at boot; a different value means the machine
    /// restarted since the job began.
    pub boot: u64,
    pub supervisor_pid: Option<u32>,
    pub pid: Option<u32>,
    pub started: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Outcome {
    /// `None` when killed by a signal or the command never started.
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub finished: u64,
    /// Set when the job did not run its command: spawn failure, lock busy.
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Delivery {
    pub url: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub delivered: Option<u64>,
    pub acked: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Recorded but the supervisor has not taken the lock yet.
    Starting,
    Running,
    Succeeded,
    Failed,
    /// A lock it needed was held by another job; the command never ran.
    Blocked,
    Cancelled,
    /// No result and nobody holds the lock: the outcome is unknown.
    Lost,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Status {
    pub id: String,
    pub state: State,
    pub record: Record,
    pub outcome: Option<Outcome>,
    pub delivery: Option<Delivery>,
    /// Milliseconds since the log last grew. Informational: a quiet compiler
    /// is still running while the lock is held.
    pub quiet_ms: Option<u64>,
    pub log_bytes: u64,
    pub detail: Option<String>,
}

impl Status {
    pub fn finished(&self) -> bool {
        !matches!(self.state, State::Starting | State::Running)
    }
}

pub fn root() -> PathBuf {
    paths::state().join("jobs")
}

fn dir(id: &str) -> PathBuf {
    root().join(id)
}

pub fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 96
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.starts_with('.');
    if !ok {
        bail!("job id {id:?} must be 1-96 chars of [A-Za-z0-9._-]");
    }
    Ok(())
}

pub fn boot_time() -> u64 {
    sysinfo::System::boot_time()
}

fn host_name() -> String {
    let h = sysinfo::System::host_name().unwrap_or_default();
    h.split('.').next().unwrap_or(&h).to_ascii_lowercase()
}

/// A fresh id from the command and the clock, for callers that do not name
/// their job. Callers that may retry should pass their own stable id.
pub fn new_id(cmd: &[String]) -> String {
    let word = cmd
        .first()
        .map(|c| Path::new(c).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default())
        .unwrap_or_default();
    let word: String = word.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
    let ts = jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S");
    format!("{}-{ts}-{:04x}", if word.is_empty() { "job" } else { &word }, std::process::id() & 0xffff)
}

fn open_lock(path: &Path) -> Result<File> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

/// Non-blocking exclusive flock. `Ok(false)` when someone else holds it.
fn try_flock(file: &File) -> Result<bool> {
    let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(err.into())
    }
}

/// True when some live process holds the flock on `path`.
fn held(path: &Path) -> bool {
    match open_lock(path) {
        Ok(f) => !try_flock(&f).unwrap_or(true),
        Err(_) => false,
    }
}

fn lock_file_for(name: &str) -> PathBuf {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(name.as_bytes());
    let hex: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
    paths::state().join("locks").join(format!("{hex}.lock"))
}

/// Who holds a named resource lock right now, if anyone.
pub fn lock_holder(name: &str) -> Option<String> {
    let path = lock_file_for(name);
    if !held(&path) {
        return None;
    }
    std::fs::read_to_string(path.with_extension("owner")).ok().map(|s| s.trim().to_string())
}

/// Records the job and detaches a supervisor. Starting an id that already
/// exists returns its current status instead of running it twice, so a
/// caller whose reply was lost can simply start again.
pub fn start(mut spec: Spec, exe: &Path) -> Result<Status> {
    if spec.id.is_empty() {
        spec.id = new_id(&spec.cmd);
    }
    validate_id(&spec.id)?;
    if spec.cmd.is_empty() {
        bail!("no command given");
    }
    let d = dir(&spec.id);
    std::fs::create_dir_all(root())?;
    // create_dir is the atomic claim: two concurrent starts of one id cannot
    // both get here.
    match std::fs::create_dir(&d) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return status(&spec.id),
        Err(e) => return Err(e).with_context(|| format!("creating {}", d.display())),
    }
    if spec.owner.is_empty() {
        spec.owner = format!("{}@{}", std::env::var("USER").unwrap_or_default(), host_name());
    }
    let record = Record {
        spec: spec.clone(),
        host: host_name(),
        created: paths::now_ms(),
        boot: boot_time(),
        supervisor_pid: None,
        pid: None,
        started: None,
    };
    paths::write_json(&d.join("job.json"), &record)?;
    let log = OpenOptions::new().create(true).append(true).open(d.join("log"))?;
    let mut cmd = Command::new(exe);
    cmd.args(["job", "__supervise", &spec.id])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // A new session: the supervisor survives the ssh connection or terminal
    // that started it, and never receives that terminal's SIGHUP.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("spawning supervisor")?;
    // Reap the intermediate process so it does not linger as a zombie while
    // this caller keeps running (`serve`). The supervisor itself stays up.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    // Bounded: the supervisor takes its lock within milliseconds. Past this
    // the status says `starting`, which callers treat as running.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let s = status(&spec.id)?;
        if s.state != State::Starting || Instant::now() >= deadline {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Runs inside the detached supervisor process.
pub fn supervise(id: &str) -> Result<()> {
    validate_id(id)?;
    let d = dir(id);
    let lock = open_lock(&d.join("lock"))?;
    if !try_flock(&lock)? {
        bail!("job {id} already has a supervisor");
    }
    let mut record: Record = paths::read_json(&d.join("job.json")).context("reading job.json")?;
    record.supervisor_pid = Some(std::process::id());
    paths::write_json(&d.join("job.json"), &record)?;

    let finish = |outcome: Outcome| -> Result<()> {
        paths::write_json(&d.join("result.json"), &outcome)?;
        if let Some(url) = &record.spec.notify {
            deliver(id, url);
        }
        Ok(())
    };

    // Resource locks are held by this process and inherited by the child,
    // like the job lock.
    let mut held_locks = Vec::new();
    for name in &record.spec.locks {
        let path = lock_file_for(name);
        let f = open_lock(&path)?;
        if !try_flock(&f)? {
            let holder = std::fs::read_to_string(path.with_extension("owner")).unwrap_or_default();
            return finish(Outcome {
                code: None,
                signal: None,
                finished: paths::now_ms(),
                error: Some(format!("lock {name:?} is held by {}", holder.trim())),
            });
        }
        paths::write_atomic(&path.with_extension("owner"), format!("{id} ({})", record.spec.owner).as_bytes())?;
        held_locks.push(f);
    }

    let log = OpenOptions::new().create(true).append(true).open(d.join("log"))?;
    let mut cmd = Command::new(&record.spec.cmd[0]);
    cmd.args(&record.spec.cmd[1..]).envs(&record.spec.env).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    if let Some(cwd) = &record.spec.cwd {
        cmd.current_dir(cwd);
    }
    // Own process group, so cancel reaches the compiler's children too.
    cmd.process_group(0);
    let fds: Vec<i32> = std::iter::once(lock.as_raw_fd()).chain(held_locks.iter().map(|f| f.as_raw_fd())).collect();
    unsafe {
        cmd.pre_exec(move || {
            for fd in &fds {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags == -1 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return finish(Outcome {
                code: None,
                signal: None,
                finished: paths::now_ms(),
                error: Some(format!("could not start {}: {e}", record.spec.cmd[0])),
            });
        }
    };
    record.pid = Some(child.id());
    record.started = Some(paths::now_ms());
    paths::write_json(&d.join("job.json"), &record)?;
    let status = child.wait()?;
    use std::os::unix::process::ExitStatusExt;
    finish(Outcome { code: status.code(), signal: status.signal(), finished: paths::now_ms(), error: None })
}

/// Posts the job's final status to `url`, a few times with backoff.
/// Attempts and the outcome are recorded so `reconcile` can resume later.
pub fn deliver(id: &str, url: &str) {
    let path = dir(id).join("notify.json");
    let mut d: Delivery = paths::read_json(&path).unwrap_or_else(|| Delivery { url: url.into(), ..Default::default() });
    if d.delivered.is_some() {
        return;
    }
    let Ok(st) = status(id) else { return };
    let tail = tail(id, 40).unwrap_or_default();
    let body = serde_json::json!({
        "event": "meshtop.job.finished",
        "id": id,
        "host": st.record.host,
        "state": st.state,
        "code": st.outcome.as_ref().and_then(|o| o.code),
        "error": st.outcome.as_ref().and_then(|o| o.error.clone()),
        "owner": st.record.spec.owner,
        "cmd": st.record.spec.cmd.join(" "),
        "tail": tail,
        "ack": format!("meshtop job ack {id}"),
    });
    let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).build().new_agent();
    for wait in [0u64, 2, 10] {
        std::thread::sleep(Duration::from_secs(wait));
        d.attempts += 1;
        match agent.post(url).send_json(&body) {
            Ok(_) => {
                d.delivered = Some(paths::now_ms());
                d.last_error = None;
                break;
            }
            Err(e) => d.last_error = Some(e.to_string()),
        }
    }
    let _ = paths::write_json(&path, &d);
}

pub fn ack(id: &str) -> Result<Delivery> {
    validate_id(id)?;
    let path = dir(id).join("notify.json");
    let mut d: Delivery = paths::read_json(&path).unwrap_or_default();
    d.acked.get_or_insert(paths::now_ms());
    paths::write_json(&path, &d)?;
    Ok(d)
}

fn modified_ms(path: &Path) -> Option<u64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
}

pub fn status(id: &str) -> Result<Status> {
    validate_id(id)?;
    let d = dir(id);
    let record: Record =
        paths::read_json(&d.join("job.json")).with_context(|| format!("no job {id} on {}", host_name()))?;
    let outcome: Option<Outcome> = paths::read_json(&d.join("result.json"));
    let delivery: Option<Delivery> = paths::read_json(&d.join("notify.json"));
    let cancelled = d.join("cancelled").exists();
    let log = d.join("log");
    let log_bytes = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let mut detail = None;
    let state = match &outcome {
        Some(o) if o.error.as_deref().is_some_and(|e| e.starts_with("lock ")) => State::Blocked,
        Some(o) if o.error.is_some() => State::Failed,
        Some(_) if cancelled => State::Cancelled,
        Some(o) if o.code == Some(0) => State::Succeeded,
        Some(_) => State::Failed,
        None if held(&d.join("lock")) => State::Running,
        None if record.supervisor_pid.is_none() && paths::now_ms().saturating_sub(record.created) < 10_000 => {
            State::Starting
        }
        None => {
            detail = Some(if record.boot != boot_time() {
                "the host rebooted while the job ran; its outcome is unknown".into()
            } else {
                "supervisor and command are gone without a result; the outcome is unknown".into()
            });
            State::Lost
        }
    };
    if let Some(o) = &outcome {
        detail = o.error.clone();
    }
    let quiet_ms = match state {
        State::Running => modified_ms(&log).map(|m| paths::now_ms().saturating_sub(m)),
        _ => None,
    };
    Ok(Status { id: id.into(), state, record, outcome, delivery, quiet_ms, log_bytes, detail })
}

pub fn list() -> Vec<Status> {
    let mut out: Vec<Status> = std::fs::read_dir(root())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| status(&e.file_name().to_string_lossy()).ok())
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.record.created));
    out
}

/// The last `lines` lines of the log, read from the end so a huge build log
/// costs no more than its tail.
pub fn tail(id: &str, lines: usize) -> Result<String> {
    validate_id(id)?;
    let mut f = File::open(dir(id).join("log"))?;
    let len = f.metadata()?.len();
    let want = (lines as u64 * 400).max(4096).min(len);
    f.seek(SeekFrom::Start(len - want))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let all: Vec<&str> = text.lines().collect();
    Ok(all[all.len().saturating_sub(lines)..].join("\n"))
}

/// Polls until the job finishes or `timeout` passes. The deadline only
/// bounds this wait; the job keeps running either way.
pub fn wait(id: &str, timeout: Duration) -> Result<Status> {
    let deadline = Instant::now() + timeout;
    loop {
        let s = status(id)?;
        if s.finished() || Instant::now() >= deadline {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// SIGTERM to the job's process group; the supervisor then records the exit.
pub fn cancel(id: &str) -> Result<Status> {
    let s = status(id)?;
    if s.finished() {
        return Ok(s);
    }
    let Some(pid) = s.record.pid else { bail!("job {id} has no command pid yet") };
    if s.record.boot != boot_time() {
        bail!("job {id} is from an earlier boot");
    }
    std::fs::write(dir(id).join("cancelled"), b"")?;
    if unsafe { libc::kill(-(pid as i32), libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error()).context("signalling the job");
    }
    wait(id, Duration::from_secs(5))
}

/// Retries undelivered notifications of finished jobs. Run by `serve` on a
/// timer and safe to run by hand.
pub fn reconcile() -> Vec<String> {
    let mut done = Vec::new();
    for s in list() {
        let Some(url) = s.record.spec.notify.clone() else { continue };
        if !s.finished() {
            continue;
        }
        if s.delivery.as_ref().is_some_and(|d| d.delivered.is_some() || d.attempts >= 12) {
            continue;
        }
        deliver(&s.id, &url);
        done.push(s.id);
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TEST_ENV as ENV;

    fn with_state<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("meshtop-jobs-{}-{}", std::process::id(), paths::now_ms()));
        unsafe { std::env::set_var("MESHTOP_STATE", &dir) };
        let out = f();
        unsafe { std::env::remove_var("MESHTOP_STATE") };
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    /// Runs the supervisor on a thread, standing in for the detached process.
    fn run(spec: Spec) -> std::thread::JoinHandle<Result<()>> {
        let d = dir(&spec.id);
        std::fs::create_dir_all(&d).unwrap();
        let record = Record {
            spec: spec.clone(),
            host: host_name(),
            created: paths::now_ms(),
            boot: boot_time(),
            supervisor_pid: None,
            pid: None,
            started: None,
        };
        paths::write_json(&d.join("job.json"), &record).unwrap();
        let id = spec.id.clone();
        std::thread::spawn(move || supervise(&id))
    }

    #[test]
    fn exit_before_anyone_waits_is_still_reported() {
        with_state(|| {
            run(Spec { id: "fast".into(), cmd: sh("echo built; exit 3"), ..Default::default() }).join().unwrap().unwrap();
            let s = wait("fast", Duration::from_secs(1)).unwrap();
            assert_eq!(s.state, State::Failed);
            assert_eq!(s.outcome.unwrap().code, Some(3));
            assert_eq!(tail("fast", 5).unwrap(), "built");
        })
    }

    #[test]
    fn second_writer_on_a_lock_is_blocked_not_run() {
        with_state(|| {
            // `a` holds the lock until the test lets it go, however slow the machine.
            let go = paths::state().join("go");
            let hold = format!("while [ ! -e '{}' ]; do sleep 0.05; done", go.display());
            let first = run(Spec { id: "a".into(), cmd: sh(&hold), locks: vec!["~/repo".into()], ..Default::default() });
            let deadline = Instant::now() + Duration::from_secs(20);
            while status("a").unwrap().state != State::Running && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            run(Spec { id: "b".into(), cmd: sh("touch ran"), locks: vec!["~/repo".into()], ..Default::default() })
                .join()
                .unwrap()
                .unwrap();
            let b = status("b").unwrap();
            assert_eq!(b.state, State::Blocked, "{b:?}");
            assert!(b.detail.unwrap().contains("a ("));
            std::fs::write(&go, "").unwrap();
            first.join().unwrap().unwrap();
            assert_eq!(status("a").unwrap().state, State::Succeeded);
        })
    }

    #[test]
    fn no_result_and_no_holder_is_lost_not_failed() {
        with_state(|| {
            let d = dir("gone");
            std::fs::create_dir_all(&d).unwrap();
            let record = Record {
                spec: Spec { id: "gone".into(), cmd: sh("true"), ..Default::default() },
                host: host_name(),
                created: paths::now_ms() - 60_000,
                boot: boot_time(),
                supervisor_pid: Some(1),
                pid: Some(1),
                started: Some(paths::now_ms() - 60_000),
            };
            paths::write_json(&d.join("job.json"), &record).unwrap();
            let s = status("gone").unwrap();
            assert_eq!(s.state, State::Lost);
            assert!(s.outcome.is_none());
        })
    }

    #[test]
    fn rejects_ids_that_escape_the_jobs_dir() {
        assert!(validate_id("../x").is_err());
        assert!(validate_id(".hidden").is_err());
        assert!(validate_id("nix-build_g815.1").is_ok());
    }
}
