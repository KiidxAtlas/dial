use super::*;
use serde_json::json;

fn read(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: json!({ "path": path }),
        malformed_arguments: false,
    }
}

fn output(text: &str) -> ToolOutput {
    ToolOutput {
        text: text.into(),
        is_error: false,
        data: None,
        execution: Default::default(),
    }
}

#[test]
fn an_identical_reread_after_an_unrelated_edit_gets_a_pointer_without_altering_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
    let mut engine = Engine::default();
    let mut first = output(&"alpha body ".repeat(32));
    assert!(
        engine
            .settle(
                &workspace,
                &read("first", "a.txt"),
                &mut first,
                Default::default(),
                |_| true
            )
            .0
            .is_none()
    );
    // An edit to another file clears the observation memory; the first read stays shown.
    let mut edit = output("edited");
    edit.execution.changed_paths = vec!["b.txt".into()];
    let edit_call = ToolCall {
        id: "edit".into(),
        name: "write".into(),
        arguments: json!({ "path": "b.txt" }),
        malformed_arguments: false,
    };
    engine.settle(
        &workspace,
        &edit_call,
        &mut edit,
        Default::default(),
        |_| true,
    );
    let mut again = output(&"alpha body ".repeat(32));
    let (duplicate, _, _) = engine.settle(
        &workspace,
        &read("second", "a.txt"),
        &mut again,
        Default::default(),
        |_| true,
    );
    let pointer = duplicate.expect("an identical re-read is answered with a pointer");
    assert!(investigation::is_unchanged_read(&pointer));
    // The output keeps the real result, so its artifact and evidence describe what was returned.
    assert_eq!(again.text, "alpha body ".repeat(32));
}

#[test]
fn compaction_releases_withheld_repeats_so_the_same_read_runs_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut engine = Engine::default();
    let mut first = output(&"alpha body ".repeat(32));
    engine.settle(
        &workspace,
        &read("first", "a.txt"),
        &mut first,
        Default::default(),
        |_| true,
    );
    let mut repeat = output(&"alpha body ".repeat(32));
    let (withheld, _, _) = engine.settle(
        &workspace,
        &read("second", "a.txt"),
        &mut repeat,
        Default::default(),
        |_| true,
    );
    assert!(withheld.is_some_and(|advice| advice.contains("Reuse")));
    assert!(
        engine
            .rejection_at(&workspace, &read("third", "a.txt"))
            .is_some()
    );

    // Compaction removed the earlier result: nothing may withhold or cite it any more.
    engine.evidence_compacted();
    assert!(
        engine
            .rejection_at(&workspace, &read("third", "a.txt"))
            .is_none()
    );
    let mut fresh = output(&"alpha body ".repeat(32));
    let (duplicate, repeat_advice, stop) = engine.settle(
        &workspace,
        &read("third", "a.txt"),
        &mut fresh,
        Default::default(),
        |_| false,
    );
    assert!(duplicate.is_none() && repeat_advice.is_none() && stop.is_none());
    assert_eq!(fresh.text, "alpha body ".repeat(32));
}

#[test]
fn repeated_reads_prefer_the_short_pointer_and_keep_recovery_enforcement() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut engine = Engine::default();
    for (id, duplicate) in [("first", false), ("second", true)] {
        let mut result = output(&"alpha body ".repeat(32));
        let (pointer, _, _) = engine.settle(
            &workspace,
            &read(id, "a.txt"),
            &mut result,
            Default::default(),
            |_| true,
        );
        assert_eq!(
            pointer
                .as_deref()
                .is_some_and(investigation::is_unchanged_read),
            duplicate
        );
        assert_eq!(result.text, "alpha body ".repeat(32));
    }
    assert!(
        engine
            .rejection_at(&workspace, &read("third", "a.txt"))
            .is_some()
    );
}

#[test]
fn visible_edit_rows_replace_only_the_matching_fresh_read_and_preserve_raw_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut engine = Engine::default();
    let source = format!(
        "[a.rs#AB12]\n[Source raw SHA256:00]\n1:{}\n",
        "fresh source ".repeat(50)
    );
    let edit = ToolCall {
        id: "edit-result".into(),
        name: "edit".into(),
        arguments: json!({}),
        malformed_arguments: false,
    };
    let mut edited = output(&source);
    edited.execution.changed_paths = vec!["a.rs".into()];
    engine.settle(&workspace, &edit, &mut edited, Default::default(), |_| true);
    let mut reread = output(&source);
    let (pointer, _, _) = engine.settle(
        &workspace,
        &read("reread", "a.rs"),
        &mut reread,
        Default::default(),
        |id| id == "edit-result",
    );
    let pointer = pointer.expect("the successful fresh read reuses visible edit rows");
    assert!(investigation::is_unchanged_read(&pointer) && pointer.contains("edit-result"));
    assert!(pointer.len() < source.len());
    assert_eq!(reread.text, source);

    engine.evidence_compacted();
    let mut fresh = output(&source);
    let (pointer, _, _) = engine.settle(
        &workspace,
        &read("after-compact", "a.rs"),
        &mut fresh,
        Default::default(),
        |_| false,
    );
    assert!(pointer.is_none());
    assert_eq!(fresh.text, source);
}

#[test]
fn tiny_duplicate_reads_keep_the_source_without_resetting_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut engine = Engine::default();
    for id in ["first", "second"] {
        let mut result = output("alpha body");
        let (replacement, _, _) = engine.settle(
            &workspace,
            &read(id, "a.txt"),
            &mut result,
            Default::default(),
            |_| true,
        );
        assert!(
            replacement.is_none(),
            "short source must not become longer coaching"
        );
        assert_eq!(result.text, "alpha body");
        let progress = engine.take_round_progress();
        assert_eq!(
            progress,
            if id == "first" {
                RoundProgress::Productive
            } else {
                RoundProgress::Stalled
            }
        );
    }
    assert!(
        engine
            .rejection_at(&workspace, &read("third", "a.txt"))
            .is_some()
    );
}

#[test]
fn pointer_projection_keeps_shorter_handle_source_without_resetting_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut engine = Engine::default();
    let path = format!("{}a.txt", "deep/".repeat(30));
    let source = format!("[{path}#AB12]\n1:x\n");
    let projected = crate::agent::window::file_handles::model_result(
        &workspace,
        &read("first", &path),
        &source,
    );
    assert!(projected.len() < 32);
    for id in ["first", "second"] {
        let mut result = output(&source);
        let (replacement, _, _) = engine.settle(
            &workspace,
            &read(id, &path),
            &mut result,
            Default::default(),
            |_| true,
        );
        assert!(
            replacement.is_none(),
            "pointer must not expand compact source"
        );
        assert_eq!(result.text, source);
        if id == "second" {
            assert_eq!(engine.take_round_progress(), RoundProgress::Stalled);
        } else {
            engine.take_round_progress();
        }
    }
    assert!(
        engine
            .rejection_at(&workspace, &read("third", &path))
            .is_some()
    );
}
