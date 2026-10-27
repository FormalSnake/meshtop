//! Skills: the registry checkout is the one editable source; discovery dirs
//! (`~/.claude/skills`, `~/.codex/skills`) get links or copies of it.
//!
//! meshtop only ever replaces or removes entries it installed itself, as
//! recorded in `$STATE/skills/<target>.json`. Anything else in a discovery
//! dir (app-owned folders, plugin installs, hand-made skills) is foreign and
//! left alone unless `adopt` names a registry skill of the same name, and even
//! then the old entry is moved to a backup outside discovery, never deleted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths;
use crate::registry::{InstallMode, Registry, Target};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub hash: String,
    pub files: usize,
    pub bytes: u64,
}

/// `name:` and `description:` from SKILL.md frontmatter. Folded (`>`) and
/// quoted values are flattened to one line.
pub fn frontmatter(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(rest) = text.strip_prefix("---") else { return out };
    let Some(end) = rest.find("\n---") else { return out };
    let mut key: Option<String> = None;
    for line in rest[..end].lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(k) = &key {
                let v = out.entry(k.clone()).or_insert_with(String::new);
                if !v.is_empty() {
                    v.push(' ');
                }
                v.push_str(line.trim());
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim();
            let v = if matches!(v, ">" | "|" | ">-" | "|-") { "" } else { v.trim_matches(|c| c == '"' || c == '\'') };
            out.insert(k.trim().to_string(), v.to_string());
            key = Some(k.trim().to_string());
        }
    }
    out
}

fn ignored(name: &str) -> bool {
    name == ".DS_Store" || name == ".git"
}

/// Content hash of a skill dir: relative paths and bytes, in sorted order,
/// following symlinks. Two trees with the same files hash the same.
pub fn tree_hash(dir: &Path) -> Result<(String, usize, u64)> {
    let mut files = Vec::new();
    collect(dir, Path::new(""), &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    let mut bytes = 0;
    for rel in &files {
        let data = std::fs::read(dir.join(rel)).with_context(|| format!("reading {}", dir.join(rel).display()))?;
        bytes += data.len() as u64;
        h.update(rel.to_string_lossy().as_bytes());
        h.update([0]);
        h.update((data.len() as u64).to_le_bytes());
        h.update(&data);
    }
    let hex: String = h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
    Ok((hex, files.len(), bytes))
}

fn collect(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(root.join(rel))? {
        let entry = entry?;
        let name = entry.file_name();
        if ignored(&name.to_string_lossy()) {
            continue;
        }
        let r = rel.join(&name);
        if std::fs::metadata(root.join(&r))?.is_dir() {
            collect(root, &r, out)?;
        } else {
            out.push(r);
        }
    }
    Ok(())
}

pub fn read_skill(dir: &Path) -> Result<Skill> {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let text = std::fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
    let fm = frontmatter(&text);
    let (hash, files, bytes) = tree_hash(dir)?;
    Ok(Skill { name, description: fm.get("description").cloned().unwrap_or_default(), path: dir.into(), hash, files, bytes })
}

fn skill_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("SKILL.md").is_file())
        .collect();
    out.sort();
    out
}

pub fn catalog(reg: &Registry) -> Result<Vec<Skill>> {
    skill_dirs(&reg.skills_dir()).iter().map(|d| read_skill(d)).collect()
}

/// Skill roots owned by other installers (plugins, `npx skills`), by name.
pub fn external(reg: &Registry, target: Option<&str>) -> BTreeMap<String, Vec<(String, PathBuf)>> {
    let mut out: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
    let applies = |e: &crate::registry::External| match target {
        Some(t) => e.targets.is_empty() || e.targets.iter().any(|x| x == t),
        None => true,
    };
    for ext in reg.manifest.external.iter().filter(|e| applies(e)) {
        for root in glob(&ext.glob) {
            for d in skill_dirs(&root) {
                let name = d.file_name().unwrap().to_string_lossy().into_owned();
                out.entry(name).or_default().push((ext.name.clone(), d));
            }
        }
    }
    out
}

/// `*` segments only, which is all the registry needs.
fn glob(pattern: &str) -> Vec<PathBuf> {
    let path = paths::expand(pattern);
    let mut acc = vec![PathBuf::from("/")];
    for comp in path.components().skip(1) {
        let c = comp.as_os_str().to_string_lossy();
        acc = if c == "*" {
            acc.iter()
                .flat_map(|p| std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect()
        } else {
            acc.into_iter().map(|p| p.join(&*c)).filter(|p| p.exists()).collect()
        };
    }
    acc
}

/// What meshtop installed into one target.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Installed {
    pub skills: BTreeMap<String, Entry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub mode: InstallMode,
    /// Registry hash at install time; for copies, also the copy's hash then.
    pub hash: String,
    pub installed: u64,
    pub revision: Option<String>,
}

fn installed_path(target: &str) -> PathBuf {
    paths::state().join("skills").join(format!("{target}.json"))
}

pub fn installed(target: &str) -> Installed {
    paths::read_json(&installed_path(target)).unwrap_or_default()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EntryState {
    /// Installed by meshtop and matching the registry.
    Current,
    /// Installed by meshtop, the registry has moved on.
    Stale,
    /// A meshtop copy someone edited in place; sync keeps it until forced.
    Edited,
    /// In the registry, not in the target.
    Missing,
    /// Present and not installed by meshtop; same name as a registry skill.
    Conflict,
    /// Present and not installed by meshtop, unrelated to the registry.
    Foreign,
    /// Installed by meshtop, since removed from the registry.
    Orphan,
    /// A backup or leftover (`*.before-hm`, `*.bak`) sitting in discovery,
    /// where tools load it as a second copy.
    Backup,
    /// Present and also shipped by a plugin the target loads.
    Duplicate,
    Excluded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetEntry {
    pub name: String,
    pub state: EntryState,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetStatus {
    pub target: String,
    pub dir: PathBuf,
    pub mode: InstallMode,
    pub entries: Vec<TargetEntry>,
}

pub fn is_backup_name(name: &str) -> bool {
    [".before-hm", ".bak", ".orig", ".old", "~"].iter().any(|s| name.ends_with(s))
        || name.contains(".backup")
        || name.contains(".meshtop-tmp")
}

fn link_target(path: &Path) -> Option<PathBuf> {
    std::fs::read_link(path).ok()
}

fn points_into(path: &Path, dir: &Path) -> bool {
    let a = std::fs::canonicalize(path).ok();
    let b = std::fs::canonicalize(dir).ok();
    matches!((a, b), (Some(a), Some(b)) if a == b)
}

pub fn target_status(reg: &Registry, t: &Target) -> Result<TargetStatus> {
    let dir = paths::expand(&t.dir);
    let skills: BTreeMap<String, Skill> = catalog(reg)?.into_iter().map(|s| (s.name.clone(), s)).collect();
    let mine = installed(&t.name);
    let external = external(reg, Some(&t.name));
    let mut present = BTreeSet::new();
    let mut entries = Vec::new();

    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !is_backup_name(&name) {
            continue;
        }
        present.insert(name.clone());
        let path = e.path();
        let reg_skill = skills.get(&name);
        let (state, detail) = if is_backup_name(&name) {
            (EntryState::Backup, None)
        } else if t.exclude.contains(&name) && !mine.skills.contains_key(&name) {
            (EntryState::Excluded, None)
        } else if let Some(entry) = mine.skills.get(&name) {
            match reg_skill {
                None => (EntryState::Orphan, None),
                Some(s) => match entry.mode {
                    InstallMode::Link if points_into(&path, &s.path) => (EntryState::Current, None),
                    InstallMode::Link => (EntryState::Stale, link_target(&path).map(|p| format!("links to {}", p.display()))),
                    InstallMode::Copy => {
                        let (h, _, _) = tree_hash(&path).unwrap_or_default();
                        if h != entry.hash {
                            (EntryState::Edited, Some(format!("copy hash {h}, installed {}", entry.hash)))
                        } else if h != s.hash {
                            (EntryState::Stale, None)
                        } else {
                            (EntryState::Current, None)
                        }
                    }
                },
            }
        } else if reg_skill.is_some() {
            let how = match link_target(&path) {
                Some(l) => format!("links to {}", l.display()),
                None => "unmanaged directory".into(),
            };
            (EntryState::Conflict, Some(how))
        } else if let Some(ext) = external.get(&name) {
            (EntryState::Duplicate, Some(format!("also in {}", ext[0].0)))
        } else {
            (EntryState::Foreign, None)
        };
        entries.push(TargetEntry { name, state, detail });
    }
    for name in skills.keys() {
        if !present.contains(name) {
            let state = if t.exclude.contains(name) { EntryState::Excluded } else { EntryState::Missing };
            if state == EntryState::Missing {
                entries.push(TargetEntry { name: name.clone(), state, detail: None });
            }
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(TargetStatus { target: t.name.clone(), dir, mode: t.mode, entries })
}

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    /// Replace foreign entries that share a registry skill's name.
    pub adopt: bool,
    /// Overwrite copies edited in place.
    pub force: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Change {
    pub target: String,
    pub name: String,
    pub action: String,
    pub backup: Option<PathBuf>,
}

/// One sync run's backups live together so `rollback` can put them back.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Generation {
    pub id: String,
    pub created: u64,
    pub revision: Option<String>,
    pub changes: Vec<Change>,
    /// The install records before this run, per target.
    pub previous: BTreeMap<String, Installed>,
}

pub fn backups_root() -> PathBuf {
    paths::state().join("skill-backups")
}

fn generation_id() -> String {
    format!("{}-{:03}", jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S"), paths::now_ms() % 1000)
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Moves `path` into the generation's backup dir. A rename where possible;
/// across filesystems, copy then remove.
fn backup(gen_dir: &Path, target: &str, path: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("no file name")?;
    let dest_dir = gen_dir.join(target);
    std::fs::create_dir_all(&dest_dir)?;
    let mut dest = dest_dir.join(name);
    let mut n = 1;
    while exists(&dest) {
        dest = dest_dir.join(format!("{}.{n}", name.to_string_lossy()));
        n += 1;
    }
    if std::fs::rename(path, &dest).is_err() {
        copy_tree(path, &dest)?;
        remove(path)?;
    }
    Ok(dest)
}

fn remove(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() { std::fs::remove_dir_all(path)? } else { std::fs::remove_file(path)? }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() && !from.is_dir() && !from.exists() {
        // A dangling link: keep it as a link.
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
        return Ok(());
    }
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            if ignored(&e.file_name().to_string_lossy()) {
                continue;
            }
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
    } else {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

fn install(t: &Target, dir: &Path, skill: &Skill) -> Result<()> {
    let dest = dir.join(&skill.name);
    match t.mode {
        InstallMode::Link => std::os::unix::fs::symlink(&skill.path, &dest)?,
        InstallMode::Copy => {
            // Copy beside it under a hidden name, then rename into place, so
            // the tool never sees half a skill.
            let tmp = dir.join(format!(".{}.meshtop-tmp", skill.name));
            if exists(&tmp) {
                remove(&tmp)?;
            }
            copy_tree(&skill.path, &tmp)?;
            std::fs::rename(&tmp, &dest)?;
        }
    }
    Ok(())
}

pub fn sync(reg: &Registry, only: Option<&str>, opts: &SyncOptions) -> Result<Generation> {
    let skills: BTreeMap<String, Skill> = catalog(reg)?.into_iter().map(|s| (s.name.clone(), s)).collect();
    let mut generation = Generation { id: generation_id(), created: paths::now_ms(), revision: reg.revision(), ..Default::default() };
    let gen_dir = backups_root().join(&generation.id);

    for t in &reg.manifest.targets {
        if only.is_some_and(|o| o != t.name) {
            continue;
        }
        let dir = paths::expand(&t.dir);
        if !dir.is_dir() {
            continue;
        }
        let status = target_status(reg, t)?;
        let mut mine = installed(&t.name);
        generation.previous.insert(t.name.clone(), mine.clone());

        for e in &status.entries {
            let path = dir.join(&e.name);
            let skill = skills.get(&e.name);
            let action = match (&e.state, skill) {
                (EntryState::Current | EntryState::Foreign | EntryState::Backup | EntryState::Duplicate | EntryState::Excluded, _) => continue,
                (EntryState::Edited, _) if !opts.force => {
                    generation.changes.push(Change { target: t.name.clone(), name: e.name.clone(), action: "kept edited copy (use --force)".into(), backup: None });
                    continue;
                }
                (EntryState::Conflict, _) if !opts.adopt => {
                    generation.changes.push(Change { target: t.name.clone(), name: e.name.clone(), action: "skipped foreign entry (use --adopt)".into(), backup: None });
                    continue;
                }
                (EntryState::Orphan, _) => "removed",
                (EntryState::Missing, Some(_)) => "installed",
                (EntryState::Conflict, Some(_)) => "adopted",
                (EntryState::Stale | EntryState::Edited, Some(_)) => "updated",
                _ => continue,
            };
            let mut change = Change { target: t.name.clone(), name: e.name.clone(), action: action.into(), backup: None };
            if opts.dry_run {
                generation.changes.push(change);
                continue;
            }
            if exists(&path) {
                // A link meshtop made carries no content worth keeping.
                let ours_link = t.mode == InstallMode::Link && mine.skills.contains_key(&e.name) && link_target(&path).is_some();
                if ours_link {
                    remove(&path)?;
                } else {
                    change.backup = Some(backup(&gen_dir, &t.name, &path)?);
                }
            }
            match skill {
                Some(s) if action != "removed" => {
                    install(t, &dir, s)?;
                    mine.skills.insert(
                        s.name.clone(),
                        Entry { mode: t.mode, hash: s.hash.clone(), installed: paths::now_ms(), revision: generation.revision.clone() },
                    );
                }
                _ => {
                    mine.skills.remove(&e.name);
                }
            }
            generation.changes.push(change);
        }
        if !opts.dry_run {
            paths::write_json(&installed_path(&t.name), &mine)?;
        }
    }
    if !opts.dry_run && generation.changes.iter().any(|c| c.backup.is_some() || matches!(c.action.as_str(), "installed" | "updated" | "adopted" | "removed")) {
        paths::write_json(&gen_dir.join("generation.json"), &generation)?;
    }
    Ok(generation)
}

/// Moves backup-named leftovers and plugin duplicates out of discovery.
pub fn doctor_fix(reg: &Registry, dry_run: bool) -> Result<Generation> {
    let mut generation = Generation { id: generation_id(), created: paths::now_ms(), revision: reg.revision(), ..Default::default() };
    let gen_dir = backups_root().join(&generation.id);
    for t in &reg.manifest.targets {
        let status = target_status(reg, t)?;
        generation.previous.insert(t.name.clone(), installed(&t.name));
        for e in status.entries.iter().filter(|e| matches!(e.state, EntryState::Backup | EntryState::Duplicate)) {
            let path = status.dir.join(&e.name);
            let backup = if dry_run { None } else { Some(backup(&gen_dir, &t.name, &path)?) };
            generation.changes.push(Change { target: t.name.clone(), name: e.name.clone(), action: "moved out of discovery".into(), backup });
        }
    }
    if !dry_run && !generation.changes.is_empty() {
        paths::write_json(&gen_dir.join("generation.json"), &generation)?;
    }
    Ok(generation)
}

pub fn generations() -> Vec<Generation> {
    let mut out: Vec<Generation> = std::fs::read_dir(backups_root())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| paths::read_json(&e.path().join("generation.json")))
        .collect();
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

/// Undoes one generation: entries it installed are moved aside (into a new
/// generation, so a rollback can itself be rolled back) and the entries it
/// backed up go back where they were.
pub fn rollback(reg: &Registry, id: Option<&str>) -> Result<Generation> {
    let all = generations();
    let g = match id {
        Some(id) => all.iter().find(|g| g.id == id).with_context(|| format!("no generation {id}"))?,
        None => all.first().context("no generations to roll back")?,
    };
    let mut undo = Generation { id: generation_id(), created: paths::now_ms(), revision: reg.revision(), ..Default::default() };
    let undo_dir = backups_root().join(&undo.id);
    for c in g.changes.iter().rev() {
        let Some(t) = reg.manifest.targets.iter().find(|t| t.name == c.target) else { continue };
        let path = paths::expand(&t.dir).join(&c.name);
        let installed_something = matches!(c.action.as_str(), "installed" | "updated" | "adopted");
        if installed_something && exists(&path) {
            let b = backup(&undo_dir, &t.name, &path)?;
            undo.changes.push(Change { target: t.name.clone(), name: c.name.clone(), action: "removed".into(), backup: Some(b) });
        }
        if let Some(b) = &c.backup {
            if exists(&path) {
                bail!("{} exists; not restoring {} over it", path.display(), b.display());
            }
            std::fs::rename(b, &path).or_else(|_| copy_tree(b, &path))?;
            undo.changes.push(Change { target: t.name.clone(), name: c.name.clone(), action: "restored".into(), backup: None });
        }
    }
    for (target, prev) in &g.previous {
        undo.previous.insert(target.clone(), installed(target));
        paths::write_json(&installed_path(target), prev)?;
    }
    paths::write_json(&undo_dir.join("generation.json"), &undo)?;
    Ok(undo)
}

/// `diff -ru` between the registry skill and a target's entry.
pub fn diff(reg: &Registry, name: &str, target: &str) -> Result<String> {
    let t = reg.manifest.targets.iter().find(|t| t.name == target).with_context(|| format!("no target {target}"))?;
    let a = reg.skills_dir().join(name);
    let b = paths::expand(&t.dir).join(name);
    let out = std::process::Command::new("diff").arg("-ru").arg("-x").arg(".DS_Store").arg(&a).arg(&b).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

/// Writes one file inside a registry skill, refusing paths that leave it.
pub fn write_file(reg: &Registry, name: &str, rel: &str, content: &str) -> Result<()> {
    if name.contains('/') || name.starts_with('.') || rel.split('/').any(|c| c == ".." || c.is_empty()) {
        bail!("invalid skill path {name}/{rel}");
    }
    let dir = reg.skills_dir().join(name);
    if !dir.join("SKILL.md").is_file() {
        bail!("no registry skill {name}");
    }
    paths::write_atomic(&dir.join(rel), content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Manifest;
    use crate::paths::TEST_ENV as ENV;

    struct Fixture {
        root: PathBuf,
        reg: Registry,
    }

    fn fixture(mode: InstallMode) -> Fixture {
        let root = std::env::temp_dir().join(format!("meshtop-skills-{}-{}", std::process::id(), paths::now_ms()));
        let skills = root.join("registry/skills");
        for name in ["alpha", "beta"] {
            std::fs::create_dir_all(skills.join(name)).unwrap();
            std::fs::write(skills.join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: >\n  does {name}\n---\nbody")).unwrap();
        }
        let target_dir = root.join("discovery");
        std::fs::create_dir_all(&target_dir).unwrap();
        unsafe { std::env::set_var("MESHTOP_STATE", root.join("state")) };
        let manifest = Manifest {
            targets: vec![Target { name: "t".into(), dir: target_dir.to_string_lossy().into(), mode, exclude: vec![] }],
            ..Default::default()
        };
        Fixture { reg: Registry { root: root.join("registry"), manifest }, root }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("MESHTOP_STATE") };
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn state_of(f: &Fixture, name: &str) -> EntryState {
        let st = target_status(&f.reg, &f.reg.manifest.targets[0]).unwrap();
        st.entries.into_iter().find(|e| e.name == name).map(|e| e.state).unwrap()
    }

    #[test]
    fn foreign_same_name_is_kept_until_adopted_then_backed_up() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let f = fixture(InstallMode::Copy);
        let disc = f.root.join("discovery");
        std::fs::create_dir_all(disc.join("alpha")).unwrap();
        std::fs::write(disc.join("alpha/SKILL.md"), "hand made").unwrap();
        std::fs::create_dir_all(disc.join(".system")).unwrap();

        sync(&f.reg, None, &SyncOptions::default()).unwrap();
        assert_eq!(std::fs::read_to_string(disc.join("alpha/SKILL.md")).unwrap(), "hand made");
        assert_eq!(state_of(&f, "beta"), EntryState::Current);

        let g = sync(&f.reg, None, &SyncOptions { adopt: true, ..Default::default() }).unwrap();
        let b = g.changes.iter().find(|c| c.name == "alpha").unwrap().backup.clone().unwrap();
        assert_eq!(std::fs::read_to_string(b.join("SKILL.md")).unwrap(), "hand made");
        assert_eq!(state_of(&f, "alpha"), EntryState::Current);
        assert!(disc.join(".system").is_dir(), "app-owned dirs stay");

        rollback(&f.reg, Some(&g.id)).unwrap();
        assert_eq!(std::fs::read_to_string(disc.join("alpha/SKILL.md")).unwrap(), "hand made");
        assert_eq!(state_of(&f, "alpha"), EntryState::Conflict);
    }

    #[test]
    fn edited_copy_survives_sync_without_force() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let f = fixture(InstallMode::Copy);
        sync(&f.reg, None, &SyncOptions::default()).unwrap();
        let disc = f.root.join("discovery");
        std::fs::write(disc.join("beta/SKILL.md"), "local fix").unwrap();
        std::fs::write(f.reg.skills_dir().join("beta/SKILL.md"), "---\nname: beta\n---\nnew").unwrap();
        assert_eq!(state_of(&f, "beta"), EntryState::Edited);
        sync(&f.reg, None, &SyncOptions::default()).unwrap();
        assert_eq!(std::fs::read_to_string(disc.join("beta/SKILL.md")).unwrap(), "local fix");
    }

    #[test]
    fn links_track_registry_edits_and_orphans_are_removed() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let f = fixture(InstallMode::Link);
        sync(&f.reg, None, &SyncOptions::default()).unwrap();
        let disc = f.root.join("discovery");
        std::fs::write(f.reg.skills_dir().join("alpha/SKILL.md"), "edited").unwrap();
        assert_eq!(std::fs::read_to_string(disc.join("alpha/SKILL.md")).unwrap(), "edited");
        std::fs::remove_dir_all(f.reg.skills_dir().join("beta")).unwrap();
        assert_eq!(state_of(&f, "beta"), EntryState::Orphan);
        sync(&f.reg, None, &SyncOptions::default()).unwrap();
        assert!(!exists(&disc.join("beta")));
    }

    #[test]
    fn backups_in_discovery_move_out() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let f = fixture(InstallMode::Link);
        let disc = f.root.join("discovery");
        std::fs::create_dir_all(disc.join("wrangler.before-hm")).unwrap();
        let g = doctor_fix(&f.reg, false).unwrap();
        assert!(!exists(&disc.join("wrangler.before-hm")));
        assert!(g.changes[0].backup.as_ref().unwrap().starts_with(backups_root()));
    }

    #[test]
    fn folded_description_is_flattened() {
        let fm = frontmatter("---\nname: x\ndescription: >\n  one\n  two\n---\n");
        assert_eq!(fm["description"], "one two");
    }
}
