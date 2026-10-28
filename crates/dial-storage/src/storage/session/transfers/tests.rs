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
