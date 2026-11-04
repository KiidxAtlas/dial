use super::*;
use std::fs;

fn layout() -> (tempfile::TempDir, Layout) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("cwd");
    fs::create_dir_all(&cwd).unwrap();
    let layout = Layout::with_user_dir(&cwd, root.path().join("home")).unwrap();
    (root, layout)
}

#[test]
fn recorded_move_is_found_by_its_source_path() {
    let (root, layout) = layout();
    let from = root.path().join("old.jsonl");
    let to = root.path().join("new.jsonl");
    record(&layout, "sess-1", &from, &to).unwrap();

    let moved = moved_away_from(&layout, &from).unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].session_id, "sess-1");
    assert_eq!(moved[0].to, resolve_allowing_missing(&to).unwrap());
    assert!(moved_away_from(&layout, &to).unwrap().is_empty());
}

#[test]
fn move_onto_the_same_path_records_nothing() {
    let (root, layout) = layout();
    let path = root.path().join("same.jsonl");
    record(&layout, "sess-1", &path, &path).unwrap();
    assert!(moved_away_from(&layout, &path).unwrap().is_empty());
}

#[test]
fn corrupt_ledger_is_quarantined_and_history_resets() {
    let (_root, layout) = layout();
    crate::platform::atomic::private_dir(&layout.user_dir).unwrap();
    fs::write(layout.user_dir.join(LEDGER_FILE), b"{not json").unwrap();
    let path = layout.state_dir.join("anything.jsonl");

    assert!(moved_away_from(&layout, &path).unwrap().is_empty());
    assert!(!layout.user_dir.join(LEDGER_FILE).exists());
    let quarantined: Vec<String> = fs::read_dir(&layout.user_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("session-transfers.json.corrupt-"))
        .collect();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(
        fs::read(layout.user_dir.join(&quarantined[0])).unwrap(),
        b"{not json"
    );

    // Recording works again on a fresh ledger.
    let moved_to = layout.state_dir.join("moved.jsonl");
    record(&layout, "s1", &path, &moved_to).unwrap();
    assert_eq!(moved_away_from(&layout, &path).unwrap().len(), 1);
}

#[test]
fn rollback_removes_only_the_newest_exact_record() {
    let (root, layout) = layout();
    let from = root.path().join("old.jsonl");
    let first = root.path().join("first.jsonl");
    let second = root.path().join("second.jsonl");
    record(&layout, "s1", &from, &first).unwrap();
    record(&layout, "s2", &from, &second).unwrap();
    record(&layout, "s1", &from, &first).unwrap();

    rollback(&layout, "s1", &from, &first).unwrap();

    let remaining: Vec<(String, PathBuf)> = moved_away_from(&layout, &from)
        .unwrap()
        .into_iter()
        .map(|t| (t.session_id, t.to))
        .collect();
    assert_eq!(
        remaining,
        vec![
            ("s1".to_owned(), resolve_allowing_missing(&first).unwrap()),
            ("s2".to_owned(), resolve_allowing_missing(&second).unwrap()),
        ]
    );
}

#[test]
fn rollback_of_an_absent_record_changes_nothing() {
    let (root, layout) = layout();
    let from = root.path().join("old.jsonl");
    let to = root.path().join("new.jsonl");
    record(&layout, "s1", &from, &to).unwrap();
    let before = fs::read(ledger_path(&layout)).unwrap();

    rollback(&layout, "other", &from, &to).unwrap();
    rollback(&layout, "s1", &to, &from).unwrap();

    assert_eq!(fs::read(ledger_path(&layout)).unwrap(), before);
}

#[test]
fn ledger_refused_before_publication_is_marked_unpublished_for_rollback_and_record() {
    let (root, layout) = layout();
    let from = root.path().join("old.jsonl");
    let to = root.path().join("new.jsonl");
    crate::platform::atomic::private_dir(&layout.user_dir).unwrap();
    // A directory where the ledger file belongs fails the load before any write is attempted, so
    // no entry was published and the refusal may be marked as such.
    fs::create_dir(ledger_path(&layout)).unwrap();

    let error = rollback(&layout, "s1", &from, &to).unwrap_err();
    assert!(definitely_unpublished(&error));
    let error = record(&layout, "s1", &from, &to).unwrap_err();
    assert!(definitely_unpublished(&error));
}

#[test]
fn refusal_before_publication_is_marked_unpublished() {
    let (root, layout) = layout();
    let from = root.path().join("old.jsonl");
    let to = root.path().join("new.jsonl");
    // The user directory itself is a file, so nothing can be locked or published.
    fs::create_dir_all(layout.user_dir.parent().unwrap()).unwrap();
    fs::write(&layout.user_dir, b"not a directory").unwrap();

    let error = record(&layout, "s1", &from, &to).unwrap_err();
    assert!(definitely_unpublished(&error));
}
