use super::render;
use serde_json::json;

#[test]
fn plain_answer_has_no_report() {
    let summary = json!({"outcome": "completed", "changed_files": [], "recorded_checks": [], "open_tasks": 0});
    assert_eq!(render(&summary), None);
}

#[test]
fn missing_summary_has_no_report() {
    assert_eq!(render(&serde_json::Value::Null), None);
}

#[test]
fn completed_edit_reports_files_checks_and_undo() {
    let summary = json!({
        "outcome": "completed",
        "changed_files": ["src/a.rs"],
        "recorded_checks": [{"name": "cargo test", "status": "passed"}],
        "checks_current": true,
        "open_tasks": 0,
        "uncheckpointed_shell_commands": 0,
        "uncheckpointed_extension_calls": 0
    });
    let text = render(&summary).expect("edits produce a report");
    assert!(text.contains("Changed: 1 file(s): src/a.rs"), "{text}");
    assert!(text.contains("Checks: cargo test passed"), "{text}");
    assert!(text.contains("Undo: /undo"), "{text}");
    assert!(
        text.contains("Not reversible: no shell or extension effects recorded."),
        "{text}"
    );
    assert!(!text.contains("Resume:"), "{text}");
}

#[test]
fn interrupted_turn_names_unreversible_effects_and_resume() {
    let summary = json!({
        "outcome": "interrupted",
        "changed_files": [],
        "recorded_checks": [],
        "checks_current": false,
        "open_tasks": 2,
        "uncheckpointed_shell_commands": 1,
        "uncheckpointed_extension_calls": 0
    });
    let text = render(&summary).expect("interruptions produce a report");
    assert!(text.contains("Outcome: interrupted"), "{text}");
    assert!(text.contains("Changed: no files"), "{text}");
    assert!(text.contains("Checks: none run"), "{text}");
    assert!(text.contains("Open tasks: 2 not finished"), "{text}");
    assert!(
        text.contains("Not reversible: 1 shell command(s) and 0 extension call(s)"),
        "{text}"
    );
    assert!(text.contains("Resume:"), "{text}");
}

#[test]
fn stale_checks_and_long_file_lists_are_marked() {
    let files: Vec<String> = (0..10).map(|i| format!("f{i}.rs")).collect();
    let summary = json!({
        "outcome": "completed",
        "changed_files": files,
        "recorded_checks": [{"name": "lint", "status": "failed"}],
        "checks_current": false,
        "open_tasks": 0
    });
    let text = render(&summary).expect("edits produce a report");
    assert!(text.contains("Changed: 10 file(s): f0.rs, f1.rs"), "{text}");
    assert!(text.contains("and 2 more"), "{text}");
    assert!(
        text.contains("Checks: lint failed; not current with the final edits"),
        "{text}"
    );
}
