//! `meshtop deliver`: the CLI and the real-world side of `core::deliver`.
//! The driver runs as a local meshtop job, so it survives the agent that
//! started it and leaves a log, and its `deliver:<id>` lock keeps a second
//! resume from driving the same delivery twice.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use meshtop_core::deliver::{self, Intent, JobSeen, Ops, Progress, Step, TargetSeen, Tuning};
use meshtop_core::hosts::{self, HostsFile, Want};
use meshtop_core::jobs::{self, Spec};
use meshtop_core::paths;
use meshtop_core::registry::Registry;

use crate::manage::{ago, local_host, print_json, remote_call};

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum DeliverCmd {
    /// Record a build-then-install delivery and start its driver. The build
    /// runs as soon as a builder is free; the install waits for the target.
    Start {
        /// Stable id; starting it again reports the existing delivery.
        #[arg(long)]
        id: String,
        /// The host being installed to.
        #[arg(long)]
        target: String,
        /// The OS the target must be running.
        #[arg(long)]
        os: Option<String>,
        /// Source revision, passed to both steps as MESHTOP_REV.
        #[arg(long)]
        rev: Option<String>,
        /// Shell command that builds; its last output line is the artifact.
        #[arg(long)]
        build: Option<String>,
        #[arg(long)]
        build_host: Option<String>,
        #[arg(long, default_value = "builder")]
        build_role: String,
        /// Ask for a builder that can take heavy work.
        #[arg(long)]
        heavy: bool,
        #[arg(long)]
        build_cwd: Option<PathBuf>,
        /// Shell command that installs, given MESHTOP_ARTIFACT and friends.
        #[arg(long)]
        install: String,
        /// Where the install command runs (default: this host).
        #[arg(long)]
        install_host: Option<String>,
        #[arg(long)]
        install_cwd: Option<PathBuf>,
        /// Give up waiting after this many hours.
        #[arg(long, default_value_t = 72.0)]
        expires_hours: f64,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Exit 0 done, 1 failed, 74 unknown (a step's outcome was lost), 75
    /// still going.
    Status {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Start a driver again for a delivery whose driver stopped.
    Resume {
        id: String,
        /// Allow an install whose outcome was lost to run again.
        #[arg(long)]
        retry_install: bool,
    },
    /// Stop the driver. The delivery keeps its phase and can be resumed.
    Cancel { id: String },
    List {
        #[arg(long)]
        json: bool,
    },
    #[command(name = "__drive", hide = true)]
    Drive { id: String },
}

struct Real {
    file: HostsFile,
    here: String,
    exe: PathBuf,
    cancel: &'static AtomicBool,
}

impl Real {
    fn local(&self, host: &str) -> bool {
        host == self.here
    }

    fn remote(&self, host: &str, args: &[&str], stdin: Option<String>, limit: Duration) -> Option<(i32, String, String)> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let r = remote_call(host, &args, stdin, limit).ok()?;
        Some((r.code?, r.stdout, r.stderr))
    }

    fn seen_remote(&self, r: Option<(i32, String, String)>) -> Option<JobSeen> {
        let (_, out, err) = r?;
        match serde_json::from_str::<jobs::Status>(&out) {
            Ok(s) => Some(seen(&s)),
            Err(_) if err.contains("no job ") => Some(JobSeen::Absent),
            Err(_) => None,
        }
    }
}

fn seen(s: &jobs::Status) -> JobSeen {
    JobSeen::Present { state: s.state.clone(), code: s.outcome.as_ref().and_then(|o| o.code) }
}

fn local_seen(r: Result<jobs::Status>) -> Option<JobSeen> {
    match r {
        Ok(s) => Some(seen(&s)),
        Err(e) if e.to_string().starts_with("no job ") => Some(JobSeen::Absent),
        Err(_) => None,
    }
}

impl Ops for Real {
    fn local_host(&self) -> String {
        self.here.clone()
    }

    fn pick(&self, role: &str, heavy: bool) -> Result<String, String> {
        let st = hosts::status_all(&self.file, &[], Duration::from_secs(30), Duration::from_secs(8), false);
        hosts::pick(&st, role, heavy).map(|s| s.name.clone()).map_err(|why| why.iter().map(|(h, w)| format!("{h}: {w}")).collect::<Vec<_>>().join("; "))
    }

    fn job(&self, host: &str, id: &str) -> Option<JobSeen> {
        if self.local(host) {
            return local_seen(jobs::status(id));
        }
        self.seen_remote(self.remote(host, &["job", "status", id, "--json"], None, Duration::from_secs(20)))
    }

    fn start(&self, host: &str, spec: &Spec) -> Option<JobSeen> {
        if self.local(host) {
            return local_seen(jobs::start(spec.clone(), &self.exe));
        }
        let body = serde_json::to_string(spec).ok()?;
        self.seen_remote(self.remote(host, &["job", "start", "--spec", "--json"], Some(body), Duration::from_secs(30)))
    }

    fn wait_job(&self, host: &str, id: &str, limit: Duration) -> Option<JobSeen> {
        // In steps, so a cancel is noticed within a minute.
        let end = Instant::now() + limit;
        loop {
            let step = end.saturating_duration_since(Instant::now()).min(Duration::from_secs(60));
            let s = if self.local(host) {
                local_seen(jobs::wait(id, step))
            } else {
                let t = step.as_secs().max(1).to_string();
                self.seen_remote(self.remote(host, &["job", "wait", id, "--timeout", &t, "--json"], None, step + Duration::from_secs(30)))
            };
            let running = matches!(s, Some(JobSeen::Present { state: jobs::State::Starting | jobs::State::Running, .. }));
            if !running || Instant::now() >= end || self.cancelled() {
                return s;
            }
        }
    }

    fn last_line(&self, host: &str, id: &str) -> Option<String> {
        let log = if self.local(host) {
            jobs::tail(id, 20).ok()?
        } else {
            self.remote(host, &["job", "logs", id, "-n", "20"], None, Duration::from_secs(20))?.1
        };
        log.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(String::from)
    }

    fn target(&self, target: &str, want: &Want, limit: Duration, fresh: bool) -> TargetSeen {
        let Some(def) = self.file.hosts.get(target) else { return TargetSeen::Blocked(format!("{target} is not in hosts.toml")) };
        let interval = Duration::from_secs(30);
        let observe = |max_age: Duration| {
            let c = hosts::observe(target, def, max_age, Duration::from_secs(8)).unwrap_or_else(|_| hosts::cached(target));
            hosts::status_from(target, def, &c, max_age.max(interval), &hosts::holds())
        };
        let ready = |s: &hosts::HostStatus| TargetSeen::Ready { boot_id: s.boot_id.clone(), hostname: s.hostname.clone() };
        if fresh {
            let s = observe(Duration::ZERO);
            return match hosts::meets(&s, want) {
                Ok(()) => ready(&s),
                Err((true, why)) => TargetSeen::Blocked(why),
                Err((false, why)) => TargetSeen::NotYet(why),
            };
        }
        let last = std::cell::RefCell::new(None::<hosts::HostStatus>);
        let keep = |max_age| {
            let s = observe(max_age);
            *last.borrow_mut() = Some(s.clone());
            s
        };
        let wake = || last.borrow().as_ref().is_some_and(hosts::tailscale_wake);
        let w = hosts::Waiter { observe: &keep, wake: &wake, check: &|_| Ok(()), cancel: self.cancel };
        let r = hosts::wait(&w, want, limit, interval);
        match r.end {
            hosts::WaitEnd::Ready => ready(&r.status),
            hosts::WaitEnd::Blocked(why) => TargetSeen::Blocked(why),
            hosts::WaitEnd::Deadline(why) => TargetSeen::NotYet(why),
            hosts::WaitEnd::Cancelled(_) => TargetSeen::Cancelled,
        }
    }

    fn pause(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.cancelled() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        true
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn now(&self) -> u64 {
        paths::now_ms()
    }
}

fn driver_id(id: &str) -> String {
    format!("{id}-drive-{}", paths::now_ms())
}

/// The driver job for this delivery that is still running. Found by id
/// prefix rather than recorded, so nothing races the driver's own saves.
pub(crate) fn running_driver(id: &str) -> Option<jobs::Status> {
    let prefix = format!("{id}-drive-");
    jobs::list().into_iter().find(|s| s.id.starts_with(&prefix) && !s.finished())
}

/// Starts a driver job unless one is already running for this delivery. The
/// `deliver:<id>` lock settles two concurrent resumes: the second is blocked.
pub(crate) fn spawn_driver(id: &str) -> Result<jobs::Status> {
    if let Some(s) = running_driver(id) {
        return Ok(s);
    }
    let exe = std::env::current_exe()?;
    let spec = Spec {
        id: driver_id(id),
        cmd: vec![exe.display().to_string(), "deliver".into(), "__drive".into(), id.into()],
        owner: format!("deliver {id}"),
        locks: vec![format!("deliver:{id}")],
        ..Default::default()
    };
    jobs::start(spec, &exe)
}

fn print(i: &Intent, p: &Progress) {
    println!("{} {:?} -> {} ({} ago)", i.id, p.phase, i.target, ago(p.updated));
    println!("  {}", p.detail);
    if let Some(a) = &p.artifact {
        println!("  artifact {a} from {}", p.build_host.as_deref().unwrap_or("?"));
    }
    if let Some(b) = &p.target_boot {
        println!("  installed against boot {b} of {}", p.target_hostname.as_deref().unwrap_or("?"));
    }
    match running_driver(&i.id) {
        Some(s) => println!("  driver {} running", s.id),
        None if !p.phase.finished() => println!("  no driver running: meshtop deliver resume {}", i.id),
        None => {}
    }
}

pub(crate) fn view(i: &Intent, p: &Progress) -> serde_json::Value {
    let driver = running_driver(&i.id).map(|s| s.id);
    serde_json::json!({ "intent": i, "progress": p, "driver": driver })
}

pub fn deliver_cmd(cmd: DeliverCmd) -> Result<i32> {
    match cmd {
        DeliverCmd::Start { id, target, os, rev, build, build_host, build_role, heavy, build_cwd, install, install_host, install_cwd, expires_hours, owner, json } => {
            jobs::validate_id(&id)?;
            let reg = Registry::open(None)?;
            let file = hosts::load(&reg)?;
            for h in [Some(&target), build_host.as_ref(), install_host.as_ref()].into_iter().flatten() {
                if !file.hosts.contains_key(h) {
                    bail!("{h} is not in {}", reg.hosts_file().display());
                }
            }
            let sh = |c: String| vec!["sh".to_string(), "-c".into(), c];
            let now = paths::now_ms();
            let intent = Intent {
                id: id.clone(),
                target,
                os,
                rev,
                build: build.map(|c| Step { cmd: sh(c), host: build_host, role: Some(build_role), heavy, cwd: build_cwd }),
                install: Step { cmd: sh(install), host: install_host, role: None, heavy: false, cwd: install_cwd },
                owner: owner.unwrap_or_else(|| std::env::var("USER").unwrap_or_default()),
                created: now,
                expires: now + (expires_hours * 3_600_000.0) as u64,
            };
            let p = deliver::create(&intent)?;
            let (intent, _) = deliver::load(&id)?;
            if !p.phase.finished() {
                spawn_driver(&id)?;
            }
            if json { print_json(&view(&intent, &p))? } else { print(&intent, &p) }
            Ok(0)
        }
        DeliverCmd::Status { id, json } => {
            let (i, p) = deliver::load(&id)?;
            if json { print_json(&view(&i, &p))? } else { print(&i, &p) }
            Ok(deliver::exit_code(p.phase))
        }
        DeliverCmd::Resume { id, retry_install } => {
            let p = if retry_install { deliver::retry_install(&id)? } else { deliver::load(&id)?.1 };
            if p.phase.finished() {
                bail!("delivery {id} is {:?}: {}", p.phase, p.detail);
            }
            let s = spawn_driver(&id)?;
            println!("driver {} {:?}", s.id, s.state);
            Ok(0)
        }
        DeliverCmd::Cancel { id } => {
            deliver::load(&id)?;
            let d = running_driver(&id).with_context(|| format!("no driver is running for {id}"))?;
            let s = jobs::cancel(&d.id)?;
            println!("driver {} {:?}; resume with meshtop deliver resume {id}", d.id, s.state);
            Ok(0)
        }
        DeliverCmd::List { json } => {
            let all = deliver::list();
            if json {
                print_json(&all.iter().map(|(i, p)| view(i, p)).collect::<Vec<_>>())?;
            } else {
                for (i, p) in &all {
                    println!("{:<32} {:<14} {:<8} {:>4}  {}", i.id, format!("{:?}", p.phase).to_lowercase(), i.target, ago(p.updated), p.detail);
                }
            }
            Ok(0)
        }
        DeliverCmd::Drive { id } => {
            let reg = Registry::open(None)?;
            let file = hosts::load(&reg)?;
            let here = local_host(&reg);
            let ops = Real { file, here, exe: std::env::current_exe()?, cancel: hosts::cancel_on_signal() };
            let (i, p) = deliver::load(&id)?;
            let p = deliver::drive(&i, p, &ops, &Tuning::default())?;
            println!("{:?}: {}", p.phase, p.detail);
            Ok(if ops.cancelled() { 130 } else { deliver::exit_code(p.phase) })
        }
    }
}
