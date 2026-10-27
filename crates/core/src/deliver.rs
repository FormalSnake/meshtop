//! A delivery is a build, a wait for the target, and an install, written
//! down before any of it runs. It outlives the agent that asked for it: the
//! target can be off for days (a laptop travelling) while the artifact
//! waits, and whoever resumes it continues from the recorded phase instead
//! of replaying steps whose outcome is unknown.
//!
//! The driver only ever waits for a host. It never powers one on, wakes it
//! or reboots it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::hosts::Want;
use crate::jobs::{self, Spec, State};
use crate::paths;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Step {
    pub cmd: Vec<String>,
    /// Where it runs. Unset: a host picked by `role` (build) or the host
    /// running the driver (install).
    pub host: Option<String>,
    pub role: Option<String>,
    #[serde(default)]
    pub heavy: bool,
    pub cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Intent {
    pub id: String,
    /// The machine being installed to; it must be ready before the install.
    pub target: String,
    pub os: Option<String>,
    /// The source revision this delivery is for, passed to both steps.
    pub rev: Option<String>,
    pub build: Option<Step>,
    pub install: Step,
    pub owner: String,
    pub created: u64,
    /// Give up waiting after this (ms since the epoch).
    pub expires: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Building,
    WaitingTarget,
    Installing,
    Done,
    Failed,
    /// A step ran and nobody knows how it ended. A person decides whether
    /// running it again is safe (`deliver resume --retry-install`).
    Unknown,
}

impl Phase {
    pub fn finished(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed | Phase::Unknown)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Progress {
    pub phase: Phase,
    pub build_host: Option<String>,
    pub artifact: Option<String>,
    /// Boot and machine the target answered with when the install started.
    pub target_boot: Option<String>,
    pub target_hostname: Option<String>,
    pub install_host: Option<String>,
    #[serde(default)]
    pub install_attempt: u32,
    pub detail: String,
    pub updated: u64,
}

impl Progress {
    pub fn build_job(&self, i: &Intent) -> String {
        format!("{}-build", i.id)
    }

    pub fn install_job(&self, i: &Intent) -> String {
        if self.install_attempt == 0 { format!("{}-install", i.id) } else { format!("{}-install-{}", i.id, self.install_attempt) }
    }
}

fn dir(id: &str) -> PathBuf {
    paths::state().join("deliveries").join(id)
}

pub fn create(intent: &Intent) -> Result<Progress> {
    if intent.install.cmd.is_empty() {
        bail!("an install command is required");
    }
    let d = dir(&intent.id);
    std::fs::create_dir_all(d.parent().context("state dir")?)?;
    match std::fs::create_dir(&d) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return load(&intent.id).map(|(_, p)| p),
        Err(e) => return Err(e.into()),
    }
    let p = Progress {
        phase: if intent.build.is_some() { Phase::Building } else { Phase::WaitingTarget },
        build_host: intent.build.as_ref().and_then(|b| b.host.clone()),
        artifact: None,
        target_boot: None,
        target_hostname: None,
        install_host: None,
        install_attempt: 0,
        detail: "recorded".into(),
        updated: paths::now_ms(),
    };
    paths::write_json(&d.join("intent.json"), intent)?;
    paths::write_json(&d.join("progress.json"), &p)?;
    Ok(p)
}

pub fn load(id: &str) -> Result<(Intent, Progress)> {
    let d = dir(id);
    let i = paths::read_json(&d.join("intent.json")).with_context(|| format!("no delivery {id}"))?;
    let p = paths::read_json(&d.join("progress.json")).with_context(|| format!("delivery {id} has no progress"))?;
    Ok((i, p))
}

pub fn save(id: &str, p: &Progress) -> Result<()> {
    paths::write_json(&dir(id).join("progress.json"), p)
}

pub fn list() -> Vec<(Intent, Progress)> {
    let Ok(rd) = std::fs::read_dir(paths::state().join("deliveries")) else { return vec![] };
    let mut out: Vec<_> = rd.flatten().filter_map(|e| load(&e.file_name().to_string_lossy()).ok()).collect();
    out.sort_by_key(|(i, _)| std::cmp::Reverse(i.created));
    out
}

/// Lets a person allow the install to run again after its outcome was lost.
pub fn retry_install(id: &str) -> Result<Progress> {
    let (_, mut p) = load(id)?;
    if p.phase != Phase::Unknown {
        bail!("delivery {id} is {:?}; only an unknown install can be retried", p.phase);
    }
    p.install_attempt += 1;
    p.phase = Phase::WaitingTarget;
    p.detail = "install retry allowed".into();
    p.updated = paths::now_ms();
    save(id, &p)?;
    Ok(p)
}

/// What the driver knows about a job. `None` from the ops means the host
/// could not be asked, which says nothing about the job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobSeen {
    Absent,
    Present { state: State, code: Option<i32> },
}

pub enum TargetSeen {
    Ready { boot_id: Option<String>, hostname: Option<String> },
    /// Waiting cannot fix it: the wrong machine, a rejected key.
    Blocked(String),
    NotYet(String),
    Cancelled,
}

/// Everything the driver does to the world, so it can be tested.
pub trait Ops {
    fn local_host(&self) -> String;
    fn pick(&self, role: &str, heavy: bool) -> Result<String, String>;
    fn job(&self, host: &str, id: &str) -> Option<JobSeen>;
    fn start(&self, host: &str, spec: &Spec) -> Option<JobSeen>;
    /// Blocks up to `limit` for the job to finish.
    fn wait_job(&self, host: &str, id: &str, limit: Duration) -> Option<JobSeen>;
    fn last_line(&self, host: &str, id: &str) -> Option<String>;
    /// Waits up to `limit` for the target. `fresh` forces a new probe.
    fn target(&self, target: &str, want: &Want, limit: Duration, fresh: bool) -> TargetSeen;
    /// False when cancelled during the pause.
    fn pause(&self, d: Duration) -> bool;
    fn cancelled(&self) -> bool;
    fn now(&self) -> u64;
}

pub struct Tuning {
    /// Longest single wait before progress is written again.
    pub slice: Duration,
    /// Pause after a host could not be asked.
    pub retry: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self { slice: Duration::from_secs(60), retry: Duration::from_secs(30) }
    }
}

/// Runs the delivery from its recorded phase until it finishes, expires or
/// is cancelled. Every transition is saved before the next step starts.
pub fn drive(intent: &Intent, mut p: Progress, ops: &dyn Ops, t: &Tuning) -> Result<Progress> {
    let set = |p: &mut Progress, phase: Phase, detail: String| -> Result<()> {
        p.phase = phase;
        p.detail = detail;
        p.updated = ops.now();
        save(&intent.id, p)
    };
    let note = |p: &mut Progress, detail: String| -> Result<()> {
        if p.detail != detail {
            p.detail = detail;
            p.updated = ops.now();
            save(&intent.id, p)?;
        }
        Ok(())
    };
    while !p.phase.finished() {
        if ops.cancelled() {
            break;
        }
        let left = Duration::from_millis(intent.expires.saturating_sub(ops.now()));
        if left.is_zero() {
            let what = match p.phase {
                Phase::Building => "the build to finish",
                Phase::WaitingTarget => "the target",
                _ => "the install to finish",
            };
            let why = format!("expired waiting for {what}; last: {}", p.detail);
            set(&mut p, Phase::Failed, why)?;
            break;
        }
        let slice = left.min(t.slice);
        match p.phase {
            Phase::Building => {
                let b = intent.build.as_ref().context("building without a build step")?;
                let host = match &p.build_host {
                    Some(h) => h.clone(),
                    None => match ops.pick(b.role.as_deref().unwrap_or("builder"), b.heavy) {
                        Ok(h) => {
                            p.build_host = Some(h.clone());
                            note(&mut p, format!("building on {h}"))?;
                            h
                        }
                        Err(why) => {
                            note(&mut p, format!("no builder yet: {why}"))?;
                            if !ops.pause(t.retry.min(left)) {
                                return Ok(p);
                            }
                            continue;
                        }
                    },
                };
                let id = p.build_job(intent);
                let seen = match ops.job(&host, &id) {
                    Some(JobSeen::Absent) => {
                        let spec = Spec { id: id.clone(), cmd: b.cmd.clone(), cwd: b.cwd.clone(), owner: format!("deliver {}", intent.id), locks: vec![format!("deliver-build:{}", intent.id)], env: env(intent, &p), ..Default::default() };
                        ops.start(&host, &spec)
                    }
                    other => other,
                };
                match seen {
                    None => {
                        note(&mut p, format!("could not ask {host} about {id}; retrying"))?;
                        if !ops.pause(t.retry.min(left)) {
                            return Ok(p);
                        }
                    }
                    Some(JobSeen::Absent) => bail!("{host} accepted {id} and then had no record of it"),
                    Some(JobSeen::Present { state: State::Starting | State::Running, .. }) => {
                        note(&mut p, format!("building on {host} ({id})"))?;
                        let _ = ops.wait_job(&host, &id, slice);
                    }
                    Some(JobSeen::Present { state: State::Succeeded, .. }) => {
                        p.artifact = ops.last_line(&host, &id);
                        let why = format!("built on {host}: {}", p.artifact.as_deref().unwrap_or("(no output)"));
                        set(&mut p, Phase::WaitingTarget, why)?;
                    }
                    Some(JobSeen::Present { state: State::Lost, .. }) => set(&mut p, Phase::Unknown, format!("build {id} on {host} has no result; check its log before starting again"))?,
                    Some(JobSeen::Present { state, code }) => set(&mut p, Phase::Failed, format!("build {id} on {host} ended {state:?} (exit {code:?})").to_lowercase())?,
                }
            }
            Phase::WaitingTarget => {
                let want = Want { os: intent.os.clone(), ..Default::default() };
                // The first look returns at once, so status shows why it waits.
                let limit = if p.detail.starts_with("waiting for") { slice } else { Duration::ZERO };
                match ops.target(&intent.target, &want, limit, false) {
                    TargetSeen::Ready { .. } => set(&mut p, Phase::Installing, format!("{} is ready", intent.target))?,
                    TargetSeen::Blocked(why) => set(&mut p, Phase::Failed, format!("{} cannot take the install: {why}", intent.target))?,
                    TargetSeen::NotYet(why) => note(&mut p, format!("waiting for {}: {why}", intent.target))?,
                    TargetSeen::Cancelled => return Ok(p),
                }
            }
            Phase::Installing => {
                let host = p.install_host.clone().or_else(|| intent.install.host.clone()).unwrap_or_else(|| ops.local_host());
                let id = p.install_job(intent);
                let seen = match ops.job(&host, &id) {
                    Some(JobSeen::Absent) => {
                        // The target may have gone again, or booted another
                        // OS, since the wait ended: look once more, fresh.
                        let want = Want { os: intent.os.clone(), ..Default::default() };
                        match ops.target(&intent.target, &want, Duration::ZERO, true) {
                            TargetSeen::Ready { boot_id, hostname } => {
                                p.target_boot = boot_id;
                                p.target_hostname = hostname;
                                p.install_host = Some(host.clone());
                            }
                            TargetSeen::Blocked(why) => {
                                set(&mut p, Phase::Failed, format!("{} cannot take the install: {why}", intent.target))?;
                                continue;
                            }
                            TargetSeen::NotYet(why) => {
                                set(&mut p, Phase::WaitingTarget, format!("{} went away before the install: {why}", intent.target))?;
                                continue;
                            }
                            TargetSeen::Cancelled => return Ok(p),
                        }
                        let i = &intent.install;
                        let spec = Spec { id: id.clone(), cmd: i.cmd.clone(), cwd: i.cwd.clone(), owner: format!("deliver {}", intent.id), locks: vec![format!("deliver-install:{}", intent.target)], env: env(intent, &p), ..Default::default() };
                        save(&intent.id, &p)?;
                        ops.start(&host, &spec)
                    }
                    other => other,
                };
                match seen {
                    None => {
                        note(&mut p, format!("could not ask {host} about {id}; retrying"))?;
                        if !ops.pause(t.retry.min(left)) {
                            return Ok(p);
                        }
                    }
                    Some(JobSeen::Absent) => bail!("{host} accepted {id} and then had no record of it"),
                    Some(JobSeen::Present { state: State::Starting | State::Running, .. }) => {
                        note(&mut p, format!("installing from {host} ({id})"))?;
                        let _ = ops.wait_job(&host, &id, slice);
                    }
                    Some(JobSeen::Present { state: State::Succeeded, .. }) => set(&mut p, Phase::Done, format!("installed on {} by {id}", intent.target))?,
                    Some(JobSeen::Present { state: State::Lost, .. }) => {
                        set(&mut p, Phase::Unknown, format!("install {id} on {host} has no result; check {} and its log, then `meshtop deliver resume {} --retry-install` if running it again is safe", intent.target, intent.id))?
                    }
                    Some(JobSeen::Present { state, code }) => set(&mut p, Phase::Failed, format!("install {id} on {host} ended {state:?} (exit {code:?})").to_lowercase())?,
                }
            }
            Phase::Done | Phase::Failed | Phase::Unknown => {}
        }
    }
    Ok(p)
}

fn env(i: &Intent, p: &Progress) -> BTreeMap<String, String> {
    let mut e = BTreeMap::new();
    e.insert("MESHTOP_DELIVERY".into(), i.id.clone());
    e.insert("MESHTOP_TARGET".into(), i.target.clone());
    for (k, v) in [("MESHTOP_REV", &i.rev), ("MESHTOP_ARTIFACT", &p.artifact), ("MESHTOP_BUILD_HOST", &p.build_host), ("MESHTOP_TARGET_BOOT", &p.target_boot)] {
        if let Some(v) = v {
            e.insert(k.into(), v.clone());
        }
    }
    e
}

/// Exit code for a delivery's phase, matching the job codes.
pub fn exit_code(p: Phase) -> i32 {
    match p {
        Phase::Done => 0,
        Phase::Failed => 1,
        Phase::Unknown => jobs::EXIT_LOST,
        _ => jobs::EXIT_STILL_RUNNING,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TEST_ENV as ENV;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct World {
        jobs: RefCell<BTreeMap<(String, String), (Spec, State)>>,
        /// What the target answers on each look, the last repeating.
        target: RefCell<Vec<&'static str>>,
        looks: Cell<usize>,
        transport_fail: Cell<u32>,
        /// What a started job ends as.
        ends: RefCell<BTreeMap<String, State>>,
        clock: Cell<u64>,
    }

    impl World {
        fn started(&self) -> Vec<String> {
            self.jobs.borrow().keys().map(|(h, id)| format!("{h}:{id}")).collect()
        }
    }

    impl Ops for World {
        fn local_host(&self) -> String {
            "macbook".into()
        }
        fn pick(&self, _: &str, heavy: bool) -> Result<String, String> {
            if heavy { Ok("g815".into()) } else { Err("none".into()) }
        }
        fn job(&self, host: &str, id: &str) -> Option<JobSeen> {
            if self.transport_fail.get() > 0 {
                self.transport_fail.set(self.transport_fail.get() - 1);
                return None;
            }
            Some(match self.jobs.borrow().get(&(host.into(), id.into())) {
                None => JobSeen::Absent,
                Some((_, s)) => JobSeen::Present { state: s.clone(), code: None },
            })
        }
        fn start(&self, host: &str, spec: &Spec) -> Option<JobSeen> {
            let end = self.ends.borrow().get(&spec.id).cloned().unwrap_or(State::Succeeded);
            self.jobs.borrow_mut().entry((host.into(), spec.id.clone())).or_insert((spec.clone(), end));
            self.job(host, &spec.id)
        }
        fn wait_job(&self, host: &str, id: &str, _: Duration) -> Option<JobSeen> {
            self.job(host, id)
        }
        fn last_line(&self, _: &str, _: &str) -> Option<String> {
            Some("/nix/store/abc-system".into())
        }
        fn target(&self, _: &str, _: &Want, _: Duration, _: bool) -> TargetSeen {
            let n = self.looks.get();
            self.looks.set(n + 1);
            self.clock.set(self.clock.get() + 1000);
            let t = self.target.borrow();
            match t[n.min(t.len() - 1)] {
                "ready" => TargetSeen::Ready { boot_id: Some(format!("boot{n}")), hostname: Some("e1504g".into()) },
                "wrong" => TargetSeen::Blocked("answered as g815, expected e1504g".into()),
                "cancel" => TargetSeen::Cancelled,
                why => TargetSeen::NotYet(why.into()),
            }
        }
        fn pause(&self, d: Duration) -> bool {
            self.clock.set(self.clock.get() + d.as_millis() as u64);
            true
        }
        fn cancelled(&self) -> bool {
            false
        }
        fn now(&self) -> u64 {
            self.clock.get()
        }
    }

    fn intent(id: &str) -> Intent {
        Intent {
            id: id.into(),
            target: "e1504g".into(),
            os: Some("linux".into()),
            rev: Some("abc123".into()),
            build: Some(Step { cmd: vec!["fspec".into(), "build".into()], role: Some("builder".into()), heavy: true, ..Default::default() }),
            install: Step { cmd: vec!["fspec".into(), "apply".into()], host: Some("g815".into()), ..Default::default() },
            owner: "test".into(),
            created: 0,
            expires: 1_000_000,
        }
    }

    fn with_state<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("meshtop-deliver-{}-{}", std::process::id(), paths::now_ms()));
        unsafe { std::env::set_var("MESHTOP_STATE", &dir) };
        let out = f();
        unsafe { std::env::remove_var("MESHTOP_STATE") };
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    const T: Tuning = Tuning { slice: Duration::from_secs(60), retry: Duration::from_secs(5) };

    #[test]
    fn offline_target_gets_the_build_when_it_comes_back() {
        with_state(|| {
            let w = World::default();
            *w.target.borrow_mut() = vec!["unreachable: tailscale reports e1504g offline", "running windows, waiting for linux", "ready", "ready"];
            let i = intent("trip");
            let p = drive(&i, create(&i).unwrap(), &w, &T).unwrap();
            assert_eq!(p.phase, Phase::Done, "{}", p.detail);
            assert_eq!(w.started(), ["g815:trip-build", "g815:trip-install"]);
            let jobs = w.jobs.borrow();
            let env = &jobs[&("g815".to_string(), "trip-install".to_string())].0.env;
            assert_eq!(env["MESHTOP_ARTIFACT"], "/nix/store/abc-system");
            assert_eq!(env["MESHTOP_REV"], "abc123");
            assert_eq!(env["MESHTOP_TARGET_BOOT"], "boot3");
            assert_eq!(load("trip").unwrap().1.phase, Phase::Done);
        })
    }

    #[test]
    fn a_lost_install_is_not_replayed_until_allowed() {
        with_state(|| {
            let w = World::default();
            *w.target.borrow_mut() = vec!["ready"];
            w.ends.borrow_mut().insert("lost-install".into(), State::Lost);
            let i = intent("lost");
            let p = drive(&i, create(&i).unwrap(), &w, &T).unwrap();
            assert_eq!(p.phase, Phase::Unknown);
            // Resuming without permission does nothing.
            let (i, p) = load("lost").unwrap();
            assert_eq!(drive(&i, p, &w, &T).unwrap().phase, Phase::Unknown);
            assert_eq!(w.jobs.borrow().len(), 2);
            let p = drive(&i, retry_install("lost").unwrap(), &w, &T).unwrap();
            assert_eq!(p.phase, Phase::Done);
            assert!(w.started().contains(&"g815:lost-install-1".to_string()));
        })
    }

    #[test]
    fn resume_joins_a_running_install_and_survives_transport_failures() {
        with_state(|| {
            let w = World::default();
            *w.target.borrow_mut() = vec!["ready"];
            let i = intent("again");
            let mut p = create(&i).unwrap();
            p.phase = Phase::Installing;
            p.install_host = Some("g815".into());
            save("again", &p).unwrap();
            w.jobs.borrow_mut().insert(("g815".into(), "again-install".into()), (Spec::default(), State::Succeeded));
            w.transport_fail.set(2);
            let p = drive(&i, p, &w, &T).unwrap();
            assert_eq!(p.phase, Phase::Done);
            assert_eq!(w.jobs.borrow().len(), 1, "no second install");
            assert_eq!(w.looks.get(), 0, "an install already started is not re-gated");
        })
    }

    #[test]
    fn wrong_machine_fails_and_cancel_or_expiry_stop_cleanly() {
        with_state(|| {
            let w = World::default();
            *w.target.borrow_mut() = vec!["wrong"];
            let i = intent("wrong");
            assert_eq!(drive(&i, create(&i).unwrap(), &w, &T).unwrap().phase, Phase::Failed);
            assert_eq!(w.started(), ["g815:wrong-build"]);

            let w = World::default();
            *w.target.borrow_mut() = vec!["unreachable", "cancel"];
            let i = intent("cancel");
            let p = drive(&i, create(&i).unwrap(), &w, &T).unwrap();
            assert_eq!(p.phase, Phase::WaitingTarget, "resumable after cancel");

            let w = World::default();
            *w.target.borrow_mut() = vec!["unreachable"];
            let mut i = intent("expire");
            i.expires = 10_000;
            let p = drive(&i, create(&i).unwrap(), &w, &T).unwrap();
            assert_eq!(p.phase, Phase::Failed);
            assert!(p.detail.starts_with("expired waiting for the target"), "{}", p.detail);
        })
    }

    #[test]
    fn target_gone_between_wait_and_install_goes_back_to_waiting() {
        with_state(|| {
            let w = World::default();
            *w.target.borrow_mut() = vec!["ready", "unreachable: timed out", "ready", "ready"];
            let i = intent("flap");
            let p = drive(&i, create(&i).unwrap(), &w, &T).unwrap();
            assert_eq!(p.phase, Phase::Done);
            assert_eq!(p.target_boot.as_deref(), Some("boot3"));
        })
    }
}
