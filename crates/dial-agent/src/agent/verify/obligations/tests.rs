use super::*;
use serde_json::json;

#[test]
fn blocked_step_and_blocked_deliverable_drive_rejection_outcome_and_blocker_text() {
    let mut todo = TodoList::default();
    assert!(
        !todo
            .call(&json!({"op": "set", "tasks": [{"content": "Deploy build"}]}))
            .is_error
    );
    assert!(
        !todo
            .call(&json!({"op": "block", "index": 1, "reason": "missing prod credentials"}))
            .is_error
    );
    let empty_working = json!({});
    let obligations = Obligations::collect(&todo, &empty_working);
    assert!(obligations.has_external_blocker());
    assert_eq!(obligations.rejection_outcome(), TurnOutcome::Blocked);
    let blocker = obligations
        .completion_blocker()
        .expect("blocked step refuses completion");
    assert!(blocker.contains("Deploy build: missing prod credentials"));

    let empty_todo = TodoList::default();
    let blocked_working = json!({
        "deliverables": [
            {"id": "d1", "description": "Signed bundle", "status": "blocked", "reason": "no key"}
        ]
    });
    let obligations = Obligations::collect(&empty_todo, &blocked_working);
    assert!(obligations.has_external_blocker());
    assert_eq!(obligations.rejection_outcome(), TurnOutcome::Blocked);
    assert!(obligations.completion_blocker().is_some());
}

#[test]
fn open_steps_refuse_completion_while_optional_follow_ups_do_not() {
    let mut todo = TodoList::default();
    assert!(
        !todo
            .call(&json!({
                "op": "set",
                "tasks": [
                    {"content": "Write parser", "status": "completed"},
                    {"content": "Add benchmark", "kind": "follow_up"}
                ]
            }))
            .is_error
    );
    let working = json!({});
    let obligations = Obligations::collect(&todo, &working);
    assert!(!obligations.has_open_steps());
    assert!(!obligations.has_external_blocker());
    assert_eq!(obligations.rejection_outcome(), TurnOutcome::Incomplete);
    assert!(obligations.completion_blocker().is_none());
    let notice = obligations
        .outstanding_notice(TurnOutcome::Answered)
        .expect("optional follow-up stays reported");
    assert!(notice.contains("0 execution step(s)"));
    assert!(notice.contains("1 optional follow-up(s)"));
    assert!(notice.contains("does not mark that work complete"));
}
