//! Skill packages: install, update, list and remove skill directories from Git repositories or registries.
//!
//! Each package is stored once under `skill-packages/store/<integrity>/<skill>/…` (content-addressed) and recorded in
//! `skill-packages.lock.json` in the project (`.dial/`) or user scope. Discovery precedence is project Dial skills,
//! user Dial skills, installed packages, then foreign skill roots. Install scripts are never run.

pub mod git;
pub mod registry;
pub mod source;

use crate::config::paths::Layout;
use crate::storage::skills::Scope;
use crate::storage::skills::import::lock_root;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use source::Source;

/// Origin label shown for skills that came from an installed package.
pub const PACKAGE_ORIGIN: &str = "Dial package";
const LOCK_FILE: &str = "skill-packages.lock.json";
const LOCK_VERSION: u32 = 1;
const OWNERS_FILE: &str = "owners.json";
const NO_REGISTRIES: &str = "No skill registries are configured, so search has nothing to query. Add one under Settings → Skills → Skill registries (name = index URL), or install directly with /skills install <git-url>[@ref][#path]. Search only covers configured registries.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub scope: Scope,
    pub source: Source,
    pub commit: String,
    pub integrity: String,
    pub skills: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockFile {
    version: u32,
    #[serde(default)]
    packages: BTreeMap<String, LockEntry>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LockEntry {
    url: String,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    commit: String,
    integrity: String,
    skills: Vec<String>,
}

/// Both lock files, for discovery cache invalidation.
pub fn lock_paths(layout: &Layout) -> [PathBuf; 2] {
    [
        lock_path(layout, Scope::Project),
        lock_path(layout, Scope::User),
    ]
}

fn scope_dir(layout: &Layout, scope: Scope) -> &Path {
    match scope {
        Scope::Project => &layout.project_dir,
        Scope::User => &layout.user_dir,
    }
}

fn lock_path(layout: &Layout, scope: Scope) -> PathBuf {
    scope_dir(layout, scope).join(LOCK_FILE)
}

fn state_dir(layout: &Layout) -> PathBuf {
    layout.user_dir.join("skill-packages")
}

fn store_root(layout: &Layout) -> PathBuf {
    state_dir(layout).join("store")
}

/// One lock for every mutation (store and both lock files), so prune never races an install.
fn mutation_lock(layout: &Layout) -> Result<std::fs::File> {
    lock_root(&state_dir(layout), ".lock", "skill package change")
}

fn read_lock(path: &Path) -> Result<LockFile> {
    match fs::read(path) {
        Ok(bytes) => {
            let lock: LockFile = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "{} is not a valid skill package lock; fix or remove it",
                    path.display()
                )
            })?;
            ensure!(
                lock.version == LOCK_VERSION,
                "{} uses lock version {}; this Dial reads version {LOCK_VERSION}",
                path.display(),
                lock.version
            );
            Ok(lock)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LockFile {
            version: LOCK_VERSION,
            packages: BTreeMap::new(),
        }),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_lock(path: &Path, lock: &LockFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(lock)?;
    bytes.push(b'\n');
    crate::storage::memory::atomic_write(path, &bytes, 0o644)
}

/// A package integrity is the lowercase hex SHA-256 of its manifest (`git::integrity`).
fn valid_integrity(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Every installed package in both scopes, project first, then by name.
pub fn list_packages(layout: &Layout) -> Result<Vec<Package>> {
    let mut out = Vec::new();
    for scope in [Scope::Project, Scope::User] {
        let lock = read_lock(&lock_path(layout, scope))?;
        for (name, entry) in lock.packages {
            // A malformed digest cannot name a store entry; skip it rather than trusting the path.
            if !valid_integrity(&entry.integrity) {
                continue;
            }
            out.push(Package {
                name,
                scope,
                source: Source {
                    url: entry.url,
                    reference: entry.reference,
                    path: entry.path,
                },
                commit: entry.commit,
                integrity: entry.integrity,
                skills: entry.skills,
            });
        }
    }
    Ok(out)
}

/// Stored skill roots of installed packages, for discovery. Each root's children are skill directories.
/// Entries with a malformed digest or a store directory that is not a real directory inside the store
/// are skipped, so a lock cannot point discovery at external content.
pub fn roots(layout: &Layout) -> Vec<(Scope, PathBuf)> {
    list_packages(layout)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|package| {
            stored_dir(layout, &package.integrity).map(|dir| (package.scope, dir))
        })
        .collect()
}

/// The store directory for `integrity`, only when the digest is well formed and the directory is a
/// real directory (not a symlink) whose canonical path lies inside the canonical store.
fn stored_dir(layout: &Layout, integrity: &str) -> Option<PathBuf> {
    if !valid_integrity(integrity) {
        return None;
    }
    let store = store_root(layout);
    let dir = store.join(integrity);
    if !fs::symlink_metadata(&dir).ok()?.is_dir() {
        return None;
    }
    let canonical_store = fs::canonicalize(&store).ok()?;
    let canonical_dir = fs::canonicalize(&dir).ok()?;
    canonical_dir.starts_with(&canonical_store).then_some(dir)
}

/// Source resolved for an install, with the package name it will be recorded under.
#[derive(Debug)]
struct Plan {
    name: String,
    source: Source,
    label: String,
}

/// `<registry>/<skill>[@ref]` when the registry is configured; otherwise a Git URL, `url[@ref][#path]`.
fn plan(spec: &str, registries: &BTreeMap<String, String>, base: &Path) -> Result<Plan> {
    let spec = spec.trim();
    ensure!(
        !spec.is_empty(),
        "usage: /skills install <git-url>[@ref][#path] or <registry>/<skill>[@ref]"
    );
    if let Some((registry, rest)) = spec.split_once('/')
        && let Some(index_url) = registries.get(registry)
    {
        let (skill, reference) = match rest.rsplit_once('@') {
            Some((skill, reference)) => (skill, Some(reference)),
            None => (rest, None),
        };
        let entries = registry::fetch_index(index_url)
            .with_context(|| format!("reading registry {registry:?}"))?;
        let entry = registry::find(&entries, skill).ok_or_else(|| {
            anyhow!("registry {registry:?} has no skill {skill:?}; try /skills search {skill}")
        })?;
        let mut source = source::parse(&entry.url)?;
        if let Some(reference) = reference {
            ensure!(!reference.is_empty(), "empty ref after '@' in {spec:?}");
            source.reference = Some(reference.to_owned());
            source::parse(&source.spec())?;
        }
        return Ok(Plan {
            name: entry.name.clone(),
            source,
            label: format!("{registry}/{skill}"),
        });
    }
    let source = source::parse(spec)?;
    let local =
        source.url.contains("://") || source.url.contains(':') || base.join(&source.url).exists();
    ensure!(
        spec.contains(['/', ':']) && local,
        "{spec:?} is neither a Git URL nor a registry reference. Use <registry>/<skill>[@ref] with a registry configured under Settings → Skills, a Git URL such as https://host/owner/repo.git, or a local path such as ./repo"
    );
    let name = source::default_name(&source).ok_or_else(|| {
        anyhow!("cannot derive a package name from {spec:?}; end the #path with the package name")
    })?;
    Ok(Plan {
        name,
        source,
        label: spec.to_owned(),
    })
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

fn ensure_not_installed(layout: &Layout, name: &str) -> Result<()> {
    if let Some(existing) = list_packages(layout)?.into_iter().find(|p| p.name == name) {
        bail!(
            "skill package {name:?} is already installed in {} scope; remove it first or update it with /skills update {name}",
            existing.scope.as_str()
        );
    }
    Ok(())
}

fn ensure_no_skill_clash(layout: &Layout, package: &str, skills: &[String]) -> Result<()> {
    for other in list_packages(layout)? {
        if other.name == package {
            continue;
        }
        if let Some(skill) = skills.iter().find(|skill| other.skills.contains(skill)) {
            bail!(
                "skill {skill:?} is already provided by package {:?}; remove that package first",
                other.name
            );
        }
    }
    Ok(())
}

/// Removes its staging directory on drop unless it was renamed into the store.
struct Staging(Option<PathBuf>);

impl Drop for Staging {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

struct Stored {
    integrity: String,
    skills: Vec<String>,
    manifest: BTreeMap<String, String>,
}

/// Copies the selected skills into a new store entry. Caller holds the mutation lock.
fn store_checkout(layout: &Layout, checkout: &Path, path: Option<&str>) -> Result<Stored> {
    let store = store_root(layout);
    fs::create_dir_all(&store).with_context(|| format!("creating {}", store.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let staging_path = store.join(format!(".staging-{}-{nanos}", std::process::id()));
    fs::create_dir(&staging_path)?;
    let mut staging = Staging(Some(staging_path.clone()));
    let skills = git::copy_skills(checkout, path, &staging_path)?;
    let manifest = git::manifest(&staging_path)?;
    let integrity = git::integrity(&manifest);
    let target = store.join(&integrity);
    if !target.exists() {
        fs::rename(&staging_path, &target)
            .with_context(|| format!("storing package in {}", target.display()))?;
        staging.0 = None;
    }
    Ok(Stored {
        integrity,
        skills,
        manifest,
    })
}

/// Project lock files that have recorded packages in the shared user store. Every project uses the
/// same store, so pruning must consult each of them and not only the current project's lock.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnersFile {
    version: u32,
    #[serde(default)]
    project_locks: BTreeSet<PathBuf>,
}

fn owners_path(layout: &Layout) -> PathBuf {
    state_dir(layout).join(OWNERS_FILE)
}

fn read_owners(layout: &Layout) -> Result<BTreeSet<PathBuf>> {
    let path = owners_path(layout);
    match fs::read(&path) {
        Ok(bytes) => {
            let owners: OwnersFile = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "{} is not a valid skill package owner index; fix or remove it",
                    path.display()
                )
            })?;
            ensure!(
                owners.version == LOCK_VERSION,
                "{} uses owner index version {}; this Dial reads version {LOCK_VERSION}",
                path.display(),
                owners.version
            );
            Ok(owners.project_locks)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Every lock that can own store content: this project's, the user's, and each project recorded earlier.
fn owner_locks(layout: &Layout) -> Result<BTreeSet<PathBuf>> {
    let mut locks = read_owners(layout)?;
    locks.insert(lock_path(layout, Scope::Project));
    locks.insert(lock_path(layout, Scope::User));
    Ok(locks)
}

/// Writes a lock file. A project lock is first recorded as a store owner, so other projects' pruning
/// keeps its content. Caller holds the mutation lock.
fn save_lock(layout: &Layout, scope: Scope, lock: &LockFile) -> Result<()> {
    if matches!(scope, Scope::Project) {
        let mut owners = read_owners(layout)?;
        if owners.insert(lock_path(layout, Scope::Project)) {
            fs::create_dir_all(state_dir(layout))?;
            let index = OwnersFile {
                version: LOCK_VERSION,
                project_locks: owners,
            };
            let mut bytes = serde_json::to_vec_pretty(&index)?;
            bytes.push(b'\n');
            crate::storage::memory::atomic_write(&owners_path(layout), &bytes, 0o644)?;
        }
    }
    write_lock(&lock_path(layout, scope), lock)
}

/// Deletes store entries that no owner lock references. Caller holds the mutation lock.
fn prune_store(layout: &Layout) -> Result<()> {
    let mut live: BTreeSet<String> = BTreeSet::new();
    for path in owner_locks(layout)? {
        for package in read_lock(&path)?.packages.into_values() {
            live.insert(package.integrity);
        }
    }
    let store = store_root(layout);
    let entries = match fs::read_dir(&store) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || live.contains(&name) {
            continue;
        }
        fs::remove_dir_all(entry.path())
            .with_context(|| format!("removing unused package store {name}"))?;
    }
    Ok(())
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::SeqCst), "cancelled");
    Ok(())
}

/// Installs a package and returns a summary. `cancel` stops a running git command; `progress` gets status lines.
pub fn install(
    layout: &Layout,
    registries: &BTreeMap<String, String>,
    scope: Scope,
    spec: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    let plan = plan(spec, registries, &layout.project_root)?;
    ensure_not_installed(layout, &plan.name)?;
    let checkout = git::fetch(&plan.source, &layout.project_root, cancel, progress)?;
    check_cancel(cancel)?;
    progress("Copying skills");
    let _lock = mutation_lock(layout)?;
    ensure_not_installed(layout, &plan.name)?;
    let stored = store_checkout(layout, checkout.dir.path(), plan.source.path.as_deref())?;
    ensure_no_skill_clash(layout, &plan.name, &stored.skills)?;
    let path = lock_path(layout, scope);
    let mut lock = read_lock(&path)?;
    lock.packages.insert(
        plan.name.clone(),
        LockEntry {
            url: plan.source.url.clone(),
            reference: plan.source.reference.clone(),
            path: plan.source.path.clone(),
            commit: checkout.commit.clone(),
            integrity: stored.integrity.clone(),
            skills: stored.skills.clone(),
        },
    );
    save_lock(layout, scope, &lock)?;
    Ok(format!(
        "Installed {} ({}) at {} into {} scope.\nSkills: {}\nNo install scripts were run.",
        plan.name,
        plan.label,
        short(&checkout.commit),
        scope.as_str(),
        stored.skills.join(", ")
    ))
}

/// Describes what changed between two stored trees and two skill lists.
fn describe_change(
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
    old_skills: &[String],
    new_skills: &[String],
) -> String {
    let mut parts = Vec::new();
    let added_skills: Vec<&str> = new_skills
        .iter()
        .filter(|s| !old_skills.contains(s))
        .map(String::as_str)
        .collect();
    let removed_skills: Vec<&str> = old_skills
        .iter()
        .filter(|s| !new_skills.contains(s))
        .map(String::as_str)
        .collect();
    if !added_skills.is_empty() {
        parts.push(format!("new skills: {}", added_skills.join(", ")));
    }
    if !removed_skills.is_empty() {
        parts.push(format!("removed skills: {}", removed_skills.join(", ")));
    }
    let added = new.keys().filter(|k| !old.contains_key(*k)).count();
    let removed = old.keys().filter(|k| !new.contains_key(*k)).count();
    let changed = new
        .iter()
        .filter(|(k, v)| old.get(*k).is_some_and(|o| o != *v))
        .count();
    if added + removed + changed == 0 {
        parts.push("no file changes".into());
    } else {
        parts.push(format!(
            "files: {added} added, {changed} changed, {removed} removed"
        ));
    }
    parts.join("; ")
}

/// True when the locked entry is still the one `package` was read from.
fn same_entry(entry: &LockEntry, package: &Package) -> bool {
    entry.url == package.source.url
        && entry.reference == package.source.reference
        && entry.path == package.source.path
        && entry.commit == package.commit
        && entry.integrity == package.integrity
        && entry.skills == package.skills
}

fn update_one(
    layout: &Layout,
    package: &Package,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    // Fetch outside the lock; the entry is compared against the snapshot under the lock before publishing.
    let checkout = git::fetch(&package.source, &layout.project_root, cancel, progress)?;
    check_cancel(cancel)?;
    progress(&format!("Storing {}", package.name));
    let _lock = mutation_lock(layout)?;
    let path = lock_path(layout, package.scope);
    let mut lock = read_lock(&path)?;
    let current = lock
        .packages
        .get(&package.name)
        .ok_or_else(|| anyhow!("{} was removed while updating", package.name))?;
    ensure!(
        same_entry(current, package),
        "{} was changed or reinstalled while updating; run /skills update {} again",
        package.name,
        package.name
    );
    // Same commit with its content still stored is already current; otherwise store the checkout,
    // which also restores content that went missing.
    let old_dir = stored_dir(layout, &package.integrity);
    if checkout.commit == package.commit && old_dir.is_some() {
        return Ok(format!(
            "{}: already up to date at {}",
            package.name,
            short(&package.commit)
        ));
    }
    let old_manifest = match old_dir {
        Some(dir) => git::manifest(&dir)?,
        None => BTreeMap::new(),
    };
    let stored = store_checkout(layout, checkout.dir.path(), package.source.path.as_deref())?;
    ensure_no_skill_clash(layout, &package.name, &stored.skills)?;
    let entry = lock
        .packages
        .get_mut(&package.name)
        .ok_or_else(|| anyhow!("{} was removed while updating", package.name))?;
    entry.commit = checkout.commit.clone();
    entry.integrity = stored.integrity.clone();
    entry.skills = stored.skills.clone();
    save_lock(layout, package.scope, &lock)?;
    prune_store(layout)?;
    if checkout.commit == package.commit {
        return Ok(format!(
            "{}: restored missing content at {}",
            package.name,
            short(&package.commit)
        ));
    }
    Ok(format!(
        "{}: {} → {} ({})",
        package.name,
        short(&package.commit),
        short(&checkout.commit),
        describe_change(
            &old_manifest,
            &stored.manifest,
            &package.skills,
            &stored.skills
        )
    ))
}

/// Updates one package by name, or all packages. Each package follows its locked source and ref.
pub fn update(
    layout: &Layout,
    name: Option<&str>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    let packages = list_packages(layout)?;
    let targets: Vec<Package> = match name {
        Some(name) => {
            let found: Vec<Package> = packages.into_iter().filter(|p| p.name == name).collect();
            ensure!(
                !found.is_empty(),
                "no installed skill package named {name:?}; see /skills installed"
            );
            found
        }
        None => packages,
    };
    if targets.is_empty() {
        return Ok("No skill packages installed.".into());
    }
    let mut lines = Vec::new();
    let mut failed = false;
    for package in &targets {
        check_cancel(cancel)?;
        match update_one(layout, package, cancel, progress) {
            Ok(line) => lines.push(line),
            Err(error) => {
                failed = true;
                lines.push(format!("{}: failed: {error:#}", package.name));
            }
        }
    }
    let text = lines.join("\n");
    ensure!(!failed, "{text}");
    Ok(text)
}

/// Removes a package from whichever scope holds it, then deletes store entries nothing references.
pub fn remove(layout: &Layout, name: &str) -> Result<String> {
    let _lock = mutation_lock(layout)?;
    for scope in [Scope::Project, Scope::User] {
        let path = lock_path(layout, scope);
        let mut lock = read_lock(&path)?;
        if lock.packages.remove(name).is_some() {
            save_lock(layout, scope, &lock)?;
            prune_store(layout)?;
            return Ok(format!(
                "Removed {name} from {} scope. Its skills are no longer loaded.",
                scope.as_str()
            ));
        }
    }
    bail!("no installed skill package named {name:?}; see /skills installed")
}

/// Human-readable list of installed packages.
pub fn list_text(layout: &Layout) -> Result<String> {
    let packages = list_packages(layout)?;
    if packages.is_empty() {
        return Ok("No skill packages installed. Install one with /skills install <git-url>[@ref][#path] or <registry>/<skill>[@ref].".into());
    }
    let lines: Vec<String> = packages
        .iter()
        .map(|p| {
            format!(
                "{} [{} scope] {} at {} — skills: {}",
                p.name,
                p.scope.as_str(),
                p.source.spec(),
                short(&p.commit),
                p.skills.join(", ")
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

/// Searches every configured registry index. Each registry that fails is reported without stopping the rest.
pub fn search_text(registries: &BTreeMap<String, String>, query: &str) -> Result<String> {
    if registries.is_empty() {
        return Ok(NO_REGISTRIES.into());
    }
    let mut lines = Vec::new();
    for (name, url) in registries {
        ensure!(
            source::valid_package_name(name),
            "registry name {name:?} must use letters, digits, '-', '_' or '.'"
        );
        match registry::fetch_index(url) {
            Ok(entries) => {
                let hits = registry::search(&entries, query);
                if hits.is_empty() {
                    lines.push(format!("{name}: no matching skills"));
                }
                for entry in hits {
                    lines.push(format!(
                        "{name}/{} — {} [{}]",
                        entry.name, entry.description, entry.url
                    ));
                }
            }
            Err(error) => lines.push(format!(
                "{name}: could not read the registry index: {error:#}"
            )),
        }
    }
    lines.push("Install with /skills install <registry>/<skill>[@ref].".into());
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests;
