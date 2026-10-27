//! `meshtop serve`: the web UI. Every route calls the same core functions
//! as the CLI, against the same registry and state dir, so the two never
//! disagree about a host, a job or a worktree.

use std::io::Read;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use meshtop_core::registry::Registry;
use meshtop_core::{deliver, hosts, jobs, memory, paths, services, skills, worktrees};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::deliver::{running_driver, spawn_driver, view};
use crate::manage::{HOSTS_SCHEMA, commit_path, export_views, local_host, scan};

#[derive(Args)]
pub struct ServeArgs {
    /// Address to listen on. Anything but loopback requires the token.
    #[arg(long, default_value = "127.0.0.1:7314")]
    listen: SocketAddr,
    /// Print the access token and exit.
    #[arg(long)]
    token: bool,
    /// Another Host name to answer on loopback, for a reverse proxy such as
    /// Tailscale Serve whose own access control stands in for the token.
    #[arg(long = "allow-host", value_name = "HOST")]
    allow_hosts: Vec<String>,
}

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

fn token() -> Result<String> {
    let path = paths::state().join("serve-token");
    if let Ok(t) = std::fs::read_to_string(&path)
        && t.trim().len() >= 32 {
            return Ok(t.trim().to_string());
        }
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    let t: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    paths::write_atomic(&path, t.as_bytes())?;
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(t)
}

pub fn run(a: ServeArgs) -> Result<i32> {
    let token = token()?;
    if a.token {
        println!("{token}");
        return Ok(0);
    }
    let loopback = a.listen.ip().is_loopback();
    let server = Server::http(a.listen).map_err(|e| anyhow::anyhow!("listening on {}: {e}", a.listen))?;
    eprintln!("meshtop serve on http://{}{}", a.listen, if loopback { "" } else { " (token required: meshtop serve --token)" });
    // Jobs whose supervisor died get their final state here even when no
    // CLI call happens to look at them.
    std::thread::spawn(|| {
        loop {
            jobs::reconcile();
            std::thread::sleep(Duration::from_secs(60));
        }
    });
    let ctx = std::sync::Arc::new(Ctx { token, loopback, listen: a.listen, allow_hosts: a.allow_hosts });
    for req in server.incoming_requests() {
        let ctx = ctx.clone();
        std::thread::spawn(move || handle(&ctx, req));
    }
    Ok(0)
}

struct Ctx {
    token: String,
    loopback: bool,
    listen: SocketAddr,
    allow_hosts: Vec<String>,
}

fn header<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
}

fn reply(req: Request, status: u16, ctype: &str, body: Vec<u8>) {
    let mut r = Response::from_data(body).with_status_code(status);
    for (k, v) in [("Content-Type", ctype), ("Cache-Control", "no-store"), ("X-Content-Type-Options", "nosniff"), ("Referrer-Policy", "no-referrer")] {
        r.add_header(Header::from_bytes(k, v).expect("static header"));
    }
    let _ = req.respond(r);
}

fn reply_json(req: Request, status: u16, v: &Value) {
    reply(req, status, "application/json", serde_json::to_vec(v).unwrap_or_default());
}

/// Who may talk to this server. The Host check stops DNS rebinding; the
/// custom header on writes stops cross-site form posts, since a browser
/// will not send it cross-origin without a preflight we never answer.
fn allowed(ctx: &Ctx, req: &Request) -> Result<Option<String>, &'static str> {
    let host = header(req, "Host").unwrap_or("");
    let host_name = host.rsplit_once(':').map_or(host, |(h, _)| h).trim_matches(['[', ']']);
    let local_name = matches!(host_name, "localhost" | "127.0.0.1" | "::1") || ctx.allow_hosts.iter().any(|h| h.eq_ignore_ascii_case(host_name));
    if ctx.loopback && !local_name {
        return Err("unexpected Host header");
    }
    let query_token = req.url().split_once('?').and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("token="))).map(String::from);
    let cookie_token = header(req, "Cookie").and_then(|c| c.split(';').find_map(|kv| kv.trim().strip_prefix("meshtop=")).map(String::from));
    let bearer = header(req, "Authorization").and_then(|a| a.strip_prefix("Bearer ")).map(String::from);
    let presented = bearer.or(cookie_token).or(query_token.clone());
    if !ctx.loopback && presented.as_deref() != Some(ctx.token.as_str()) {
        return Err("token required");
    }
    if *req.method() != Method::Get && header(req, "X-Meshtop").is_none() {
        return Err("missing X-Meshtop header");
    }
    Ok(query_token)
}

fn handle(ctx: &Ctx, mut req: Request) {
    let set_cookie = match allowed(ctx, &req) {
        Err(why) => return reply_json(req, 403, &json!({ "error": why })),
        Ok(t) => t,
    };
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let path = path.to_string();
    let q = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(|v| v.replace("%2F", "/").replace('+', " "));
    let static_file = match path.as_str() {
        "/" => Some(("text/html; charset=utf-8", INDEX)),
        "/app.js" => Some(("text/javascript; charset=utf-8", APP_JS)),
        "/app.css" => Some(("text/css; charset=utf-8", APP_CSS)),
        _ => None,
    };
    if let Some((ctype, body)) = static_file {
        let mut r = Response::from_string(body);
        r.add_header(Header::from_bytes("Content-Type", ctype).expect("static header"));
        r.add_header(Header::from_bytes("Content-Security-Policy", "default-src 'self'; frame-ancestors 'none'").expect("static header"));
        if let Some(t) = set_cookie.filter(|t| *t == ctx.token) {
            let c = format!("meshtop={t}; HttpOnly; SameSite=Strict; Path=/");
            r.add_header(Header::from_bytes("Set-Cookie", c.as_bytes()).expect("cookie header"));
        }
        let _ = req.respond(r);
        return;
    }
    let mut body = String::new();
    if *req.method() == Method::Post {
        let _ = req.as_reader().take(1 << 20).read_to_string(&mut body);
    }
    let input: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let method = req.method().clone();
    match route(ctx, &method, &path, &q, &input) {
        Ok(v) => reply_json(req, 200, &v),
        Err(e) => reply_json(req, if e.to_string().starts_with("not found") { 404 } else { 400 }, &json!({ "error": format!("{e:#}") })),
    }
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(String::from)
}

fn route(ctx: &Ctx, method: &Method, path: &str, q: &dyn Fn(&str) -> Option<String>, input: &Value) -> Result<Value> {
    let reg = Registry::open(None)?;
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let num = |k: &str, d: u64| q(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    Ok(match (method, parts.as_slice()) {
        (Method::Get, ["api", "overview"]) => json!({
            "host": local_host(&reg),
            "registry": reg.root,
            "revision": reg.revision(),
            "state": paths::state(),
            "listen": ctx.listen.to_string(),
        }),
        (Method::Get, ["api", "hosts"]) => {
            let file = hosts::load(&reg)?;
            let cached = q("cached").is_some();
            let st = hosts::status_all(&file, &[], Duration::from_secs(num("max_age", 30)), Duration::from_secs(8), cached);
            json!({ "schema": HOSTS_SCHEMA, "at": paths::now_ms(), "hosts": st })
        }
        (Method::Post, ["api", "hosts", name, "hold"]) => {
            if !hosts::load(&reg)?.hosts.contains_key(*name) {
                bail!("not found: host {name}");
            }
            let reason = s(input, "reason").filter(|r| !r.trim().is_empty()).context("a hold needs a reason")?;
            hosts::hold(name, &reason, input.get("hours").and_then(Value::as_f64))?;
            json!({ "held": name })
        }
        (Method::Post, ["api", "hosts", name, "release"]) => json!({ "released": hosts::release(name)? }),
        (Method::Get, ["api", "skills"]) => {
            let mut targets = Vec::new();
            for t in &reg.manifest.targets {
                targets.push(skills::target_status(&reg, t)?);
            }
            json!({ "catalog": skills::catalog(&reg)?, "targets": targets, "generations": skills::generations() })
        }
        (Method::Get, ["api", "skills", name, "diff"]) => json!({ "diff": skills::diff(&reg, name, &q("target").unwrap_or_else(|| "claude".into()))? }),
        (Method::Post, ["api", "skills", "sync"]) => {
            let opts = skills::SyncOptions { dry_run: input.get("dry_run").and_then(Value::as_bool).unwrap_or(true), ..Default::default() };
            json!(skills::sync(&reg, s(input, "target").as_deref(), &opts)?)
        }
        (Method::Get, ["api", "memory"]) => {
            let today = memory::today();
            let all = memory::all(&reg);
            let stale: Vec<Value> = memory::stale(&all, &today, 90).into_iter().map(|(m, why)| json!({ "id": m.meta.id, "why": why })).collect();
            let list: Vec<Value> = all.iter().map(|m| json!({ "meta": m.meta, "body": m.body, "active": m.active(&today) })).collect();
            json!({ "memories": list, "stale": stale })
        }
        (Method::Post, ["api", "memory", id, "verify"]) => {
            let m = memory::verify(&reg, id)?;
            commit_path(&reg, &m.path, &format!("memory: verify {id}"))?;
            json!({ "verified": id })
        }
        (Method::Post, ["api", "memory", id, "forget"]) => {
            let reason = s(input, "reason").filter(|r| !r.trim().is_empty()).context("retiring a memory needs a reason")?;
            let m = memory::forget(&reg, id, &reason)?;
            commit_path(&reg, &m.path, &format!("memory: retire {id}"))?;
            export_views(&reg)?;
            json!({ "retired": id })
        }
        (Method::Get, ["api", "jobs"]) => json!({ "jobs": jobs::list() }),
        (Method::Get, ["api", "jobs", id, "logs"]) => json!({ "log": jobs::tail(id, num("lines", 200) as usize)? }),
        (Method::Post, ["api", "jobs", id, "cancel"]) => json!(jobs::cancel(id)?),
        (Method::Get, ["api", "deliveries"]) => json!({ "deliveries": deliver::list().iter().map(|(i, p)| view(i, p)).collect::<Vec<_>>() }),
        (Method::Post, ["api", "deliveries", id, "resume"]) => {
            let p = if input.get("retry_install").and_then(Value::as_bool).unwrap_or(false) { deliver::retry_install(id)? } else { deliver::load(id)?.1 };
            if p.phase.finished() {
                bail!("delivery {id} is {:?}: {}", p.phase, p.detail);
            }
            json!({ "driver": spawn_driver(id)?.id })
        }
        (Method::Post, ["api", "deliveries", id, "cancel"]) => {
            let d = running_driver(id).with_context(|| format!("no driver is running for {id}"))?;
            json!(jobs::cancel(&d.id)?)
        }
        (Method::Get, ["api", "services"]) => {
            let host = local_host(&reg);
            json!({ "host": host, "services": services::here(&reg, &host).iter().map(services::status).collect::<Vec<_>>() })
        }
        (Method::Get, ["api", "services", name, "logs"]) => json!({ "log": services::logs(&reg, &local_host(&reg), name, num("lines", 200) as usize)? }),
        (Method::Post, ["api", "services", name, "restart"]) => json!(services::restart(&reg, &local_host(&reg), name)?),
        (Method::Get, ["api", "worktrees"]) => {
            let (_, list) = scan(&q("repo"), num("min_idle_hours", 24));
            json!({ "worktrees": list.into_iter().filter(|w| !w.main).collect::<Vec<_>>() })
        }
        (Method::Post, ["api", "worktrees", "remove"]) => {
            let path = std::path::PathBuf::from(s(input, "path").context("path required")?);
            let (cx, list) = scan(&None, 24);
            let w = list.iter().find(|w| w.path == path && !w.main).with_context(|| format!("not found: worktree {}", path.display()))?;
            if w.verdict == worktrees::Verdict::Keep {
                bail!("kept: {}", w.reasons.join("; "));
            }
            worktrees::remove(w, &cx, false)?;
            json!({ "removed": path })
        }
        _ => bail!("not found: {method} {path}"),
    })
}
