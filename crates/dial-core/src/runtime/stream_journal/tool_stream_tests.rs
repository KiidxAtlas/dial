use super::*;
use crate::runtime::{Phase, TOOL_STREAM_LIMIT, TOOL_STREAM_MARKER, Tree};

#[test]
fn running_tool_output_stays_bounded_and_prefix_stable() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("tool.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = Tree::default();
    tree.journal(journal.clone());
    let tool = tree.start("tool", "bash", None);
    let chunk = "é".repeat(32 * 1024);
    let mut previous = String::new();
    for _ in 0..64 {
        tool.append(&chunk);
        let text = tool.snapshot().result.text;
        // Byte-cursor consumers (RPC state deltas) rely on the retained text only growing.
        assert!(text.starts_with(&previous));
        previous = text;
    }
    journal.check().unwrap();
    let live = tool.snapshot();
    assert!(live.result.text.len() <= TOOL_STREAM_LIMIT);
    assert!(live.result.text.ends_with(TOOL_STREAM_MARKER));
    assert_eq!(live.result.text.matches(TOOL_STREAM_MARKER).count(), 1);
    assert!(std::fs::metadata(path(&session)).unwrap().len() < 2 * TOOL_STREAM_LIMIT as u64);
    assert_eq!(recover(&session).unwrap(), vec![live.clone()]);
    // Settling replaces the live text as before.
    tool.settle("done".into(), None, Phase::Completed);
    assert_eq!(tool.snapshot().result.text, "done");
}

#[test]
fn an_oversized_update_is_published_bounded_without_latching_the_journal() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("oversized.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = Tree::default();
    tree.journal(journal.clone());
    let tool = tree.start("tool", "read", None);
    let huge = "x".repeat(MAX_FRAME + 1);
    tool.settle(
        huge.clone(),
        Some(serde_json::json!({ "content": huge })),
        Phase::Completed,
    );
    // The writer stays usable: later bodies and updates still journal.
    journal.check().unwrap();
    let settled = tool.snapshot();
    assert_eq!(settled.result.phase, Phase::Completed);
    assert!(settled.result.text.len() < 128 * 1024);
    assert!(settled.result.data.unwrap()["omitted"].is_string());
    let next = tree.start("assistant", "reply", None);
    next.append("still journaled");
    journal.check().unwrap();
    let recovered = recover(&session).unwrap();
    assert!(
        recovered
            .iter()
            .any(|node| node.id == next.snapshot().id && node.result.text == "still journaled")
    );
}
