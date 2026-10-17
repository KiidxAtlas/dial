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
fn home_workspace_can_persist_user_skill_preferences() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(home.path(), home.path().join(".dial")).unwrap();
    skill(&Scope::User.root(&layout), "review");
    set_enabled(&layout, "review", false, Scope::User).unwrap();
    assert!(discover(&layout).is_empty());
    set_enabled(&layout, "review", true, Scope::User).unwrap();
    assert_eq!(discover(&layout)[0].name, "review");
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
