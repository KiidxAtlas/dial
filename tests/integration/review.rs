use dial::storage::{checkpoint::Checkpoints, session::Session};
use serde_json::json;
use std::fs;

#[test]
fn selective_revert_survives_reopen_preserves_other_files_and_is_undoable() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.txt"), "original").unwrap();
    fs::write(root.path().join("b.txt"), "other original").unwrap();
    let checkpoint = Checkpoints::new(state.path().to_owned(), root.path().to_owned()).unwrap();
    let turn = checkpoint.begin_turn("two edits").unwrap();
    for path in ["a.txt", "b.txt"] {
        checkpoint.before_write(&root.path().join(path)).unwrap();
        fs::write(root.path().join(path), "edited").unwrap();
    }
    drop(checkpoint);
    let checkpoint = Checkpoints::new(state.path().to_owned(), root.path().to_owned()).unwrap();
    let (diff, token) = checkpoint.review_file(turn, "a.txt").unwrap();
    assert!(diff.contains("original") && diff.contains("edited"));
    checkpoint.restore_file(turn, "a.txt", &token).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("b.txt")).unwrap(),
        "edited"
    );
    checkpoint.rollback(1).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "edited"
    );
}

#[test]
fn selective_revert_refuses_concurrent_user_changes_and_private_paths() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let path = root.path().join("a.txt");
    fs::write(&path, "original").unwrap();
    let checkpoint = Checkpoints::new(state.path().to_owned(), root.path().to_owned()).unwrap();
    let turn = checkpoint.begin_turn("edit").unwrap();
    checkpoint.before_write(&path).unwrap();
    fs::write(&path, "edited").unwrap();
    let (_, token) = checkpoint.review_file(turn, "a.txt").unwrap();
    fs::write(&path, "user's later edit").unwrap();
    assert!(checkpoint.restore_file(turn, "a.txt", &token).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "user's later edit");
    assert!(checkpoint.review_file(turn, "../outside").is_err());
    assert!(checkpoint.review_file(turn, ".env").is_err());
}

#[test]
fn resume_summary_uses_saved_control_state_without_changing_conversation() {
    let mut session = Session::ephemeral();
    session.set_control_state(json!({
        "todo": [{"content":"unfinished repair","status":"in_progress"}, {"content":"completed edit","status":"completed"}],
        "verification_results": [{"name":"unit tests","status":"passed"}]
    })).unwrap();
    let before = session.messages();
    let summary = session.resume_summary();
    assert!(summary.contains("unfinished repair"));
    assert!(summary.contains("completed edit"));
    assert!(summary.contains("unit tests"));
    assert_eq!(session.messages(), before);
}
