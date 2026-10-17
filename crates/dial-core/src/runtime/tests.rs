use super::*;
#[test]
fn coherent_runtime_legacy_phases_and_late_cancellation_results() {
    assert_eq!(
        serde_json::from_str::<Phase>("\"done\"").unwrap(),
        Phase::Completed
    );
    assert_eq!(
        serde_json::from_str::<Phase>("\"cancelled\"").unwrap(),
        Phase::Interrupted
    );
    let tree = Tree::default();
    let body = tree.start("tool", "slow command", None);
    body.append("partial evidence");
    tree.interrupt_open();
    body.settle("late success".into(), None, Phase::Completed);
    body.progress("still running", 3);
    assert_eq!(body.snapshot().result.phase, Phase::Interrupted);
    assert_eq!(body.snapshot().result.text, "late success");
    let worker = tree.start("worker", "cancelled owner", None);
    tree.interrupt_open();
    worker.settle(
        "useful late evidence".into(),
        Some(serde_json::json!({"status":"done","result":"keep this evidence"})),
        Phase::Completed,
    );
    let node = worker.snapshot();
    assert_eq!(node.result.phase, Phase::Interrupted);
    assert_eq!(node.result.data.as_ref().unwrap()["status"], "interrupted");
    assert_eq!(
        node.result.data.as_ref().unwrap()["result"],
        "keep this evidence"
    );
}
#[test]
fn views_share_one_mutating_body_and_settlement_retains_structured_objects() {
    let tree = Tree::default();
    let body = tree.start("tool", "task", None);
    let view = body.clone();
    body.append("live");
    assert_eq!(view.snapshot().result.text, "live");
    body.settle(
        "summary".into(),
        Some(serde_json::json!({"jobs":[{"id":"worker-1","status":"done"}]})),
        Phase::Completed,
    );
    let node = view.snapshot();
    assert_eq!(node.result.data.unwrap()["jobs"][0]["id"], "worker-1");
    body.append("late");
    assert_eq!(view.snapshot().result.text, "summary");
    tree.persisted(&view.snapshot());
    assert!(tree.pending().is_empty());
}
