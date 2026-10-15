//! User-facing skill inventory, opt-in imports and enablement. Foreign sources are never activated
//! by browsing them. Preferences live outside the workspace, separate from agent-authored skills.
use super::*;
use crate::storage::settings_store;
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

fn preferences_path(layout: &Layout, scope: Scope) -> Result<PathBuf> {
    layout.ensure_state_outside_project()?;
    ensure!(
        !layout.user_dir.starts_with(&layout.project_root),
        "skill preferences must be outside the project"
    );
    let dir = match scope {
        Scope::Project => &layout.state_dir,
        Scope::User => &layout.user_dir,
    };
    settings_store::validate_private_path(dir)?;
    Ok(dir.join("skills.json"))
}

fn preferences(layout: &Layout, scope: Scope) -> Result<BTreeMap<String, bool>> {
    let path = preferences_path(layout, scope)?;
    let Some(bytes) = settings_store::read_private(&path)? else {
        return Ok(BTreeMap::new());
    };
    ensure!(
        bytes.len() <= MAX_READ_BYTES,
        "skill preferences exceed 64 KiB"
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid skill preferences in {}", path.display()))
}

pub fn installed(layout: &Layout) -> Result<Vec<(Skill, bool)>> {
    let mut enabled = preferences(layout, Scope::User)?;
    enabled.extend(preferences(layout, Scope::Project)?);
    let mut skills = discover_all(layout);
    for skill in crate::storage::memory_backend::managed_skills(layout)? {
        if let Err(index) = skills.binary_search_by(|authored| authored.name.cmp(&skill.name)) {
            skills.insert(index, skill);
        }
    }
    Ok(skills
        .into_iter()
        .map(|skill| {
            let active = enabled.get(&skill.name).copied().unwrap_or(true);
            (skill, active)
        })
        .collect())
}

pub fn set_enabled(layout: &Layout, name: &str, enabled: bool, scope: Scope) -> Result<()> {
    ensure!(valid_name(name), "invalid skill name");
    ensure!(
        installed(layout)?
            .iter()
            .any(|(skill, _)| skill.name == name),
        "no installed skill named {name:?}"
    );
    let path = preferences_path(layout, scope)?;
    let _lock = settings_store::locked(&path)?;
    let mut settings = preferences(layout, scope)?;
    settings.insert(name.to_owned(), enabled);
    let bytes = serde_json::to_vec_pretty(&settings)?;
    ensure!(
        bytes.len() <= MAX_READ_BYTES,
        "skill preferences exceed 64 KiB"
    );
    settings_store::write_private(&path, &bytes)
}

pub const HELP: &str = "Skills: /skills list | sources | enable <name> [--user] | disable <name> [--user] | import <path> [--user] [--dry-run] [--update] | install <git-url>[@ref][#path] | install <registry>/<skill>[@ref] [--user] | update [name] | remove <name> | installed | search <query>";

pub fn list(layout: &Layout) -> Result<String> {
    let mut lines: Vec<String> = installed(layout)?
        .into_iter()
        .map(|(skill, enabled)| {
            format!(
                "{} [{}; {}/{}] {} — {}",
                skill.name,
                if enabled { "on" } else { "off" },
                skill.origin,
                skill.source.as_str(),
                skill.path.display(),
                skill.description
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("No installed skills. Browse /skills sources, then import a directory.".into());
    }
    lines.push(HELP.into());
    Ok(lines.join("\n"))
}

fn source_roots(layout: &Layout, home: Option<&Path>) -> Vec<(String, PathBuf)> {
    use crate::storage::foreign::{PROJECT_SKILL_ROOTS, USER_SKILL_ROOTS};
    let project = PROJECT_SKILL_ROOTS
        .iter()
        .map(|r| ("project", r.provider, layout.project_root.join(r.rel)));
    let user = home.into_iter().flat_map(|h| {
        USER_SKILL_ROOTS
            .iter()
            .map(move |r| ("user", r.provider, h.join(r.rel)))
    });
    let mut roots: Vec<(String, PathBuf)> = Vec::new();
    for (scope, provider, root) in project.chain(user) {
        if !roots.iter().any(|(_, path)| path == &root) {
            roots.push((format!("{provider} {scope}"), root));
        }
    }
    roots
}

fn browse(layout: &Layout, home: Option<&Path>) -> String {
    let mut lines = vec!["Local sources (not imported or enabled by browsing):".to_owned()];
    for (label, root) in source_roots(layout, home) {
        if !is_real_dir(&root) {
            continue;
        }
        let mut paths: Vec<_> = fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        paths.sort();
        lines.push(format!("{label}: {}", root.display()));
        let skills: Vec<_> = paths
            .iter()
            .filter_map(|path| load_skill(path, Scope::User))
            .take(MAX_SKILLS_PER_DIR)
            .collect();
        if skills.is_empty() {
            lines.push("  No compatible skills".into());
        }
        for skill in skills {
            lines.push(format!(
                "  {} — {}\n    {}",
                skill.name,
                skill.description,
                skill.dir().display()
            ));
        }
    }
    if lines.len() == 1 {
        lines.push("No common local source directories found.".into());
    }
    lines.push("Import a listed directory with /skills import <path> (or dial skills import <path>). Other local checkouts work too; no downloads or scripts are run.".into());
    lines.join("\n")
}

pub fn sources(layout: &Layout) -> String {
    browse(layout, user_home().as_deref())
}

/// Slash-command adapter; the controller supplies words using the shared command tokenizer. `cancel` and
/// `progress` serve the network-bound package actions (install and update).
pub fn command(
    layout: &Layout,
    registries: &BTreeMap<String, String>,
    mut words: Vec<String>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    if words.is_empty() {
        return list(layout);
    }
    let action = words.remove(0);
    let mut scope = Scope::Project;
    let mut dry_run = false;
    let mut update = false;
    let mut positional = Vec::new();
    for word in words {
        match word.as_str() {
            "--user" => scope = Scope::User,
            "--dry-run" if action == "import" => dry_run = true,
            "--update" if action == "import" => update = true,
            flag if flag.starts_with("--") => anyhow::bail!("unknown option {flag:?}. {HELP}"),
            _ => positional.push(word),
        }
    }
    match (action.as_str(), positional.as_slice()) {
        ("list", []) => list(layout),
        ("sources", []) => Ok(sources(layout)),
        ("enable" | "disable", [name]) => {
            set_enabled(layout, name, action == "enable", scope)?;
            list(layout)
        }
        ("import", [path]) => {
            let path = if path == "~" {
                user_home().context("HOME is not set")?
            } else if let Some(suffix) = path.strip_prefix("~/") {
                user_home().context("HOME is not set")?.join(suffix)
            } else {
                layout.project_root.join(path)
            };
            Ok(import::run(layout, &path, scope, dry_run, update)?.join("\n"))
        }
        ("install", [spec]) => packages::install(layout, registries, scope, spec, cancel, progress),
        ("update", []) => packages::update(layout, None, cancel, progress),
        ("update", [name]) => packages::update(layout, Some(name), cancel, progress),
        ("remove", [name]) => packages::remove(layout, name),
        ("installed", []) => packages::list_text(layout),
        ("search", query) => packages::search_text(registries, &query.join(" ")),
        _ => anyhow::bail!("{HELP}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(root: &Path, name: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: Review code\n---\nReview carefully."),
        )
        .unwrap();
    }

    fn command(layout: &Layout, args: &str) -> Result<String> {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        super::command(
            layout,
            &std::collections::BTreeMap::new(),
            crate::agent::context::split_words(args),
            &cancel,
            &mut |_| {},
        )
    }

    #[test]
    fn toggles_persist_override_user_and_block_tool_loading() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        skill(&Scope::User.root(&layout), "review");
        skill(&Scope::Project.root(&layout), "review");
        assert_eq!(discover(&layout)[0].source, Scope::Project);
        set_enabled(&layout, "review", false, Scope::User).unwrap();
        assert!(discover(&layout).is_empty());
        assert!(catalog_block(&discover(&layout)).is_empty());
        assert!(skill_tool::call(&layout, &json!({"name":"review"})).is_error);
        assert!(list(&layout).unwrap().contains("[off; Dial/project]"));
        let reloaded = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        command(&reloaded, "enable review").unwrap();
        assert_eq!(discover(&reloaded).len(), 1);
        assert!(!skill_tool::call(&reloaded, &json!({"name":"review"})).is_error);
        command(&reloaded, "disable review").unwrap();
        assert!(discover(&reloaded).is_empty()); // must not fall back to the user copy
        assert!(command(&reloaded, "enable missing").is_err());
        assert!(command(&reloaded, "enable review --wat").is_err());
    }

    #[test]
    fn browse_is_read_only_and_import_is_explicit() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        skill(&foreign.path().join(".claude/skills"), "review");
        skill(&project.path().join(".agents/skills"), "local");
        let text = browse(&layout, Some(foreign.path()));
        assert!(text.contains("Claude user"));
        assert!(text.contains("Agents project"));
        // Project foreign roots are discovered by design; browsing itself enables nothing, so only
        // the project-level `.agents/skills/local` shows up, never the foreign user `review`.
        assert_eq!(discover(&layout).len(), 1);
        let source = project.path().join("my checkout");
        skill(&source, "review");
        assert!(
            command(&layout, "import 'my checkout' --dry-run")
                .unwrap()
                .contains("Would import review")
        );
        assert_eq!(discover(&layout).len(), 1);
        command(&layout, "import 'my checkout'").unwrap();
        assert_eq!(discover(&layout).len(), 2);
        assert!(command(&layout, "sources extra").is_err());
    }

    #[test]
    fn invalid_preferences_fail_closed_and_are_not_overwritten() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        skill(&Scope::Project.root(&layout), "review");
        let path = preferences_path(&layout, Scope::Project).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "broken").unwrap();
        assert!(list(&layout).is_err());
        assert!(discover(&layout).is_empty());
        assert!(set_enabled(&layout, "review", true, Scope::Project).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "broken");
    }

    #[test]
    #[cfg(unix)]
    fn redirected_preferences_are_rejected_without_writing_workspace() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        skill(&Scope::Project.root(&layout), "review");
        std::os::unix::fs::symlink(project.path(), home.path().join("projects")).unwrap();
        assert!(installed(&layout).is_err());
        assert!(set_enabled(&layout, "review", false, Scope::Project).is_err());
        assert!(
            !project
                .path()
                .join(layout.state_dir.file_name().unwrap())
                .exists()
        );
    }

    #[test]
    #[cfg(unix)]
    fn special_files_do_not_block_source_browsing_or_preferences() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        let dir = project.path().join(".agents/skills/review");
        fs::create_dir_all(&dir).unwrap();
        let fifo = |path: &Path| {
            let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        };
        fifo(&dir.join(SKILL_FILE));
        assert!(browse(&layout, None).contains("No compatible skills"));
        fifo(&preferences_path(&layout, Scope::User).unwrap());
        assert!(installed(&layout).is_err());
    }
}
