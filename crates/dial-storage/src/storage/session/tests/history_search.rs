use super::*;

#[test]
fn hit_identity_and_preview_survive_appends_and_cache_rebuild_without_journal_mutation() {
    let e = env();
    let mut session = new_session(&e);
    let id = session.append(user("before platypus after")).unwrap();
    let path = session.path().unwrap().to_owned();
    let before = fs::read(&path).unwrap();
    let hit = Session::search(&e.layout, "platypus", 10)
        .unwrap()
        .remove(0);
    assert_eq!(hit.entry_id, id);
    assert_eq!(hit.preview, "before platypus after");
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "search must never append a branch or resume marker"
    );
    session.append(assistant("a newer answer")).unwrap();
    session
        .set_additional_dirs(&[e._home.path().join("new-root")])
        .unwrap();
    let hit = Session::search(&e.layout, "platypus", 10)
        .unwrap()
        .remove(0);
    assert_eq!(hit.entry_id, id);
    assert_eq!(hit.preview, "before platypus after");
}
