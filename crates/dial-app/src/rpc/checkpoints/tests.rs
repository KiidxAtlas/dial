use super::*;
use std::sync::Arc;

fn code(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<super::super::RequestError>()
        .map_or(ErrorCode::Failed, |error| error.code)
}

/// A journal with one finished turn that edited `a.txt` (main) and `b.txt` (main and a worker).
fn journal_with_turn() -> (tempfile::TempDir, tempfile::TempDir, Arc<Checkpoints>, u64) {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(root.path().join(name), "original").unwrap();
    }
    let cp = Checkpoints::new(state.path().to_owned(), root.path().to_owned()).unwrap();
    let turn = cp.begin_turn("edit").unwrap();
    for (name, owner) in [("a.txt", "main"), ("b.txt", "main"), ("b.txt", "worker-1")] {
        cp.before_write_as(&root.path().join(name), owner).unwrap();
        std::fs::write(root.path().join(name), format!("edited by {owner}")).unwrap();
    }
    cp.begin_turn("next").unwrap();
    (root, state, Arc::new(cp), turn)
}

#[test]
fn review_lists_turns_files_and_diffs_with_tokens() {
    let (_root, _state, cp, turn) = journal_with_turn();
    let listed = list(Some(&cp));
    assert_eq!(listed["checkpoints"][0]["turn"], turn);
    assert_eq!(listed["checkpoints"][0]["files"], 2);
    assert_eq!(list(None), json!({"checkpoints": []}));

    let files = files(Some(&cp), &json!({"turn": turn})).unwrap();
    assert_eq!(files["files"][0]["path"], "a.txt");
    assert_eq!(files["files"][0]["overlap"], Value::Null);
    assert!(
        files["files"][1]["overlap"]
            .as_str()
            .unwrap()
            .contains("worker-1")
    );

    let diff = diff(Some(&cp), &json!({"turn": turn, "path": "a.txt"})).unwrap();
    assert!(diff["diff"].as_str().unwrap().contains("original"));
    assert!(!diff["token"].as_str().unwrap().is_empty());
}

#[test]
fn restore_needs_fresh_tokens_and_confirmed_overlap() {
    let (root, _state, cp, turn) = journal_with_turn();
    let token = |path: &str| {
        diff(Some(&cp), &json!({"turn": turn, "path": path})).unwrap()["token"].clone()
    };
    let a = token("a.txt");
    std::fs::write(root.path().join("a.txt"), "user edit").unwrap();
    let stale = json!({"turn": turn, "files": [{"path": "a.txt", "token": a}]});
    assert_eq!(
        code(restore(Some(&cp), &stale).unwrap_err()),
        ErrorCode::Conflict
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "user edit"
    );

    let shared = json!({"turn": turn, "files": [{"path": "b.txt", "token": token("b.txt")}]});
    assert_eq!(
        code(restore(Some(&cp), &shared).unwrap_err()),
        ErrorCode::Conflict
    );
    let mut confirmed = shared.clone();
    confirmed["overlap_confirmed"] = json!(true);
    let restored = restore(Some(&cp), &confirmed).unwrap();
    assert_eq!(restored["restored"], json!(["b.txt"]));
    assert_eq!(
        std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
        "original"
    );

    let fresh = json!({"turn": turn, "files": [{"path": "a.txt", "token": token("a.txt")}]});
    restore(Some(&cp), &fresh).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "original"
    );
}

#[test]
fn refusals_carry_codes() {
    let (_root, _state, cp, turn) = journal_with_turn();
    assert_eq!(
        code(files(None, &json!({"turn": 1})).unwrap_err()),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(files(Some(&cp), &json!({})).unwrap_err()),
        ErrorCode::InvalidParams
    );
    assert_eq!(
        code(files(Some(&cp), &json!({"turn": 999})).unwrap_err()),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(diff(Some(&cp), &json!({"turn": turn, "path": "zzz.txt"})).unwrap_err()),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(restore(Some(&cp), &json!({"turn": turn, "files": []})).unwrap_err()),
        ErrorCode::InvalidParams
    );
    assert_eq!(undo_turns(&json!({})).unwrap(), 1);
    assert_eq!(
        code(undo_turns(&json!({"turns": 0})).unwrap_err()),
        ErrorCode::InvalidParams
    );
    let refused = undo_result(
        1,
        Err(crate::rewind::UndoRefusal::Gate(anyhow::anyhow!(
            "hook says no"
        ))),
    );
    assert_eq!(code(refused.unwrap_err()), ErrorCode::Refused);
    let kept = undo_result(
        1,
        Ok(crate::rewind::UndoOutcome {
            rollback_error: Some("disk".into()),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(kept["conversation_kept"], true);
}
