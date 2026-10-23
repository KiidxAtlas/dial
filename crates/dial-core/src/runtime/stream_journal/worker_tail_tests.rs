use super::*;
use crate::runtime::{Phase, Tree, WORKER_TAIL_LIMIT};

#[test]
fn worker_delta_volume_is_linear_without_repeated_metadata_or_tail_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("worker.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = Tree::default();
    tree.journal(journal.clone());
    let worker = tree.start("worker", "stream", None);
    worker.progress(&"metadata".repeat(8192), 1);
    worker.append(&"x".repeat(WORKER_TAIL_LIMIT));
    let before = std::fs::metadata(path(&session)).unwrap().len();
    for _ in 0..1024 {
        worker.append("é");
    }
    journal.check().unwrap();
    let written = std::fs::metadata(path(&session)).unwrap().len() - before;
    // Each frame is a small delta, not the 16 KiB tail or the much larger metadata.
    assert!(written < 160 * 1024, "worker delta bytes: {written}");
    let live = worker.snapshot();
    assert_eq!(live.result.text.len(), WORKER_TAIL_LIMIT);
    assert_eq!(recover(&session).unwrap(), vec![live]);
}

#[test]
fn worker_utf8_tails_survive_checkpoint_reopen_and_oversized_deltas() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("worker.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = Tree::default();
    tree.journal(journal.clone());
    let worker = tree.start("worker", "unicode", None);
    let unjournaled = Tree::default().start("worker", "unicode", None);
    let mut expected = String::new();
    for text in [
        "a".repeat(16383),
        "💡".into(),
        "é".repeat(20000),
        "終".into(),
    ] {
        expected.push_str(&text);
        let mut cut = expected.len().saturating_sub(WORKER_TAIL_LIMIT);
        while !expected.is_char_boundary(cut) {
            cut += 1;
        }
        expected.drain(..cut);
        worker.append(&text);
        unjournaled.append(&text);
        assert_eq!(worker.snapshot().result.text, expected);
        assert_eq!(unjournaled.snapshot().result.text, expected);
        assert_eq!(recover(&session).unwrap(), vec![worker.snapshot()]);
        let node = worker.snapshot();
        journal
            .checkpoint(&BTreeMap::from([(node.id, node.result.revision)]))
            .unwrap();
    }
    let live = worker.snapshot();
    drop(worker);
    drop(tree);
    drop(journal);
    let reopened = Journal::open(&session).unwrap();
    let restored = Tree::default();
    restored.journal(reopened);
    restored.restore(recover(&session).unwrap().remove(0));
    let worker = restored.bodies("worker").remove(0);
    assert_eq!(worker.snapshot(), live);
    worker.append(" resumed");
    assert_eq!(recover(&session).unwrap(), vec![worker.snapshot()]);
    restored.interrupt_open();
    let interrupted = worker.snapshot();
    worker.append(" late");
    worker.append("");
    assert_eq!(worker.snapshot(), interrupted);
    assert_eq!(recover(&session).unwrap(), vec![interrupted]);
}

#[test]
fn worker_failed_append_is_not_visible_and_latches_later_updates() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("worker.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = Tree::default();
    tree.journal(journal.clone());
    let worker = tree.start("worker", "failure", None);
    worker.append("durable prefix");
    let before = worker.snapshot();
    journal.fail_writes_for_test(File::open(path(&session)).unwrap());
    worker.append(" not published");
    worker.settle("not published either".into(), None, Phase::Completed);
    assert!(tree.check_persistence().is_err());
    assert_eq!(worker.snapshot(), before);
    assert_eq!(recover(&session).unwrap(), vec![before]);
}

#[test]
fn legacy_frames_and_bounded_deltas_preserve_revision_and_phase_guards() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("legacy.jsonl");
    let worker = Tree::default().start("worker", "legacy", None);
    let mut snapshot = serde_json::json!({"kind":"snapshot", "node":worker.snapshot()});
    // No version or retain_bytes field: this is the original journal encoding.
    let append =
        serde_json::json!({"kind":"append", "id":"runtime-1", "revision":1, "text":"legacy"});
    let mut bytes = format!("{snapshot}\n{append}\n");
    let bounded = |revision, retain_bytes, text| {
        serde_json::json!({
            "kind":"append", "schema_version":2, "id":"runtime-1", "revision":revision,
            "text":text, "retain_bytes":retain_bytes
        })
    };
    for frame in [
        bounded(1, 0, "duplicate"),
        bounded(3, 0, "gap"),
        bounded(2, WORKER_TAIL_LIMIT + 1, "invalid"),
        bounded(2, 3, " tail"),
    ] {
        bytes.push_str(&format!("{frame}\n"));
    }
    std::fs::write(path(&session), &bytes).unwrap();
    let recovered = recover(&session).unwrap().remove(0);
    assert_eq!(recovered.result.text, "acy tail");
    assert_eq!(recovered.result.revision, 2);
    snapshot["node"] = serde_json::to_value(&recovered).unwrap();
    snapshot["node"]["result"]["phase"] = serde_json::json!("completed");
    bytes.push_str(&format!("{snapshot}\n{}\n", bounded(3, 0, "late")));
    std::fs::write(path(&session), bytes).unwrap();
    let recovered = recover(&session).unwrap().remove(0);
    assert_eq!(recovered.result.text, "acy tail");
    assert_eq!(recovered.result.revision, 2);
    assert_eq!(recovered.result.phase, Phase::Completed);
}

#[test]
fn version_one_legacy_frames_replay_and_only_tail_deltas_require_version_two() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("versions.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = Tree::default();
    tree.journal(journal.clone());
    let assistant = tree.start("assistant", "legacy", None);
    assistant.append("ordinary append");
    let frames = || {
        std::fs::read_to_string(path(&session))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    // Explicit v1 snapshots and ordinary appends remain readable without migration.
    assert!(frames().iter().all(|frame| frame["schema_version"] == 1));
    assert_eq!(recover(&session).unwrap(), vec![assistant.snapshot()]);

    let worker = tree.start("worker", "bounded", None);
    worker.append("tail");
    let emitted = frames();
    let bounded = emitted.last().unwrap();
    assert_eq!(bounded["schema_version"], 2);
    assert_eq!(bounded["retain_bytes"], WORKER_TAIL_LIMIT - 4);
    // This is precisely the existing v1 reader's fail-closed version guard.
    assert!(bounded["schema_version"].as_u64().unwrap() > 1);
    assert_eq!(recover(&session).unwrap(), tree.snapshots());

    journal.checkpoint(&BTreeMap::new()).unwrap();
    // A checkpoint stores fully materialized nodes: no v2 semantics are needed.
    assert!(
        frames()
            .iter()
            .all(|frame| { frame["kind"] == "snapshot" && frame["schema_version"] == 1 })
    );
    assert_eq!(recover(&session).unwrap(), tree.snapshots());
    worker.append(" after checkpoint");
    assert_eq!(frames().last().unwrap()["schema_version"], 2);
    assert_eq!(recover(&session).unwrap(), tree.snapshots());
}
