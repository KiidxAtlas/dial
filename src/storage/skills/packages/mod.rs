//! Skill packages: install, update, list and remove skill directories from Git repositories or registries.
//!
//! Each package is stored once under `skill-packages/store/<integrity>/<skill>/…` (content-addressed) and recorded in
//! `skill-packages.lock.json` in the project (`.dial/`) or user scope. Discovery precedence is project Dial skills,
//! user Dial skills, installed packages, then foreign skill roots. Install scripts are never run.

pub mod git;
pub mod registry;
pub mod source;

use crate::storage::paths::Layout;
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

/// Every installed package in both scopes, project first, then by name.
pub fn list_packages(layout: &Layout) -> Result<Vec<Package>> {
    let mut out = Vec::new();
    for scope in [Scope::Project, Scope::User] {
        let lock = read_lock(&lock_path(layout, scope))?;
        for (name, entry) in lock.packages {
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
pub fn roots(layout: &Layout) -> Vec<(Scope, PathBuf)> {
    list_packages(layout)
        .unwrap_or_default()
        .into_iter()
        .map(|package| (package.scope, store_root(layout).join(package.integrity)))
        .collect()
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

/// Deletes store entries no lock references. Caller holds the mutation lock.
fn prune_store(layout: &Layout) -> Result<()> {
    let live: BTreeSet<String> = list_packages(layout)?
        .into_iter()
        .map(|package| package.integrity)
        .collect();
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
    write_lock(&path, &lock)?;
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

fn update_one(
    layout: &Layout,
    package: &Package,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    let checkout = git::fetch(&package.source, &layout.project_root, cancel, progress)?;
    if checkout.commit == package.commit {
        return Ok(format!(
            "{}: already up to date at {}",
            package.name,
            short(&package.commit)
        ));
    }
    check_cancel(cancel)?;
    progress(&format!("Storing {}", package.name));
    let _lock = mutation_lock(layout)?;
    let old_dir = store_root(layout).join(&package.integrity);
    let old_manifest = if old_dir.is_dir() {
        git::manifest(&old_dir)?
    } else {
        BTreeMap::new()
    };
    let stored = store_checkout(layout, checkout.dir.path(), package.source.path.as_deref())?;
    ensure_no_skill_clash(layout, &package.name, &stored.skills)?;
    let path = lock_path(layout, package.scope);
    let mut lock = read_lock(&path)?;
    let entry = lock
        .packages
        .get_mut(&package.name)
        .ok_or_else(|| anyhow!("{} was removed while updating", package.name))?;
    entry.commit = checkout.commit.clone();
    entry.integrity = stored.integrity.clone();
    entry.skills = stored.skills.clone();
    write_lock(&path, &lock)?;
    prune_store(layout)?;
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
            write_lock(&path, &lock)?;
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
mod tests {
    use super::*;

    fn layout(root: &Path) -> Layout {
        Layout::with_user_dir(root, root.join("user")).unwrap()
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dial-packages-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(url: &str, skills: &[&str]) -> LockEntry {
        LockEntry {
            url: url.into(),
            reference: Some("main".into()),
            path: Some("skills".into()),
            commit: "a".repeat(40),
            integrity: "b".repeat(64),
            skills: skills.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn lock_round_trips_and_keeps_optional_fields_out_when_absent() {
        let root = scratch("lock");
        let path = root.join(LOCK_FILE);
        let mut lock = LockFile {
            version: LOCK_VERSION,
            packages: BTreeMap::new(),
        };
        lock.packages
            .insert("pdf".into(), entry("https://x/r.git", &["pdf"]));
        lock.packages.insert(
            "bare".into(),
            LockEntry {
                reference: None,
                path: None,
                ..entry("https://x/b.git", &["bare"])
            },
        );
        write_lock(&path, &lock).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"ref\": \"main\""));
        assert!(!text.contains("\"bare\"\n") || !text.contains("\"ref\": null"));
        let back = read_lock(&path).unwrap();
        assert!(back.packages["pdf"] == lock.packages["pdf"]);
        assert!(back.packages["bare"] == lock.packages["bare"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_lock_reads_as_empty_and_bad_versions_are_rejected() {
        let root = scratch("lockerr");
        let path = root.join(LOCK_FILE);
        assert!(read_lock(&path).unwrap().packages.is_empty());
        fs::write(&path, r#"{"version":9,"packages":{}}"#).unwrap();
        assert!(read_lock(&path).is_err());
        fs::write(&path, r#"{"version":1,"packages":{},"extra":true}"#).unwrap();
        assert!(read_lock(&path).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_uses_registry_only_when_configured_and_rejects_bare_words() {
        let root = scratch("plan");
        let index = root.join("index.json");
        fs::write(
            &index,
            r#"{"version":1,"skills":[{"name":"pdf","description":"PDF","url":"https://x/skills.git@v1#pdf"}]}"#,
        )
        .unwrap();
        let mut registries = BTreeMap::new();
        registries.insert("team".to_owned(), format!("file://{}", index.display()));

        let planned = plan("team/pdf@v2", &registries, &root).unwrap();
        assert_eq!(planned.name, "pdf");
        assert_eq!(planned.source.reference.as_deref(), Some("v2"));
        assert_eq!(planned.source.path.as_deref(), Some("pdf"));
        assert_eq!(planned.label, "team/pdf");

        let unknown = plan("team/missing", &registries, &root)
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("no skill"), "{unknown}");

        let none = BTreeMap::new();
        assert!(plan("pdf", &none, &root).is_err());
        assert!(plan("team/pdf", &none, &root).is_err());

        let git = plan("https://example.com/org/tools.git@main#pdf", &none, &root).unwrap();
        assert_eq!(git.name, "pdf");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn describe_change_reports_skills_and_files() {
        let old: BTreeMap<String, String> = [("a/SKILL.md", "1"), ("a/x", "1")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let new: BTreeMap<String, String> = [("a/SKILL.md", "2"), ("b/SKILL.md", "1")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let text = describe_change(&old, &new, &["a".to_owned()], &["b".to_owned()]);
        assert!(text.contains("new skills: b"), "{text}");
        assert!(text.contains("removed skills: a"), "{text}");
        assert!(
            text.contains("files: 1 added, 1 changed, 1 removed"),
            "{text}"
        );
        assert_eq!(
            describe_change(&old, &old, &["a".to_owned()], &["a".to_owned()]),
            "no file changes"
        );
    }

    #[test]
    fn prune_keeps_live_entries_and_staging() {
        let root = scratch("prune");
        let layout = layout(&root);
        let store = store_root(&layout);
        fs::create_dir_all(store.join("live")).unwrap();
        fs::create_dir_all(store.join("dead")).unwrap();
        fs::create_dir_all(store.join(".staging-1")).unwrap();
        let mut lock = LockFile {
            version: LOCK_VERSION,
            packages: BTreeMap::new(),
        };
        lock.packages.insert(
            "pdf".into(),
            LockEntry {
                integrity: "live".into(),
                ..entry("https://x/r.git", &["pdf"])
            },
        );
        write_lock(&lock_path(&layout, Scope::User), &lock).unwrap();
        prune_store(&layout).unwrap();
        assert!(store.join("live").is_dir());
        assert!(!store.join("dead").exists());
        assert!(store.join(".staging-1").is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn install_update_and_remove_a_local_repository_end_to_end() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let root = scratch("e2e");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let layout = layout(&project);
        let remote = root.join("remote");
        fs::create_dir_all(remote.join("pdf")).unwrap();
        let git = |args: &[&str], dir: &Path| {
            let ok = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        let write_skill = |body: &str| {
            fs::write(
                remote.join("pdf/SKILL.md"),
                format!("---\nname: pdf\ndescription: PDF tools\n---\n{body}\n"),
            )
            .unwrap();
        };
        git(&["init", "--quiet"], &remote);
        write_skill("v1");
        git(&["add", "."], &remote);
        git(&["commit", "--quiet", "-m", "one"], &remote);
        let registries = BTreeMap::new();
        let cancel = AtomicBool::new(false);
        let spec = format!("{}#pdf", remote.display());

        let installed = install(
            &layout,
            &registries,
            Scope::Project,
            &spec,
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert!(installed.contains("Skills: pdf"), "{installed}");
        let duplicate = install(
            &layout,
            &registries,
            Scope::Project,
            &spec,
            &cancel,
            &mut |_| {},
        );
        assert!(
            duplicate
                .unwrap_err()
                .to_string()
                .contains("already installed")
        );
        let packages = list_packages(&layout).unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].skills, vec!["pdf".to_owned()]);
        assert!(
            roots(&layout)
                .iter()
                .all(|(_, root)| root.join("pdf/SKILL.md").is_file())
        );

        let up_to_date = update(&layout, Some("pdf"), &cancel, &mut |_| {}).unwrap();
        assert!(up_to_date.contains("already up to date"), "{up_to_date}");

        write_skill("v2");
        git(&["commit", "--quiet", "-am", "two"], &remote);
        let updated = update(&layout, Some("pdf"), &cancel, &mut |_| {}).unwrap();
        assert!(
            updated.contains("files: 0 added, 1 changed, 0 removed"),
            "{updated}"
        );
        assert_eq!(list_packages(&layout).unwrap()[0].commit.len(), 40);
        let body = fs::read_to_string(roots(&layout)[0].1.join("pdf/SKILL.md")).unwrap();
        assert!(body.contains("v2"));

        let removed = remove(&layout, "pdf").unwrap();
        assert!(removed.contains("Removed pdf"), "{removed}");
        assert!(list_packages(&layout).unwrap().is_empty());
        assert!(remove(&layout, "pdf").is_err());
        let store = store_root(&layout);
        let leftover = fs::read_dir(&store).map(|d| d.count()).unwrap_or(0);
        assert_eq!(leftover, 0);
        let _ = fs::remove_dir_all(&root);
    }
}
