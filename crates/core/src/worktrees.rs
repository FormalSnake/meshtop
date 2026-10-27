//! Git worktrees on this host: who made them, whether they still carry
//! work, and which ones can go. Removal never forces: a worktree with
//! changes, unintegrated commits, a live process, an owner that still
//! points at it, or ignored files that are not build output stays put.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;

/// Ignored paths that a build or install regenerates. Any other ignored
/// file (`.env`, a local config, a database) keeps the worktree.
const REGENERABLE: &[&str] = &[
    "target", "node_modules", ".direnv", "dist", "build", "out", ".next", ".turbo", ".expo", ".cache", "result",
    "DerivedData", ".build", ".swiftpm", "Pods", ".gradle", "coverage", ".svelte-kit", ".astro", "__pycache__",
    ".venv", ".pytest_cache", ".DS_Store", ".wrangler", "zig-cache", ".zig-cache", "zig-out", ".parcel-cache",
];

/// Written by `meshtop worktree add` next to git's own admin files.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Marker {
    pub owner: String,
    pub task: Option<String>,
    pub reason: String,
    pub created: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Owner {
    T3 { thread: String, title: String, archived: bool, settled: bool, updated: String },
    Meshtop(Marker),
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Clean, integrated, idle and unowned: `git worktree remove` is safe.
    Removable,
    /// The directory is gone; only git's bookkeeping is left.
    Prunable,
    Keep,
}

#[derive(Clone, Debug, Serialize)]
pub struct Worktree {
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub main: bool,
    pub exists: bool,
    pub locked: Option<String>,
    pub changed: usize,
    pub untracked: usize,
    /// Ignored files that are not build output, first few.
    pub precious: Vec<String>,
    /// The branch it is compared with, e.g. origin/main.
    pub base: Option<String>,
    /// Commits with no equivalent in `base`.
    pub unique: Option<usize>,
    /// Merging it into `base` changes nothing: merged, squashed or empty.
    pub integrated: Option<bool>,
    pub in_use: Vec<String>,
    pub owner: Option<Owner>,
    pub last_activity: u64,
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Main checkouts (a `.git` directory) under `roots`, `depth` levels deep.
pub fn repos(roots: &[PathBuf], depth: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if dir.join(".git").is_dir() {
            out.push(dir.to_path_buf());
            return;
        }
        if depth == 0 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && name != ".config" || name == "node_modules" || name == "target" {
                continue;
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&e.path(), depth - 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for r in roots {
        walk(r, depth, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

pub fn default_roots() -> Vec<PathBuf> {
    let home = paths::home();
    vec![home.join("Developer"), home.join(".config")]
}

struct Raw {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    locked: Option<String>,
    prunable: bool,
}

fn list_raw(repo: &Path) -> Vec<Raw> {
    let Some(text) = git(repo, &["worktree", "list", "--porcelain"]) else { return vec![] };
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut raw = Raw { path: PathBuf::new(), head: String::new(), branch: None, locked: None, prunable: false };
        for line in block.lines() {
            let (k, v) = line.split_once(' ').unwrap_or((line, ""));
            match k {
                "worktree" => raw.path = PathBuf::from(v),
                "HEAD" => raw.head = v.chars().take(12).collect(),
                "branch" => raw.branch = Some(v.trim_start_matches("refs/heads/").to_string()),
                "locked" => raw.locked = Some(if v.is_empty() { "locked".into() } else { v.into() }),
                "prunable" => raw.prunable = true,
                _ => {}
            }
        }
        if !raw.path.as_os_str().is_empty() {
            out.push(raw);
        }
    }
    out
}

fn base_of(repo: &Path) -> Option<String> {
    if let Some(r) = git(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        return Some(r);
    }
    ["origin/main", "origin/master", "main", "master"]
        .into_iter()
        .find(|b| git(repo, &["rev-parse", "--verify", "-q", b]).is_some())
        .map(String::from)
}

/// The T3 threads bound to a worktree, read from T3's own state. T3 keeps
/// the binding in the thread payload; nothing here writes to it.
pub fn t3_bindings() -> BTreeMap<PathBuf, Owner> {
    let db = paths::home().join(".t3/userdata/statev2.sqlite");
    let mut out = BTreeMap::new();
    if !db.is_file() {
        return out;
    }
    let q = "select json_extract(payload_json,'$.worktreePath') as path, thread_id, title, archived_at, \
             json_extract(payload_json,'$.settledAt') as settled, updated_at from orchestration_v2_projection_threads \
             where deleted_at is null and json_extract(payload_json,'$.worktreePath') is not null";
    let Ok(o) = Command::new("sqlite3").args(["-readonly", "-json"]).arg(&db).arg(q).output() else { return out };
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&o.stdout).unwrap_or_default();
    for r in rows {
        let s = |k: &str| r.get(k).and_then(|v| v.as_str()).map(String::from);
        let Some(path) = s("path") else { continue };
        out.insert(
            PathBuf::from(path),
            Owner::T3 {
                thread: s("thread_id").unwrap_or_default(),
                title: s("title").unwrap_or_default(),
                archived: s("archived_at").is_some(),
                settled: s("settled").is_some(),
                updated: s("updated_at").unwrap_or_default(),
            },
        );
    }
    out
}

/// Processes whose working directory sits inside a worktree.
fn cwds() -> Vec<(PathBuf, String)> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always));
    sys.processes()
        .iter()
        .filter_map(|(pid, p)| Some((p.cwd()?.to_path_buf(), format!("{} ({pid})", p.name().to_string_lossy()))))
        .collect()
}

fn gitdir(path: &Path) -> Option<PathBuf> {
    git(path, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

fn mtime_ms(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

fn regenerable(rel: &str) -> bool {
    rel.trim_end_matches('/').split('/').any(|seg| REGENERABLE.contains(&seg) || seg.ends_with(".tsbuildinfo") || seg.ends_with(".log"))
}

pub struct Scan {
    pub bindings: BTreeMap<PathBuf, Owner>,
    pub cwds: Vec<(PathBuf, String)>,
    /// A worktree touched more recently than this is kept as possibly in use.
    pub min_idle_ms: u64,
}

impl Scan {
    pub fn gather(min_idle_ms: u64) -> Self {
        Scan { bindings: t3_bindings(), cwds: cwds(), min_idle_ms }
    }
}

fn inspect(repo: &Path, base: Option<&str>, raw: Raw, main: bool, cx: &Scan) -> Worktree {
    let exists = raw.path.is_dir() && !raw.prunable;
    let mut w = Worktree {
        repo: repo.to_path_buf(),
        path: raw.path.clone(),
        branch: raw.branch,
        head: raw.head,
        main,
        exists,
        locked: raw.locked,
        changed: 0,
        untracked: 0,
        precious: vec![],
        base: base.map(String::from),
        unique: None,
        integrated: None,
        in_use: vec![],
        owner: None,
        last_activity: 0,
        verdict: Verdict::Keep,
        reasons: vec![],
    };
    w.owner = cx.bindings.get(&raw.path).cloned();
    if exists {
        if let Some(gd) = gitdir(&raw.path) {
            if w.owner.is_none() {
                w.owner = paths::read_json::<Marker>(&gd.join("meshtop.json")).map(Owner::Meshtop);
            }
            w.last_activity = ["index", "HEAD", "logs/HEAD"].iter().map(|f| mtime_ms(&gd.join(f))).max().unwrap_or(0);
        }
        if let Some(st) = git(&raw.path, &["status", "--porcelain", "--ignored", "--untracked-files=normal"]) {
            for line in st.lines() {
                match line.get(..2) {
                    Some("??") => w.untracked += 1,
                    Some("!!") => {
                        let rel = &line[3..];
                        if !regenerable(rel) && w.precious.len() < 5 {
                            w.precious.push(rel.to_string());
                        }
                    }
                    _ => w.changed += 1,
                }
            }
        } else {
            w.reasons.push("git status failed".into());
        }
        w.in_use = cx.cwds.iter().filter(|(c, _)| c.starts_with(&raw.path)).map(|(_, n)| n.clone()).take(5).collect();
        if let Some(b) = base {
            let head = w.head.clone();
            w.unique = git(repo, &["cherry", b, &head]).map(|o| o.lines().filter(|l| l.starts_with('+')).count());
            // Squash merges leave `cherry` reporting the original commits,
            // so also ask whether merging the branch would change base.
            let merged = git(repo, &["merge-base", "--is-ancestor", &head, b]).is_some();
            w.integrated = Some(
                merged
                    || w.unique == Some(0)
                    || match (git(repo, &["merge-tree", "--write-tree", b, &head]), git(repo, &["rev-parse", &format!("{b}^{{tree}}")])) {
                        (Some(t), Some(bt)) => t.lines().next() == Some(bt.as_str()),
                        _ => false,
                    },
            );
        }
    }
    w.verdict = verdict(&mut w, cx);
    w
}

fn verdict(w: &mut Worktree, cx: &Scan) -> Verdict {
    if w.main {
        w.reasons.push("main checkout".into());
        return Verdict::Keep;
    }
    if let Some(l) = &w.locked {
        w.reasons.push(format!("locked: {l}"));
    }
    if !w.exists {
        return if w.reasons.is_empty() { Verdict::Prunable } else { Verdict::Keep };
    }
    match &w.owner {
        Some(Owner::T3 { title, archived: false, .. }) => w.reasons.push(format!("bound to T3 thread \"{title}\"")),
        Some(Owner::Meshtop(m)) => w.reasons.push(format!("claimed by {} ({})", m.owner, m.task.as_deref().unwrap_or(&m.reason))),
        _ => {}
    }
    if w.changed > 0 {
        w.reasons.push(format!("{} changed files", w.changed));
    }
    if w.untracked > 0 {
        w.reasons.push(format!("{} untracked files", w.untracked));
    }
    if !w.precious.is_empty() {
        w.reasons.push(format!("ignored files that are not build output: {}", w.precious.join(", ")));
    }
    match (w.integrated, w.unique) {
        (Some(true), _) => {}
        (Some(false), Some(n)) => w.reasons.push(format!("{n} commits not in {}", w.base.as_deref().unwrap_or("base"))),
        (Some(false), None) => w.reasons.push("not integrated".into()),
        (None, _) => w.reasons.push("no base branch to compare with".into()),
    }
    if !w.in_use.is_empty() {
        w.reasons.push(format!("in use by {}", w.in_use.join(", ")));
    }
    let idle = paths::now_ms().saturating_sub(w.last_activity);
    if idle < cx.min_idle_ms {
        w.reasons.push(format!("touched {}h ago", idle / 3_600_000));
    }
    if w.reasons.is_empty() { Verdict::Removable } else { Verdict::Keep }
}

/// Every worktree of every repo under `roots`, main checkouts included.
pub fn inventory(roots: &[PathBuf], cx: &Scan) -> Vec<Worktree> {
    let mut jobs: Vec<(PathBuf, Option<String>, Raw, bool)> = Vec::new();
    for repo in repos(roots, 3) {
        let raws = list_raw(&repo);
        if raws.len() < 2 {
            continue;
        }
        let base = base_of(&repo);
        for (i, r) in raws.into_iter().enumerate() {
            jobs.push((repo.clone(), base.clone(), r, i == 0));
        }
    }
    let n = jobs.len();
    let jobs = Mutex::new(jobs.into_iter().map(Some).collect::<Vec<_>>());
    let next = AtomicUsize::new(0);
    let out = Mutex::new(Vec::with_capacity(n));
    std::thread::scope(|s| {
        for _ in 0..8.min(n.max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let Some((repo, base, raw, main)) = jobs.lock().unwrap()[i].take() else { continue };
                    let w = inspect(&repo, base.as_deref(), raw, main, cx);
                    out.lock().unwrap().push(w);
                }
            });
        }
    });
    let mut out = out.into_inner().unwrap();
    out.sort_by(|a, b| (&a.repo, !a.main, &a.path).cmp(&(&b.repo, !b.main, &b.path)));
    out
}

/// Removes one worktree if it is still Removable, re-checked right now.
/// `release_claim` is for the task named in its meshtop marker finishing
/// its own worktree. The branch stays; deleting it is a separate decision.
pub fn remove(w: &Worktree, cx: &Scan, release_claim: bool) -> Result<()> {
    let base = w.base.clone();
    let raw = list_raw(&w.repo).into_iter().find(|r| r.path == w.path).context("worktree no longer listed")?;
    let mut fresh = inspect(&w.repo, base.as_deref(), raw, false, cx);
    if release_claim && matches!(fresh.owner, Some(Owner::Meshtop(_))) {
        fresh.owner = None;
        fresh.reasons.retain(|r| r == "git status failed");
        fresh.verdict = verdict(&mut fresh, cx);
    }
    match fresh.verdict {
        Verdict::Removable => {
            let out = Command::new("git").arg("-C").arg(&w.repo).args(["worktree", "remove"]).arg(&w.path).output()?;
            if !out.status.success() {
                bail!("git worktree remove: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            // `<repo>-wt/<name>` layout: drop the parent once it is empty.
            if let Some(parent) = w.path.parent()
                && parent.file_name().is_some_and(|n| n.to_string_lossy().ends_with("-wt")) {
                    let _ = std::fs::remove_dir(parent);
                }
            Ok(())
        }
        Verdict::Prunable => {
            git(&w.repo, &["worktree", "prune"]).context("git worktree prune failed")?;
            Ok(())
        }
        Verdict::Keep => bail!("kept: {}", fresh.reasons.join("; ")),
    }
}

/// Creates `<repo>-wt/<name>` on a new branch and records why it exists.
pub fn add(repo: &Path, name: &str, branch: &str, base: Option<&str>, marker: Marker) -> Result<PathBuf> {
    if marker.reason.trim().is_empty() {
        bail!("a worktree needs a collision reason: who else is in the main checkout, or which builds would clash");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        bail!("worktree name {name:?} must be [A-Za-z0-9._-]");
    }
    let repo = repo.canonicalize().with_context(|| format!("{}", repo.display()))?;
    let top = git(&repo, &["rev-parse", "--show-toplevel"]).context("not a git checkout")?;
    let top = PathBuf::from(top);
    let parent = top.with_file_name(format!("{}-wt", top.file_name().unwrap().to_string_lossy()));
    let path = parent.join(name);
    std::fs::create_dir_all(&parent)?;
    let base = base.map(String::from).or_else(|| base_of(&top)).unwrap_or_else(|| "HEAD".into());
    let out = Command::new("git").arg("-C").arg(&top).args(["worktree", "add", "-b", branch]).arg(&path).arg(&base).output()?;
    if !out.status.success() {
        let _ = std::fs::remove_dir(&parent);
        bail!("git worktree add: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let gd = gitdir(&path).context("new worktree has no git dir")?;
    paths::write_json(&gd.join("meshtop.json"), &Marker { created: paths::now_ms(), ..marker })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// Fixture commits use the ambient git identity; builds without one
    /// (the nix sandbox) skip these tests instead of inventing one.
    fn setup(name: &str) -> Option<(PathBuf, PathBuf)> {
        if git(Path::new("."), &["config", "--get", "user.email"]).is_none() {
            eprintln!("skipped: no git identity");
            return None;
        }
        let root = std::env::temp_dir().join(format!("meshtop-wt-{}-{name}", std::process::id()));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let root = root.canonicalize().unwrap();
        let repo = root.join("repo");
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a"), "1\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\n.env\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-qm", "init"]);
        Some((root, repo))
    }

    fn cx() -> Scan {
        Scan { bindings: BTreeMap::new(), cwds: vec![], min_idle_ms: 0 }
    }

    fn find<'a>(all: &'a [Worktree], name: &str) -> &'a Worktree {
        all.iter().find(|w| w.path.ends_with(name)).unwrap()
    }

    #[test]
    fn squash_merged_clean_worktree_is_removable_and_work_is_kept() {
        let Some((root, repo)) = setup("squash") else { return };
        let wt = root.join("repo-wt");
        for n in ["squashed", "unmerged", "dirty", "secret"] {
            sh(&repo, &["worktree", "add", "-q", "-b", n, wt.join(n).to_str().unwrap()]);
        }
        // squashed: two commits, landed on main as one.
        let sq = wt.join("squashed");
        std::fs::write(sq.join("b"), "1\n").unwrap();
        sh(&sq, &["add", "b"]);
        sh(&sq, &["commit", "-qm", "b1"]);
        std::fs::write(sq.join("b"), "2\n").unwrap();
        sh(&sq, &["commit", "-qam", "b2"]);
        std::fs::write(repo.join("b"), "2\n").unwrap();
        sh(&repo, &["add", "b"]);
        sh(&repo, &["commit", "-qm", "squash b"]);
        // unmerged: a commit main does not have.
        let um = wt.join("unmerged");
        std::fs::write(um.join("c"), "1\n").unwrap();
        sh(&um, &["add", "c"]);
        sh(&um, &["commit", "-qm", "c"]);
        std::fs::write(wt.join("dirty/a"), "changed\n").unwrap();
        std::fs::create_dir_all(wt.join("secret/target")).unwrap();
        std::fs::write(wt.join("secret/target/x"), "").unwrap();
        std::fs::write(wt.join("secret/.env"), "TOKEN=1\n").unwrap();

        let all = inventory(&[root.clone()], &cx());
        assert_eq!(find(&all, "squashed").verdict, Verdict::Removable, "{:?}", find(&all, "squashed").reasons);
        assert_eq!(find(&all, "unmerged").verdict, Verdict::Keep);
        assert_eq!(find(&all, "dirty").verdict, Verdict::Keep);
        let secret = find(&all, "secret");
        assert_eq!(secret.verdict, Verdict::Keep);
        assert_eq!(secret.precious, vec![".env".to_string()]);

        remove(find(&all, "squashed"), &cx(), false).unwrap();
        assert!(!wt.join("squashed").exists());
        assert!(remove(find(&all, "unmerged"), &cx(), false).is_err());
        assert!(wt.join("unmerged/c").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn owned_or_busy_worktrees_are_kept() {
        let Some((root, repo)) = setup("owned") else { return };
        let path = add(&repo, "claimed", "claimed", Some("main"), Marker { owner: "agent".into(), reason: "peer in main".into(), ..Default::default() }).unwrap();
        assert!(path.ends_with("repo-wt/claimed"));
        sh(&repo, &["worktree", "add", "-q", "-b", "busy", root.join("repo-wt/busy").to_str().unwrap()]);
        let mut c = cx();
        c.cwds.push((root.join("repo-wt/busy"), "cargo (1)".into()));
        let all = inventory(&[root.clone()], &c);
        assert_eq!(find(&all, "claimed").verdict, Verdict::Keep);
        assert_eq!(find(&all, "busy").verdict, Verdict::Keep);
        assert!(add(&repo, "noreason", "noreason", None, Marker::default()).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
