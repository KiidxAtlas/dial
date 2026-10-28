use super::*;
use crate::storage::session::SessionMeta;
use serde_json::json;

fn seeded_session(layout: &Layout) -> Session {
    Session::create(
        layout,
        SessionMeta {
            model: "test-model".into(),
        },
    )
    .unwrap()
}

#[test]
fn dump_and_transcript_write_files_for_a_seeded_session() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let session = seeded_session(&layout);

    let dumped = dump_text(&layout, &session).unwrap();
    assert!(dumped.starts_with("Archive: "), "{dumped}");
    assert!(dumped.contains("Review before sharing"));

    let transcript = transcript_text(&layout, &session).unwrap();
    let path = transcript.strip_prefix("Transcript: ").unwrap();
    assert!(Path::new(path).exists());
    assert!(path.ends_with(".txt"));
    fs::remove_file(path).unwrap();

    let artifacts = artifacts_text(&session).unwrap();
    assert!(artifacts.contains("artifacts"), "{artifacts}");
}

#[test]
fn memory_lines_show_backend_counts_and_recall_cache() {
    let status = json!({
        "backend": "semantic",
        "count": 3,
        "userFacts": 1,
        "forgotten": 0,
        "embedding": "off",
        "autoRetain": true,
        "recall": {
            "cache": {
                "project": {"entries": 1, "hits": 2, "misses": 1},
                "user": {"entries": 0, "hits": 0, "misses": 0}
            },
            "last": {
                "at": 5,
                "mode": "lexical",
                "items": 2,
                "projectCache": "hit",
                "userCache": "skipped"
            }
        }
    });
    let text = memory_lines(&status);
    assert!(
        text.starts_with(
            "Backend: semantic\nFacts: 3\nUser facts: 1\nForgotten: 0\nEmbedding: off\nAuto-retain: on"
        ),
        "{text}"
    );
    assert!(
        text.contains("Recall cache project: 2 hits, 1 misses, 1 entries"),
        "{text}"
    );
    assert!(
        text.contains(
            "Last recall: lexical mode, 2 item(s), project cache hit, user cache skipped"
        ),
        "{text}"
    );
}

#[test]
fn memory_lines_leave_out_fields_a_backend_does_not_report() {
    assert_eq!(
        memory_lines(&json!({"backend": "provenance"})),
        "Backend: provenance"
    );
}
