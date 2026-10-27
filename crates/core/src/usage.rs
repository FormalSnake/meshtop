//! Plan usage for Claude Code and Codex accounts signed in on this machine.
//!
//! Claude: the same endpoint Claude Code's `/usage` reads, with the OAuth
//! token Claude Code stores. Codex: the rate limits Codex writes into its
//! own session logs after every turn, so no request is made for it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use jiff::Timestamp;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{Account, AccountKind};
use crate::source::Update;

const POLL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct Usage {
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    /// Something the reader should know, e.g. why there are no windows.
    pub note: Option<String>,
    /// When the numbers were taken; Codex only updates them on use.
    pub at: Timestamp,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub label: String,
    pub used: f64,
    pub length: Duration,
    pub resets: Option<Timestamp>,
}

/// The accounts to watch: the configured ones, or the default Claude and
/// Codex homes.
pub fn accounts(configured: &[Account]) -> Vec<Account> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    vec![
        Account { name: None, kind: AccountKind::Claude, dir: None },
        Account { name: None, kind: AccountKind::Codex, dir: None },
    ]
}

/// Reads every account once a minute. `None` means not signed in, which
/// hides the account until it is.
pub fn spawn(accounts: Vec<Account>, tx: Sender<Update>) {
    if accounts.is_empty() {
        return;
    }
    thread::spawn(move || {
        let http: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .user_agent("meshtop")
            .build()
            .into();
        loop {
            for (i, a) in accounts.iter().enumerate() {
                let dir = a.home();
                let reading = match a.kind {
                    AccountKind::Claude => claude(&http, &dir),
                    AccountKind::Codex => codex(&dir),
                };
                let reading = reading.map_err(|e| format!("{e:#}"));
                if tx.send(Update::Usage(i, reading.transpose().map(|r| r.map(Box::new)))).is_err() {
                    return;
                }
            }
            thread::sleep(POLL);
        }
    });
}

impl Account {
    pub fn home(&self) -> PathBuf {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        match &self.dir {
            Some(d) => match d.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(d),
            },
            None => match self.kind {
                AccountKind::Claude => home.join(".claude"),
                AccountKind::Codex => std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or(home.join(".codex")),
            },
        }
    }

    pub fn label(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        match &self.dir {
            Some(d) => d.trim_end_matches('/').rsplit('/').next().unwrap_or(d).trim_start_matches('.').to_string(),
            None => format!("{:?}", self.kind).to_lowercase(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeCreds {
    claude_ai_oauth: Option<ClaudeOauth>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeOauth {
    access_token: String,
    /// Unix milliseconds.
    expires_at: Option<i64>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    five_hour: Option<ClaudeWindow>,
    seven_day: Option<ClaudeWindow>,
}

#[derive(Deserialize)]
struct ClaudeWindow {
    utilization: f64,
    resets_at: Option<Timestamp>,
}

/// Claude Code keeps its token in the macOS keychain, under a name suffixed
/// with a hash of the config dir for any dir but `~/.claude`, and in
/// `.credentials.json` inside the config dir elsewhere.
fn claude_creds(dir: &Path) -> Result<Option<ClaudeOauth>> {
    let text = if cfg!(target_os = "macos") {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let service = keychain_service(dir, &home);
        let out = Command::new("security").args(["find-generic-password", "-s", &service, "-w"]).output()?;
        if !out.status.success() {
            return Ok(None);
        }
        String::from_utf8(out.stdout)?
    } else {
        match std::fs::read_to_string(dir.join(".credentials.json")) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    };
    let creds: ClaudeCreds = serde_json::from_str(&text).context("reading Claude credentials")?;
    Ok(creds.claude_ai_oauth)
}

fn keychain_service(dir: &Path, home: &Path) -> String {
    let service = "Claude Code-credentials";
    if dir == home.join(".claude") {
        return service.into();
    }
    let hash = Sha256::digest(dir.to_string_lossy().as_bytes());
    let hex: String = hash.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{service}-{hex}")
}

fn claude(http: &ureq::Agent, dir: &Path) -> Result<Option<Usage>> {
    let Some(oauth) = claude_creds(dir)? else { return Ok(None) };
    // An expired token means nobody signed in has used the account since;
    // Claude Code refreshes it on its next start.
    if oauth.expires_at.is_some_and(|ms| ms < Timestamp::now().as_millisecond()) {
        return Ok(None);
    }
    let usage: ClaudeUsage = match http
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", &format!("Bearer {}", oauth.access_token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(mut res) => res.body_mut().read_json()?,
        Err(ureq::Error::StatusCode(401)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let window = |label: &str, hours: u64, w: Option<ClaudeWindow>| {
        w.map(|w| Window {
            label: label.into(),
            used: w.utilization,
            length: Duration::from_secs(hours * 3600),
            resets: w.resets_at,
        })
    };
    let plan = oauth.subscription_type.map(|plan| {
        // `default_claude_max_20x` → `max 20x`
        match oauth.rate_limit_tier.as_deref().and_then(|t| t.rsplit('_').next()).filter(|t| t.ends_with('x')) {
            Some(tier) => format!("{plan} {tier}"),
            None => plan,
        }
    });
    Ok(Some(Usage {
        plan,
        windows: [window("5h", 5, usage.five_hour), window("week", 168, usage.seven_day)]
            .into_iter()
            .flatten()
            .collect(),
        note: None,
        at: Timestamp::now(),
    }))
}

#[derive(Deserialize)]
struct CodexAuth {
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: String,
}

#[derive(Deserialize)]
struct Jwt {
    exp: i64,
}

#[derive(Deserialize)]
struct CodexLine {
    timestamp: Timestamp,
    payload: CodexPayload,
}

#[derive(Deserialize)]
struct CodexPayload {
    rate_limits: Option<CodexLimits>,
}

#[derive(Deserialize)]
struct CodexLimits {
    primary: Option<CodexWindow>,
    secondary: Option<CodexWindow>,
    plan_type: Option<String>,
    rate_limit_reached_type: Option<String>,
}

#[derive(Deserialize)]
struct CodexWindow {
    used_percent: f64,
    window_minutes: u64,
    /// Unix seconds.
    resets_at: Option<i64>,
}

/// Signed in while `auth.json` holds an access token that has not expired.
fn codex_signed_in(dir: &Path) -> Result<bool> {
    let text = match std::fs::read_to_string(dir.join("auth.json")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let auth: CodexAuth = serde_json::from_str(&text)?;
    let Some(tokens) = auth.tokens else { return Ok(false) };
    let Some(payload) = tokens.access_token.split('.').nth(1) else { return Ok(false) };
    let jwt: Jwt = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?)?;
    Ok(jwt.exp > Timestamp::now().as_second())
}

fn codex(dir: &Path) -> Result<Option<Usage>> {
    if !codex_signed_in(dir)? {
        return Ok(None);
    }
    let Some((at, limits)) = newest_codex_limits(&dir.join("sessions"))? else {
        return Ok(Some(Usage {
            plan: None,
            windows: Vec::new(),
            note: Some("no session yet".into()),
            at: Timestamp::now(),
        }));
    };
    let window = |w: Option<CodexWindow>| {
        w.map(|w| Window {
            label: match w.window_minutes {
                m if m % 1440 == 0 && m >= 10080 => "week".into(),
                m if m % 60 == 0 => format!("{}h", m / 60),
                m => format!("{m}m"),
            },
            used: w.used_percent,
            length: Duration::from_secs(w.window_minutes * 60),
            resets: w.resets_at.and_then(|s| Timestamp::from_second(s).ok()),
        })
    };
    let note = limits.rate_limit_reached_type.map(|r| r.replace('_', " "));
    Ok(Some(Usage {
        plan: limits.plan_type,
        windows: [window(limits.primary), window(limits.secondary)].into_iter().flatten().collect(),
        note,
        at,
    }))
}

/// The last rate limits in the newest session log that has any. Logs live
/// under `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, so name order is
/// time order.
fn newest_codex_limits(sessions: &Path) -> Result<Option<(Timestamp, CodexLimits)>> {
    fn sorted(dir: &Path) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> =
            std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
        entries.sort_by(|a, b| b.cmp(a));
        entries
    }
    let mut checked = 0;
    for year in sorted(sessions) {
        for month in sorted(&year) {
            for day in sorted(&month) {
                for file in sorted(&day).into_iter().filter(|f| f.extension().is_some_and(|e| e == "jsonl")) {
                    let text = std::fs::read_to_string(&file)?;
                    for line in text.lines().rev().filter(|l| l.contains("\"rate_limits\":{")) {
                        if let Ok(CodexLine { timestamp, payload: CodexPayload { rate_limits: Some(l) } }) =
                            serde_json::from_str(line)
                        {
                            return Ok(Some((timestamp, l)));
                        }
                    }
                    checked += 1;
                    if checked >= 20 {
                        return Ok(None);
                    }
                }
            }
        }
    }
    Ok(None)
}

impl Window {
    /// How far into the window we are, 0 to 1.
    pub fn elapsed(&self) -> Option<f64> {
        let left = self.resets?.duration_since(Timestamp::now()).as_secs_f64();
        Some((1.0 - left / self.length.as_secs_f64()).clamp(0.0, 1.0))
    }

    /// Seconds until the window resets, if it has not already.
    pub fn resets_in(&self) -> Option<u64> {
        let left = self.resets?.duration_since(Timestamp::now()).as_secs_f64();
        (left > 0.0).then_some(left as u64)
    }

    /// Usage as it stands now: a window that reset since the reading is empty.
    pub fn used_now(&self) -> f64 {
        if self.resets.is_some() && self.resets_in().is_none() { 0.0 } else { self.used }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_service_hashes_extra_config_dirs() {
        let home = Path::new("/Users/kyandesutter");
        assert_eq!(keychain_service(&home.join(".claude"), home), "Claude Code-credentials");
        assert_eq!(keychain_service(&home.join(".claude-pulse"), home), "Claude Code-credentials-a8bd4122");
    }
}
