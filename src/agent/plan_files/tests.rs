use super::*;
#[test]
fn local_paths_are_session_scoped_and_names_are_confined() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = save(tmp.path(), "session-1", "# A useful plan\nDo it.").unwrap();
    assert_eq!(uri, "local://a-useful-plan.md");
    assert!(resolve_local(tmp.path(), "session-1", &uri).is_some());
    assert!(resolve_local(tmp.path(), "session-2", &uri).is_none());
    assert!(resolve_local(tmp.path(), "session-1", "local://../secret.md").is_none());
}
