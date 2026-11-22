use super::*;
use serde_json::json;

#[test]
fn task_ledger_unifies_todo_and_deliverable_obligations_and_syncs_control() {
    let mut todo = TodoList::default();
    assert!(
        !todo
            .call(&json!({"op": "set", "tasks": [{"content": "Verify fix"}]}))
            .is_error
    );
    let mut control = json!({
        "working_state": {
            "deliverables": [
                {"id": "d1", "description": "Ship patch", "status": "blocked"}
            ]
        }
    });
    let mut packet_cache = PacketCache::default();
    TaskLedger::sync_todos(&todo, &mut control, &mut packet_cache);

    assert!(control["todo"].is_array());
    assert_eq!(
        TaskLedger::rejection_outcome(&todo, &control),
        TurnOutcome::Blocked
    );
    let blocker = TaskLedger::completion_blocker(&todo, &control)
        .expect("open step and blocked deliverable must block completion");
    assert!(blocker.contains("Verify fix") || blocker.contains("blocked"));
    let notice =
        TaskLedger::outstanding_notice(&todo, &control["working_state"], TurnOutcome::Blocked)
            .expect("blocked turn should surface outstanding notice");
    assert!(notice.contains("Verify fix"));
}
