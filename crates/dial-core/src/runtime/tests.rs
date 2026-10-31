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

#[test]
fn progress_and_tool_starts_stamp_the_last_activity_time() {
    let tree = Tree::default();
    let body = tree.start("worker", "watched", None);
    body.settle(
        String::new(),
        Some(serde_json::json!({"status":"running"})),
        Phase::Running,
    );
    let stamp = |body: &Body| body.read(|n| n.result.data.as_ref().unwrap()["updated_ms"].as_u64());
    assert_eq!(stamp(&body), None, "no activity yet");
    body.progress("generating", 1);
    let first = stamp(&body).expect("progress stamps the time");
    assert!(first > 0);
    body.tool_started("read", 2);
    assert!(stamp(&body).unwrap() >= first);
}

fn node_ids(nodes: &[Node]) -> Vec<&str> {
    nodes.iter().map(|node| node.id.as_str()).collect()
}

#[test]
fn out_of_order_acknowledgement_cannot_hide_a_newer_append() {
    let tree = Tree::default();
    let body = tree.start("tool", "race", None);
    body.append("first");
    let first = tree.pending();
    assert_eq!(first.len(), 1);
    // A concurrent append lands between the pending snapshot and its acknowledgement.
    body.append("second");
    tree.persisted(&first[0]);
    let still = tree.pending();
    assert_eq!(node_ids(&still), [body.snapshot().id.as_str()]);
    assert_eq!(still[0].result.revision, 2);
    // The newer acknowledgement clears the body; a late older one must not resurrect it.
    tree.persisted(&still[0]);
    assert!(tree.pending().is_empty());
    tree.persisted(&first[0]);
    assert!(tree.pending().is_empty(), "older ack must not regress");
    body.append("third");
    assert_eq!(tree.pending()[0].result.revision, 3);
}

#[test]
fn settled_only_pending_skips_running_bodies_until_they_settle() {
    let tree = Tree::default();
    let running = tree.start("worker", "live", None);
    running.append("streaming");
    assert_eq!(node_ids(&tree.pending()).len(), 1);
    assert!(tree.pending_settled().is_empty());
    running.settle("done".into(), None, Phase::Completed);
    let settled = tree.pending_settled();
    assert_eq!(settled.len(), 1);
    tree.persisted(&settled[0]);
    assert!(tree.pending().is_empty());
    assert!(tree.pending_settled().is_empty());
}

#[test]
fn restored_nodes_are_clean_and_stale_handles_cannot_revive_them() {
    let tree = Tree::default();
    let old = tree.start("tool", "old", None);
    old.append("text");
    let mut node = old.snapshot();
    node.result.revision = 4;
    // Restore replaces the body under the same id; the old handle is a stale incarnation.
    tree.restore(node.clone());
    assert!(tree.pending().is_empty());
    old.append(" late");
    // The stale handle marks the id dirty, but pending validates against the current body only,
    // which is clean at the restored revision.
    assert!(tree.pending().is_empty());
    let current = tree.bodies("tool");
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].snapshot().result.text, "text");
    assert_eq!(current[0].snapshot().result.revision, 4);
    assert!(!current[0].same(&old));
}

#[test]
fn failed_journal_publication_stays_unpublished_and_clean() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("runtime.jsonl");
    let journal = stream_journal::Journal::open(&session).unwrap();
    let tree = Tree::default();
    tree.journal(journal.clone());
    let body = tree.start("worker", "failure", None);
    body.append("durable");
    let before = body.snapshot();
    tree.persisted(&before);
    journal.fail_writes_for_test(std::fs::File::open(stream_journal::path(&session)).unwrap());
    body.append(" refused");
    body.settle("refused".into(), None, Phase::Completed);
    assert!(tree.check_persistence().is_err());
    assert_eq!(
        body.snapshot(),
        before,
        "failed publication must not be visible"
    );
    // The accepted revision was already acknowledged; a refused mutation cannot dirty it.
    assert!(tree.pending().is_empty());
}

#[test]
fn stale_acknowledgement_cannot_clean_a_replacement_at_the_same_revision() {
    let tree = Tree::default();
    let original = tree.start("tool", "restored", None);
    original.append("first");
    let restored = original.snapshot();
    original.append("obsolete");
    let stale = original.snapshot();
    tree.restore(restored);
    // A snapshot from the old incarnation must not acknowledge a future revision either.
    tree.persisted(&stale);
    let replacement = tree.bodies("tool").pop().unwrap();
    replacement.append("replacement");
    assert_eq!(
        replacement.snapshot().result.revision,
        stale.result.revision
    );
    tree.persisted(&stale);
    assert_eq!(tree.pending(), [replacement.snapshot()]);
    tree.persisted(&replacement.snapshot());
    tree.persisted(&stale);
    assert!(tree.pending().is_empty());
}
