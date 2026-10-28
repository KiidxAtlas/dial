use super::*;
use crate::agent::workers::child_transcript::ChildTranscript;
use serde_json::Value;
use std::path::Path;

fn layout(root: &Path) -> Layout {
    Layout::with_user_dir(root, root.join("home")).unwrap()
}

/// Writes one transcript; `close` is the final status, or `None` for a run that never closed.
fn worker(layout: &Layout, parent: &str, id: &str, close: Option<Value>) {
    let sink = ChildTranscript::create(layout, parent, id, json!({"id": id})).unwrap();
    if let Some(status) = close {
        sink.finish(status).unwrap();
    }
}

#[test]
fn only_workers_left_interrupted_are_restore_candidates() {
    let project = tempfile::tempdir().unwrap();
    let layout = layout(project.path());
    // Crashed before closing: no status record.
    worker(&layout, "parent", "task-1", None);
    // Persisted as interrupted: known not replayed, so it may be rebound.
    worker(
        &layout,
        "parent",
        "task-2",
        Some(json!({"status": "interrupted"})),
    );
    // Terminal outcomes never rerun.
    worker(&layout, "parent", "task-3", Some(json!({"status": "done"})));
    worker(
        &layout,
        "parent",
        "task-4",
        Some(json!({"status": "failed"})),
    );
    worker(
        &layout,
        "parent",
        "task-5",
        Some(json!({"status": "cancelled"})),
    );

    let mut found = interrupted_workers(&layout, "parent");
    found.sort();
    assert_eq!(found, ["task-1", "task-2"]);
}

#[test]
fn a_parent_without_workers_restores_nothing() {
    let project = tempfile::tempdir().unwrap();
    let layout = layout(project.path());
    assert!(interrupted_workers(&layout, "parent").is_empty());
    assert!(interrupted_workers(&layout, "other").is_empty());
}
