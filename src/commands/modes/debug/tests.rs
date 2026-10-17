use super::*;
use crate::storage::session::SessionMeta;

fn seeded_session(layout: &Layout) -> Session {
    Session::create(
        layout,
        SessionMeta {
            model: "test-model".into(),
        },
    )
    .unwrap()
}

fn snapshot(session: &Session) -> DebugSnapshot {
    DebugSnapshot {
        session_id: session.id().to_owned(),
        session_path: session.path().map(Path::to_path_buf),
        messages: session.message_count(),
        branch_leaf: session.branch_leaf().map(str::to_owned),
        version_id: 1,
        version_kind: "Root".into(),
        version_status: "Active".into(),
        backend: "test-backend".into(),
        context_window: 8192,
        context_tokens: 123,
        context_estimated: true,
        responses: 2,
        prompt_tokens: 10,
        output_tokens: 20,
        cached_tokens: 3,
        cache_write_tokens: 4,
        reasoning_tokens: 5,
        compactions: 0,
        turns_completed: 1,
        tool_contract_errors: 0,
        session_cost: None,
        plan_mode: false,
        tools_offered: vec!["read".into(), "bash".into()],
        tools_available: 7,
    }
}

#[test]
fn overview_lists_every_omp_item_and_sections_for_a_seeded_session() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let session = seeded_session(&layout);
    let text = overview(&snapshot(&session));

    assert_eq!(ITEMS.len(), 13);
    for item in ITEMS {
        assert!(text.contains(item.name), "{}", item.name);
    }
    for header in [
        "Available (run with /debug <item>):",
        "\nSession\n",
        "\nModel\n",
        "\nUsage\n",
        "\nSettings\n",
        "\nTools\n",
    ] {
        assert!(text.contains(header), "{header}");
    }
    assert!(text.contains(&format!("id: {}", session.id())));
    assert!(text.contains("Not available in Dial:"));
    assert!(text.contains("Dial has no CPU profiler"));
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

    let transcript = transcript_text(&session).unwrap();
    let path = transcript.strip_prefix("Transcript: ").unwrap();
    assert!(Path::new(path).exists());
    assert!(path.ends_with(".txt"));
    fs::remove_file(path).unwrap();

    let artifacts = artifacts_text(&session).unwrap();
    assert!(artifacts.contains("artifacts"), "{artifacts}");
}
