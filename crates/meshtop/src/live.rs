//! Live readings for the web UI. It runs the TUI's own sources and `App`
//! state, so both draw from the same histories, liveness rule and topology.
//! Sampling starts on the first request and stops once no page has asked for
//! a while: an idle server holds no ssh sessions open to the other hosts.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use crate::app::{App, Status};
use crate::{config, source, usage};

const IDLE: Duration = Duration::from_secs(90);

struct Session {
    app: Mutex<App>,
    asked: Mutex<Instant>,
}

static LIVE: Mutex<Option<Arc<Session>>> = Mutex::new(None);

fn session() -> Result<Arc<Session>> {
    let mut slot = LIVE.lock().unwrap();
    if let Some(s) = slot.as_ref() {
        *s.asked.lock().unwrap() = Instant::now();
        return Ok(s.clone());
    }
    let config = config::load(None)?;
    let local = sysinfo::System::host_name().unwrap_or_default();
    let hosts = crate::resolve_hosts(&config, &local);
    let interval = Duration::from_secs_f64(config.interval.max(0.2));

    let (tx, rx) = mpsc::channel();
    let children = source::Children::default();
    for (i, h) in hosts.iter().enumerate() {
        if h.local {
            source::spawn_local(i, config.activity.clone(), interval, tx.clone());
        } else {
            source::spawn_remote(i, h.cfg.clone(), interval, tx.clone(), children.clone());
        }
    }
    let accounts = usage::accounts(&config.usage);
    usage::spawn(accounts.clone(), tx.clone());
    drop(tx);

    let s = Arc::new(Session { app: Mutex::new(App::new(config, hosts, accounts)), asked: Mutex::new(Instant::now()) });
    *slot = Some(s.clone());
    let pump = s.clone();
    thread::spawn(move || {
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(update) => pump.app.lock().unwrap().apply(update),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Checked under the slot lock so a request cannot pick up a
            // session that is about to stop.
            let mut slot = LIVE.lock().unwrap();
            if pump.asked.lock().unwrap().elapsed() > IDLE {
                *slot = None;
                break;
            }
        }
        // Sources exit on their next send once the receiver is gone.
        drop(rx);
        children.kill_all();
    });
    Ok(s)
}

/// Everything the live views draw. `detail` names the host whose processes
/// and connections to include; the rest leave them out to keep the poll small.
pub fn snapshot(detail: Option<&str>) -> Result<Value> {
    let s = session()?;
    let app = s.app.lock().unwrap();
    let interval = app.interval();
    let hosts: Vec<Value> = app
        .hosts
        .iter()
        .map(|h| {
            let (status, reason) = match &h.status {
                Status::Connecting => ("connecting", None),
                Status::Up => ("up", None),
                Status::Down(r) => ("down", Some(r.clone())),
            };
            let mut snap = h.snap.as_ref().map(|s| serde_json::to_value(s).unwrap_or(Value::Null)).unwrap_or(Value::Null);
            if detail != Some(h.cfg.name.as_str())
                && let Some(o) = snap.as_object_mut()
            {
                o.remove("procs");
                o.remove("conns");
            }
            json!({
                "name": h.cfg.name,
                "role": h.cfg.role,
                "local": h.local,
                "status": status,
                "reason": reason,
                "live": h.live(interval),
                "seen_ms": h.seen.map(|t| t.elapsed().as_millis() as u64),
                "history": { "cpu": h.cpu, "mem": h.mem, "rx": h.rx, "tx": h.tx, "gpu": h.gpu },
                "snap": snap,
            })
        })
        .collect();
    let edges: Vec<Value> = app
        .topo
        .edges()
        .into_iter()
        .map(|((client, server), kinds)| json!({ "client": client, "server": server, "kinds": kinds }))
        .collect();
    let links: Vec<Value> = app
        .topo
        .links
        .iter()
        .map(|l| json!({ "client": l.client, "server": l.server, "kind": l.kind, "port": l.port, "process": l.client_process }))
        .collect();
    let accounts: Vec<Value> = app
        .accounts
        .iter()
        .filter_map(|a| {
            let base = json!({ "label": a.account.label(), "kind": format!("{:?}", a.account.kind).to_lowercase(), "history": a.history });
            let mut v = base;
            match a.reading.as_ref()? {
                Err(e) => v["error"] = json!(e),
                Ok(u) => {
                    v["plan"] = json!(u.plan);
                    v["note"] = json!(u.note);
                    v["at"] = json!(u.at.as_millisecond());
                    v["windows"] = u
                        .windows
                        .iter()
                        .map(|w| {
                            json!({
                                "label": w.label,
                                "used": w.used_now(),
                                "length_s": w.length.as_secs(),
                                "resets_in": w.resets_in(),
                                "elapsed": w.elapsed(),
                            })
                        })
                        .collect();
                }
            }
            Some(v)
        })
        .collect();
    Ok(json!({
        "interval_ms": interval.as_millis() as u64,
        "hosts": hosts,
        "edges": edges,
        "links": links,
        "external": app.topo.external,
        "accounts": accounts,
    }))
}
