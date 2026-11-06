use super::*;
use crate::tools::Workspace;

#[test]
fn explicit_inherited_target_is_never_overridden() {
    let dir = Path::new("/w/.tmp/cargo-target/s1");
    assert_eq!(chosen(Some(OsStr::new("/custom")), Some(dir)), None);
}

#[test]
fn unset_environment_takes_the_session_directory() {
    let dir = Path::new("/w/.tmp/cargo-target/s1");
    assert_eq!(chosen(None, Some(dir)), Some(dir));
}

#[test]
fn no_session_scope_exports_nothing() {
    assert_eq!(chosen(None, None), None);
}

#[test]
fn session_names_are_single_safe_path_components() {
    assert!(valid_session("a1-B_2"));
    assert!(!valid_session(""));
    assert!(!valid_session("../x"));
    assert!(!valid_session("a/b"));
    assert!(!valid_session(&"a".repeat(MAX_SESSION_LEN + 1)));
}

#[test]
fn workspace_scopes_target_per_session_and_copies_get_their_own_root() {
    let project = tempfile::tempdir().unwrap();
    let copy = tempfile::tempdir().unwrap();
    let ws = Workspace::new(project.path())
        .unwrap()
        .with_cargo_session("sess-1")
        .unwrap();
    assert_eq!(ws.cargo_target_dir(), Some(dir_for(ws.root(), "sess-1")));
    let worker = ws.rebased(copy.path()).unwrap();
    assert_eq!(
        worker.cargo_target_dir(),
        Some(dir_for(worker.root(), "sess-1"))
    );
    assert_ne!(ws.cargo_target_dir(), worker.cargo_target_dir());
}

#[test]
fn invalid_session_name_leaves_isolation_off() {
    let project = tempfile::tempdir().unwrap();
    let ws = Workspace::new(project.path())
        .unwrap()
        .with_cargo_session("../escape")
        .unwrap();
    assert_eq!(ws.cargo_target_dir(), None);
}

#[test]
fn explicit_command_target_is_never_overridden() {
    let mut command = Command::new("cargo");
    command.env("CARGO_TARGET_DIR", "/explicit");
    apply(&mut command, Some(PathBuf::from("/session")));
    assert_eq!(
        command
            .get_envs()
            .find(|(key, _)| *key == "CARGO_TARGET_DIR")
            .unwrap()
            .1,
        Some(OsStr::new("/explicit")),
    );
}

#[test]
fn explicit_removal_is_not_replaced() {
    let mut command = Command::new("cargo");
    command.env_remove("CARGO_TARGET_DIR");
    apply(&mut command, Some(PathBuf::from("/session")));
    assert_eq!(
        command
            .get_envs()
            .find(|(key, _)| *key == "CARGO_TARGET_DIR")
            .unwrap()
            .1,
        None,
    );
}

#[test]
fn stale_session_targets_are_pruned_but_the_current_and_recent_ones_stay() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    for session in ["current", "recent", "old"] {
        std::fs::create_dir_all(dir_for(root, session).join("debug")).unwrap();
    }
    mark_used(&dir_for(root, "recent"));
    // A stray file and a non-session name are never touched.
    let base = dir_for(root, "x").parent().unwrap().to_path_buf();
    std::fs::write(base.join("notes.txt"), b"keep").unwrap();
    std::fs::create_dir_all(base.join("not a session")).unwrap();

    // Nothing is old enough under a one-hour threshold.
    assert!(prune_stale(root, "current", Duration::from_secs(3600)).is_empty());

    std::thread::sleep(Duration::from_millis(50));
    mark_used(&dir_for(root, "recent"));
    let removed = prune_stale(root, "current", Duration::from_millis(25));
    assert_eq!(removed, vec![dir_for(root, "old")]);
    assert!(dir_for(root, "current").is_dir());
    assert!(dir_for(root, "recent").is_dir());
    assert!(base.join("notes.txt").is_file());
    assert!(base.join("not a session").is_dir());
}

#[test]
fn marking_never_creates_a_missing_target_directory() {
    let project = tempfile::tempdir().unwrap();
    let dir = dir_for(project.path(), "unused");
    mark_used(&dir);
    assert!(!dir.exists());
}

#[test]
fn a_new_session_adopts_the_most_recent_idle_directory_but_not_a_busy_one() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    for session in ["older", "newer", "building"] {
        std::fs::create_dir_all(dir_for(root, session).join("debug")).unwrap();
        mark_used(&dir_for(root, session));
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(dir_for(root, "newer").join("debug/artifact"), b"warm").unwrap();
    // A Cargo build holds the lock in the most recently used directory.
    let lock = std::fs::File::create(dir_for(root, "building").join("debug/.cargo-lock")).unwrap();
    lock.lock().unwrap();

    // Not idle long enough yet.
    assert_eq!(adopt_idle(root, "fresh", Duration::from_secs(3600)), None);

    let adopted = adopt_idle(root, "fresh", Duration::ZERO);
    assert_eq!(adopted, Some(dir_for(root, "newer")));
    assert!(dir_for(root, "fresh").join("debug/artifact").is_file());
    assert!(!dir_for(root, "newer").exists());
    assert!(dir_for(root, "building").is_dir());
    assert!(dir_for(root, "older").is_dir());

    // A session that already has a directory keeps it.
    assert_eq!(adopt_idle(root, "fresh", Duration::ZERO), None);
    // A busy directory is never pruned either.
    assert!(!prune_stale(root, "fresh", Duration::ZERO).contains(&dir_for(root, "building")));
    drop(lock);
}

#[test]
fn nested_cross_target_build_locks_prevent_adoption_and_pruning() {
    let project = tempfile::tempdir().unwrap();
    let dir = dir_for(project.path(), "building");
    std::fs::create_dir_all(dir.join("aarch64-unknown-linux-gnu/debug")).unwrap();
    let lock =
        std::fs::File::create(dir.join("aarch64-unknown-linux-gnu/debug/.cargo-lock")).unwrap();
    lock.lock().unwrap();
    assert!(cargo_build_running(&dir));
    assert_eq!(adopt_idle(project.path(), "fresh", Duration::ZERO), None);
    assert!(prune_stale(project.path(), "fresh", Duration::ZERO).is_empty());
    assert!(dir.exists());
}

#[test]
fn session_leases_protect_idle_targets_until_all_workspaces_release_them() {
    let project = tempfile::tempdir().unwrap();
    // The workspace canonicalizes its root (macOS temp dirs live under /private/var).
    let root = std::fs::canonicalize(project.path()).unwrap();
    let ws = Workspace::new(&root)
        .unwrap()
        .with_cargo_session("live")
        .unwrap();
    let clone = ws.clone();
    let dir = ws.cargo_target_dir().unwrap();
    std::fs::create_dir_all(dir.join("debug")).unwrap();
    drop(ws);
    assert!(prune_stale(&root, "fresh", Duration::ZERO).is_empty());
    assert_eq!(adopt_idle(&root, "fresh", Duration::ZERO), None);
    drop(clone);
    assert_eq!(adopt_idle(&root, "fresh", Duration::ZERO), Some(dir));
}

#[test]
fn maintenance_lease_refuses_new_session_exports_until_released() {
    let project = tempfile::tempdir().unwrap();
    let lease = maintenance_lease(project.path(), "old").unwrap();
    assert!(
        Workspace::new(project.path())
            .unwrap()
            .with_cargo_session("old")
            .is_err()
    );
    drop(lease);
    assert!(
        Workspace::new(project.path())
            .unwrap()
            .with_cargo_session("old")
            .is_ok()
    );
}
