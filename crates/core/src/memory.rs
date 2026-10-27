//! Selective shared memory: one markdown file per fact in the registry's
//! `memory/`, with scope, provenance and an invalidation rule. Agents read a
//! small generated index (global and this-host facts only) and search the
//! rest on demand, so nothing loads the whole store every turn.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::registry::Registry;

pub const KINDS: &[&str] = &["fact", "preference", "procedure", "reference"];

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Meta {
    pub id: String,
    /// `global`, `host:<name>`, `repo:<name>` or `tool:<claude|codex>`.
    pub scope: String,
    pub kind: String,
    /// Where the fact came from: a user statement, a commit, a thread.
    pub source: String,
    pub created: String,
    /// Last date someone confirmed it still holds.
    pub verified: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// The change that would make it wrong, e.g. "meshtop moves off ssh".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidate_when: Option<String>,
    /// `active` or `retired`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Memory {
    #[serde(flatten)]
    pub meta: Meta,
    pub body: String,
    pub path: PathBuf,
}

impl Memory {
    pub fn expired(&self, today: &str) -> bool {
        self.meta.expires.as_deref().is_some_and(|e| e < today)
    }

    pub fn active(&self, today: &str) -> bool {
        self.meta.status == "active" && !self.expired(today)
    }

    /// First line of the body, for one-line listings.
    pub fn summary(&self) -> &str {
        self.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
    }
}

pub fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

fn parse(path: &Path) -> Result<Memory> {
    let text = std::fs::read_to_string(path)?;
    let rest = text.strip_prefix("---\n").with_context(|| format!("{}: no frontmatter", path.display()))?;
    let end = rest.find("\n---").with_context(|| format!("{}: unterminated frontmatter", path.display()))?;
    let meta: Meta = toml_like(&rest[..end]).with_context(|| format!("{}: frontmatter", path.display()))?;
    let body = rest[end + 4..].trim_start_matches('\n').trim_end().to_string();
    Ok(Memory { meta, body, path: path.into() })
}

/// Frontmatter is `key: value` lines; values are plain strings.
fn toml_like(text: &str) -> Result<Meta> {
    let mut map = serde_json::Map::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (k, v) = line.split_once(':').with_context(|| format!("bad line {line:?}"))?;
        map.insert(k.trim().into(), serde_json::Value::String(v.trim().trim_matches('"').into()));
    }
    Ok(serde_json::from_value(serde_json::Value::Object(map))?)
}

fn render(m: &Meta, body: &str) -> String {
    let mut out = String::from("---\n");
    let mut line = |k: &str, v: &str| out.push_str(&format!("{k}: {v}\n"));
    line("id", &m.id);
    line("scope", &m.scope);
    line("kind", &m.kind);
    line("source", &m.source);
    line("created", &m.created);
    line("verified", &m.verified);
    if let Some(e) = &m.expires {
        line("expires", e);
    }
    if let Some(i) = &m.invalidate_when {
        line("invalidate_when", i);
    }
    line("status", &m.status);
    if let Some(r) = &m.retired_reason {
        line("retired_reason", r);
    }
    out.push_str("---\n");
    out.push_str(body.trim());
    out.push('\n');
    out
}

pub fn all(reg: &Registry) -> Vec<Memory> {
    let mut out: Vec<Memory> = std::fs::read_dir(reg.memory_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md") && p.file_name().is_some_and(|n| n != "README.md"))
        .filter_map(|p| parse(&p).ok())
        .collect();
    out.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
    out
}

pub fn get(reg: &Registry, id: &str) -> Result<Memory> {
    parse(&path_for(reg, id)?)
}

fn path_for(reg: &Registry, id: &str) -> Result<PathBuf> {
    let ok = !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!("memory id {id:?} must be kebab-case [a-z0-9-], at most 80 chars");
    }
    Ok(reg.memory_dir().join(format!("{id}.md")))
}

fn valid_scope(scope: &str) -> bool {
    scope == "global"
        || ["host:", "repo:", "tool:"].iter().any(|p| scope.strip_prefix(p).is_some_and(|rest| !rest.is_empty()))
}

pub struct New {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub source: String,
    pub expires: Option<String>,
    pub invalidate_when: Option<String>,
    pub body: String,
}

/// Adds a fact. An existing id is refused: update means `replace`, which
/// keeps the provenance honest.
pub fn add(reg: &Registry, n: New, replace: bool) -> Result<Memory> {
    if !valid_scope(&n.scope) {
        bail!("scope must be global, host:<name>, repo:<name> or tool:<name>");
    }
    if !KINDS.contains(&n.kind.as_str()) {
        bail!("kind must be one of {}", KINDS.join(", "));
    }
    if n.source.trim().is_empty() {
        bail!("a source is required: who said it, or which commit or thread shows it");
    }
    if n.body.trim().is_empty() {
        bail!("empty memory");
    }
    let path = path_for(reg, &n.id)?;
    let created = match parse(&path) {
        Ok(old) if !replace => bail!("memory {} exists ({}); pass --replace to supersede it", n.id, old.summary()),
        Ok(old) => old.meta.created,
        Err(_) => today(),
    };
    let meta = Meta {
        id: n.id,
        scope: n.scope,
        kind: n.kind,
        source: n.source,
        created,
        verified: today(),
        expires: n.expires,
        invalidate_when: n.invalidate_when,
        status: "active".into(),
        retired_reason: None,
    };
    paths::write_atomic(&path, render(&meta, &n.body).as_bytes())?;
    parse(&path)
}

pub fn verify(reg: &Registry, id: &str) -> Result<Memory> {
    let mut m = get(reg, id)?;
    m.meta.verified = today();
    paths::write_atomic(&m.path, render(&m.meta, &m.body).as_bytes())?;
    Ok(m)
}

/// Retires a fact. The file stays, marked retired, so the reason is on
/// record and git shows when it stopped being true.
pub fn forget(reg: &Registry, id: &str, reason: &str) -> Result<Memory> {
    let mut m = get(reg, id)?;
    m.meta.status = "retired".into();
    m.meta.retired_reason = Some(reason.into());
    paths::write_atomic(&m.path, render(&m.meta, &m.body).as_bytes())?;
    Ok(m)
}

/// Active memories matching every term, ranked by how often terms occur.
pub fn search<'a>(mems: &'a [Memory], terms: &[String], today: &str) -> Vec<&'a Memory> {
    let terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let mut hits: Vec<(usize, &Memory)> = mems
        .iter()
        .filter(|m| m.active(today))
        .filter_map(|m| {
            let hay = format!("{} {} {}", m.meta.id, m.meta.scope, m.body).to_lowercase();
            let counts: Vec<usize> = terms.iter().map(|t| hay.matches(t.as_str()).count()).collect();
            counts.iter().all(|c| *c > 0).then(|| (counts.iter().sum(), m))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.meta.id.cmp(&b.1.meta.id)));
    hits.into_iter().map(|(_, m)| m).collect()
}

/// Memories that belong in an always-loaded view for `host` and `tool`.
pub fn index<'a>(mems: &'a [Memory], host: &str, tool: &str, today: &str) -> Vec<&'a Memory> {
    let host_scope = format!("host:{host}");
    let tool_scope = format!("tool:{tool}");
    mems.iter()
        .filter(|m| m.active(today))
        .filter(|m| m.meta.scope == "global" || m.meta.scope == host_scope || m.meta.scope == tool_scope)
        .collect()
}

pub const BEGIN: &str = "<!-- meshtop memory: generated by `meshtop memory export`, edit the registry instead -->";
pub const END: &str = "<!-- /meshtop memory -->";

/// The generated block: bounded to `max_lines` entries, one line each, with
/// how to reach the rest.
pub fn render_view(mems: &[&Memory], all: usize, max_lines: usize) -> String {
    let mut out = String::new();
    out.push_str(BEGIN);
    out.push_str("\n## Shared memory\n\n");
    out.push_str(
        "Durable facts from the agent-config registry. Repo-scoped facts are not listed: \
         run `meshtop memory search <terms>` (or `meshtop memory list --scope repo:<name>`) \
         when starting in a repo. Add a fact only when it is confirmed, reusable and not \
         already in a repo's CLAUDE.md: `meshtop memory add --help`. A fact you find wrong: \
         `meshtop memory forget <id> --reason ...`.\n\n",
    );
    for m in mems.iter().take(max_lines) {
        let mut s = m.summary().to_string();
        if s.chars().count() > 220 {
            s = s.chars().take(217).collect::<String>() + "...";
        }
        let scope = if m.meta.scope == "global" { String::new() } else { format!("[{}] ", m.meta.scope) };
        out.push_str(&format!("- {scope}{s} (`{}`)\n", m.meta.id));
    }
    let hidden = all.saturating_sub(mems.len().min(max_lines));
    if hidden > 0 {
        out.push_str(&format!("\n{hidden} more active entries are searchable, not listed.\n"));
    }
    out.push_str(END);
    out.push('\n');
    out
}

/// Replaces the generated block in `existing`, or appends it, leaving the
/// rest of the file as it was.
pub fn splice(existing: &str, block: &str) -> String {
    if let (Some(a), Some(b)) = (existing.find(BEGIN), existing.find(END))
        && a < b {
            let after = &existing[b + END.len()..];
            return format!("{}{}{}", &existing[..a], block.trim_end(), after);
        }
    if existing.trim().is_empty() {
        return block.to_string();
    }
    format!("{}\n\n{}", existing.trim_end(), block)
}

#[derive(Debug, Serialize)]
pub struct Export {
    pub path: PathBuf,
    pub entries: usize,
    pub changed: bool,
}

pub fn export(reg: &Registry, host: &str) -> Result<Vec<Export>> {
    let mems = all(reg);
    let today = today();
    let active = mems.iter().filter(|m| m.active(&today)).count();
    let views = &reg.manifest.memory;
    let mut out = Vec::new();
    for (tool, path) in [("claude", &views.claude_view), ("codex", &views.codex_view)] {
        let Some(path) = path else { continue };
        let path = paths::expand(path);
        if !path.parent().is_some_and(|p| p.is_dir()) {
            continue;
        }
        let picked = index(&mems, host, tool, &today);
        let block = render_view(&picked, active, views.max_index_lines);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let next = splice(&existing, &block);
        let changed = next != existing;
        if changed {
            paths::write_atomic(&path, next.as_bytes())?;
        }
        out.push(Export { path, entries: picked.len().min(views.max_index_lines), changed });
    }
    Ok(out)
}

/// Stale entries worth a look: expired, or unverified for `days`.
pub fn stale<'a>(mems: &'a [Memory], today: &str, days: i64) -> Vec<(&'a Memory, String)> {
    let Ok(now) = today.parse::<jiff::civil::Date>() else { return vec![] };
    mems.iter()
        .filter(|m| m.meta.status == "active")
        .filter_map(|m| {
            if m.expired(today) {
                return Some((m, format!("expired {}", m.meta.expires.as_deref().unwrap_or(""))));
            }
            let v = m.meta.verified.parse::<jiff::civil::Date>().ok()?;
            let age = i64::from((now - v).get_days());
            (age > days).then(|| (m, format!("unverified for {age} days")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Manifest;

    fn mem(id: &str, scope: &str, body: &str) -> Memory {
        Memory {
            meta: Meta { id: id.into(), scope: scope.into(), kind: "fact".into(), status: "active".into(), ..Default::default() },
            body: body.into(),
            path: PathBuf::new(),
        }
    }

    #[test]
    fn view_is_bounded_and_skips_other_hosts_and_repos() {
        let mems: Vec<Memory> = (0..10)
            .map(|i| mem(&format!("g{i}"), "global", "fact"))
            .chain([mem("h", "host:g815", "g815 only"), mem("r", "repo:meshtop", "repo only")])
            .collect();
        let picked = index(&mems, "macbook", "claude", "2026-10-27");
        assert_eq!(picked.len(), 10);
        let view = render_view(&picked, 12, 3);
        assert_eq!(view.lines().filter(|l| l.starts_with("- ")).count(), 3);
        assert!(view.contains("9 more active entries"));
        assert!(!view.contains("repo only"));
    }

    #[test]
    fn splice_keeps_hand_written_content() {
        let file = format!("# Mine\n\nkeep me\n\n{BEGIN}\nold\n{END}\ntail\n");
        let next = splice(&file, &format!("{BEGIN}\nnew\n{END}\n"));
        assert!(next.contains("keep me") && next.contains("new") && next.contains("tail"));
        assert!(!next.contains("old"));
    }

    #[test]
    fn add_requires_provenance_and_round_trips() {
        let _g = crate::paths::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("meshtop-mem-{}", paths::now_ms()));
        std::fs::create_dir_all(root.join("memory")).unwrap();
        let reg = Registry { root: root.clone(), manifest: Manifest::default() };
        let new = |source: &str| New {
            id: "g815-builds".into(),
            scope: "global".into(),
            kind: "fact".into(),
            source: source.into(),
            expires: None,
            invalidate_when: Some("g815 stops being the builder".into()),
            body: "g815 is the Linux builder: compile there, not on e1504g.".into(),
        };
        assert!(add(&reg, new(""), false).is_err());
        let m = add(&reg, new("user, 2026-10-27"), false).unwrap();
        assert_eq!(get(&reg, "g815-builds").unwrap().meta, m.meta);
        assert!(add(&reg, new("again"), false).is_err());
        forget(&reg, "g815-builds", "test").unwrap();
        assert!(search(&all(&reg), &["builder".into()], &today()).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
