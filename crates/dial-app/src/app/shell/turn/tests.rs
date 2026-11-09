use super::*;
use crate::app::shell::testkit::shell;
use serde_json::json;

#[test]
fn failed_turn_footer_keeps_a_fresh_summary_and_points_transport_errors_at_retry() {
    let recorded = json!({
        "outcome": "failed",
        "changed_files": ["src/lib.rs"],
        "open_tasks": 0,
        "error_class": "transient",
    });
    // (fresh summary, error class, keeps changed files, suggests /retry)
    let cases = [
        (None, "transient", false, true),
        (None, "rate_limited", false, true),
        (None, "fatal", false, false),
        (Some(&recorded), "transient", true, true),
        (Some(&recorded), "auth", true, false),
    ];
    for (fresh, class, keeps, retry) in cases {
        let outcome = failed_turn_outcome(fresh, &Value::Null, class);
        assert_eq!(outcome.status, "failed", "{class}");
        assert_eq!(outcome.changed_files == 1, keeps, "{class}");
        assert_eq!(
            outcome.handoff.iter().any(|line| line.contains("/retry")),
            retry,
            "{class}: {:?}",
            outcome.handoff
        );
        assert_eq!(
            outcome
                .handoff
                .iter()
                .any(|line| line.contains("Full handoff unavailable")),
            fresh.is_none(),
            "{class}"
        );
        assert!(
            outcome
                .handoff
                .iter()
                .filter(|line| line.starts_with("Next:"))
                .count()
                <= 1
        );
    }
}

#[test]
fn unsent_message_returns_to_the_composer_ahead_of_new_typing() {
    let (_home, _project, _app, mut shell) = shell();
    shell.restore_unsent("fix the bug", true);
    assert_eq!(shell.composer.draft(), "fix the bug");
    // Restoring twice never duplicates the message.
    shell.restore_unsent("fix the bug", true);
    assert_eq!(shell.composer.draft(), "fix the bug");
    shell.composer.set_draft("and add a test");
    shell.restore_unsent("fix the bug", false);
    assert_eq!(shell.composer.draft(), "fix the bug\n\nand add a test");
}
