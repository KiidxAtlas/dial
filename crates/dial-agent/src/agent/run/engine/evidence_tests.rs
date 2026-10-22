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
    let mut first = output("alpha body");
    assert!(
        engine
            .settle(
                &workspace,
                &read("first", "a.txt"),
                &mut first,
                false,
                false,
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
    engine.settle(&workspace, &edit_call, &mut edit, false, false, |_| true);
    let mut again = output("alpha body");
    let (duplicate, _, _) = engine.settle(
        &workspace,
        &read("second", "a.txt"),
        &mut again,
        false,
        false,
        |_| true,
    );
    let pointer = duplicate.expect("an identical re-read is answered with a pointer");
    assert!(investigation::is_unchanged_read(&pointer));
    // The output keeps the real result, so its artifact and evidence describe what was returned.
    assert_eq!(again.text, "alpha body");
}

#[test]
fn compaction_releases_withheld_repeats_so_the_same_read_runs_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut engine = Engine::default();
    let mut first = output("alpha body");
    engine.settle(
        &workspace,
        &read("first", "a.txt"),
        &mut first,
        false,
        false,
        |_| true,
    );
    let mut repeat = output("alpha body");
    let (withheld, _, _) = engine.settle(
        &workspace,
        &read("second", "a.txt"),
        &mut repeat,
        false,
        false,
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
    let mut fresh = output("alpha body");
    let (duplicate, repeat_advice, stop) = engine.settle(
        &workspace,
        &read("third", "a.txt"),
        &mut fresh,
        false,
        false,
        |_| false,
    );
    assert!(duplicate.is_none() && repeat_advice.is_none() && stop.is_none());
    assert_eq!(fresh.text, "alpha body");
}

#[test]
fn repeated_reads_prefer_the_short_pointer_and_keep_recovery_enforcement() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut engine = Engine::default();
    for (id, duplicate) in [("first", false), ("second", true)] {
        let mut result = output("alpha body");
        let (pointer, _, _) = engine.settle(
            &workspace,
            &read(id, "a.txt"),
            &mut result,
            false,
            false,
            |_| true,
        );
        assert_eq!(
            pointer
                .as_deref()
                .is_some_and(investigation::is_unchanged_read),
            duplicate
        );
        assert_eq!(result.text, "alpha body");
    }
    assert!(
        engine
            .rejection_at(&workspace, &read("third", "a.txt"))
            .is_some()
    );
}
