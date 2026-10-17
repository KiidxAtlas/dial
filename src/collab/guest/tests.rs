use super::*;
use crate::collab::protocol::{frame, snapshot_chunks};
use std::sync::Arc;

fn message(role: &str, text: &str) -> Value {
    json!({"role": role, "content": text})
}

/// Frames the host would send for a log, in the order the socket delivers them.
fn snapshot_frames(log: &[Value]) -> Vec<Value> {
    let entries: Arc<Vec<Arc<str>>> =
        Arc::new(log.iter().map(|m| Arc::from(m.to_string())).collect());
    let mut frames = vec![json!({"t": "welcome", "proto": 1002, "readOnly": true})];
    frames
        .extend(snapshot_chunks(entries).map(|text| serde_json::from_str::<Value>(&text).unwrap()));
    frames
}

fn entry_frame(seq: usize, message: &Value) -> Value {
    serde_json::from_str(&frame::entry_json(seq, &message.to_string())).unwrap()
}

#[test]
fn join_after_two_messages_projects_history_exactly_once() {
    let log = vec![message("user", "hi"), message("assistant", "hello")];
    let mut replica = Replica::default();
    let mut changes = Vec::new();
    for frame in snapshot_frames(&log) {
        changes.push(replica.apply(&frame));
    }
    assert_eq!(
        changes
            .iter()
            .filter(|c| **c == Change::Snapshot(2))
            .count(),
        1
    );
    assert_eq!(replica.entries(), log.as_slice());
    // A late broadcast of an entry already inside the snapshot is a duplicate.
    assert_eq!(replica.apply(&entry_frame(1, &log[1])), Change::None);
    assert_eq!(replica.entries().len(), 2);
}

#[test]
fn live_entries_append_by_index_and_gaps_are_reported() {
    let mut replica = Replica::default();
    for frame in snapshot_frames(&[message("user", "one")]) {
        replica.apply(&frame);
    }
    let next = message("assistant", "two");
    assert_eq!(replica.apply(&entry_frame(1, &next)), Change::Appended(1));
    assert_eq!(replica.entries().len(), 2);
    assert_eq!(
        replica.apply(&entry_frame(5, &message("user", "late"))),
        Change::Gap {
            expected: 2,
            received: 5
        }
    );
    assert_eq!(replica.entries().len(), 2);
}

#[test]
fn partial_snapshot_does_not_replace_a_complete_replica() {
    let mut replica = Replica::default();
    for frame in snapshot_frames(&[message("user", "old")]) {
        replica.apply(&frame);
    }
    // A reconnect starts a new welcome; its first chunk is not final yet.
    replica.apply(&json!({"t": "welcome", "proto": 1002, "readOnly": true}));
    let partial =
        json!({"t": "snapshot-chunk", "entries": [message("user", "new")], "final": false});
    assert_eq!(replica.apply(&partial), Change::None);
    assert_eq!(replica.entries(), &[message("user", "old")]);
    let last = json!({"t": "snapshot-chunk", "entries": [], "final": true});
    assert_eq!(replica.apply(&last), Change::Snapshot(1));
    assert_eq!(replica.entries(), &[message("user", "new")]);
}

#[test]
fn reset_clears_the_log_and_replay_rebuilds_it() {
    let mut replica = Replica::default();
    for frame in snapshot_frames(&[message("user", "a"), message("assistant", "b")]) {
        replica.apply(&frame);
    }
    assert_eq!(replica.apply(&json!({"t": "reset"})), Change::Reset);
    assert!(replica.entries().is_empty());
    assert_eq!(
        replica.apply(&entry_frame(0, &message("user", "a2"))),
        Change::Appended(0)
    );
    assert_eq!(replica.entries(), &[message("user", "a2")]);
}

#[test]
fn entries_before_the_first_snapshot_are_left_to_the_snapshot() {
    let mut replica = Replica::default();
    assert_eq!(
        replica.apply(&entry_frame(0, &message("user", "early"))),
        Change::None
    );
    assert!(replica.entries().is_empty());
}

#[test]
fn describe_collapses_text_and_bounds_length() {
    let parts = json!({"role": "assistant", "content": [{"type": "text", "text": "a  b\nc"}]});
    assert_eq!(describe(&parts), "assistant: a b c");
    let long = json!({"role": "user", "content": "x".repeat(PREVIEW_CHARS + 5)});
    assert!(describe(&long).ends_with('…'));
    assert_eq!(describe(&json!({"role": "tool"})), "tool: (no text)");
}
