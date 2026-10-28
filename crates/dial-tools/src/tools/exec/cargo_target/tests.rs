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
        .with_cargo_session("sess-1");
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
        .with_cargo_session("../escape");
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
