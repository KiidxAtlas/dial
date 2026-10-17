use super::*;
use serde_json::json;

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "first-read".into(),
        name: name.into(),
        arguments: args,
        malformed_arguments: false,
    }
}

#[test]
fn unchanged_reads_redirect_but_changed_results_and_mutations_do_not() {
    let mut guard = InvestigationGuard::default();
    let read = call("read", json!({"path":"src/main.rs", "i":"inspect"}));
    assert!(guard.observe(&read, "old", false).is_none());
    let again = call("read", json!({"path":"src/main.rs", "i":"inspect again"}));
    assert!(
        guard
            .observe(&again, "old", false)
            .unwrap()
            .contains("first-read")
    );
    assert!(guard.observe(&again, "new", false).is_none());
    assert!(guard.observe(&again, "new", true).is_none());
    guard.observe_output(
        &call("edit", json!({})),
        &crate::runtime::tool_output::ToolOutput::structured(json!({}))
            .with_changes(["changed.rs".into()]),
    );
    assert!(guard.observe(&again, "new", false).is_none());
}

#[test]
fn repeated_successful_shell_checks_escalate_even_when_timeout_and_artifact_change() {
    let mut guard = InvestigationGuard::default();
    let command =
        "cd project && grep '^version' pyproject.toml && grep '__version__' src/app/__init__.py";
    for attempt in 0..5 {
        let check = call("bash", json!({"command":command, "timeout":30+attempt}));
        let text = format!(
            "version = \"0.3.31\"\n__version__ = \"0.3.31\"\n[exit code: 0]\n[complete output: artifact://{attempt}]\n"
        );
        assert_eq!(guard.observe(&check, &text, false).is_some(), attempt > 0);
        assert_eq!(guard.escalation().is_some(), attempt == 4);
    }
}

#[test]
fn shell_checks_allow_new_results_different_commands_and_verification_after_edits() {
    let mut guard = InvestigationGuard::default();
    let check = call("bash", json!({"command":"cat version.txt"}));
    let old = "0.3.31\n[exit code: 0]\n";
    assert!(guard.observe(&check, old, false).is_none());
    assert!(guard.observe(&check, old, true).is_none());
    assert!(guard.observe(&check, old, false).is_some());
    assert!(
        guard
            .observe(&check, "0.3.32\n[exit code: 0]\n", false)
            .is_none()
    );
    assert!(
        guard
            .observe(
                &call("bash", json!({"command":"cat other-version.txt"})),
                old,
                false
            )
            .is_none()
    );
    guard.observe_output(
        &call("edit", json!({"path":"version.txt"})),
        &crate::runtime::tool_output::ToolOutput::structured(json!({}))
            .with_changes(["version.txt".into()]),
    );
    assert!(guard.observe(&check, old, false).is_none());
}

#[test]
fn independent_ranges_and_recovery_are_allowed_and_storage_is_bounded() {
    let mut guard = InvestigationGuard::default();
    for line in 0..200 {
        assert!(
            guard
                .observe(
                    &call("read", json!({"path":"x", "offset":line})),
                    "same",
                    false
                )
                .is_none()
        );
    }
    assert_eq!(guard.observations.len(), 128);
    for _ in 0..3 {
        assert!(
            guard
                .observe(
                    &call("artifact_read", json!({"uri":"artifact:x"})),
                    "same",
                    false
                )
                .is_none()
        );
    }
}

#[test]
fn repeated_evidence_escalates_but_new_evidence_rearms() {
    let mut guard = InvestigationGuard::default();
    let read = call("read", json!({"path":"a.rs"}));
    guard.observe(&read, "original", false);
    for _ in 0..3 {
        guard.observe(&read, "original", false);
        assert!(guard.escalation().is_none());
        guard.observe(&call("todo", json!({"op":"view"})), "plan", false);
    }
    guard.observe(&read, "original", false);
    assert!(guard.escalation().is_some());
    guard.observe(&read, "changed externally", false);
    assert!(guard.escalation().is_none());
}

#[test]
fn intervening_checks_errors_and_other_reads_do_not_hide_a_loop() {
    let mut guard = InvestigationGuard::default();
    let read = call("read", json!({"path":"src/steering.rs"}));
    guard.observe(&read, "pub(crate) fn local_unsupported_reason", false);
    for attempt in 0..4 {
        guard.observe(
            &call("bash", json!({"command":"cargo check"})),
            "subagents.rs: compile error",
            true,
        );
        guard.observe(
            &call("bash", json!({"command":"git status"})),
            "unchanged",
            false,
        );
        guard.observe(
            &call("read", json!({"path":format!("other-{attempt}.rs")})),
            "other evidence",
            false,
        );
        assert!(
            guard
                .observe(&read, "pub(crate) fn local_unsupported_reason", false)
                .is_some()
        );
    }
    assert!(guard.escalation().is_some());
    assert!(guard.observe(&read, "changed externally", false).is_none());
    assert!(guard.escalation().is_none());
}

#[test]
fn equivalent_anchored_reads_share_evidence_but_new_ranges_and_snapshots_do_not() {
    let mut guard = InvestigationGuard::default();
    let original = call("read", json!({"path":"a.rs:1-2"}));
    let text = "[a.rs#AB12]\n1:one\n2:two\n[lines 1-2 of 3]\n";
    assert!(guard.observe(&original, text, false).is_none());
    for args in [
        json!({"path":"a.rs", "offset":1, "limit":2}),
        json!({"path":"a.rs:1+2"}),
        json!({"path":"./a.rs:1-2"}),
        json!({"path":"a.rs:1-2", "limit":50}),
    ] {
        assert!(guard.observe(&call("read", args), text, false).is_some());
    }
    assert!(guard.escalation().is_some());
    assert!(
        guard
            .observe(
                &call("read", json!({"path":"a.rs", "offset":1, "limit":2})),
                "[a.rs#AB12]\n1:one\n2:two\n",
                false
            )
            .is_some()
    );
    assert!(
        guard
            .observe(&original, "[a.rs#AB12]\n3:three\n[lines 3 of 3]\n", false)
            .is_none()
    );
    assert!(guard.escalation().is_none());
    assert!(
        guard
            .observe(
                &original,
                "[a.rs#CD34]\n1:changed\n2:two\n[lines 1-2 of 3]\n",
                false
            )
            .is_none()
    );
    assert!(
        guard
            .observe(
                &call("read", json!({"path":"b.rs:1-2"})),
                "[b.rs#AB12]\n1:one\n2:two\n[lines 1-2 of 3]\n",
                false
            )
            .is_none()
    );
}

#[test]
fn no_op_edits_and_todo_polling_do_not_erase_version_check_loops() {
    let mut guard = InvestigationGuard::default();
    let check = call("bash", json!({"command":"grep '^version' pyproject.toml"}));
    for attempt in 0..5 {
        assert_eq!(
            guard.observe(&check, "version = 0.3.31", false).is_some(),
            attempt > 0
        );
        guard.observe(&call("todo", json!({"op":"view"})), "unfinished", false);
        guard.observe(
            &call("edit", json!({})),
            "No changes: the edit leaves the file identical. Nothing was written.",
            false,
        );
        guard.observe(
            &call("write", json!({})),
            "change preview (+0 -0 lines)",
            false,
        );
        assert_eq!(guard.escalation().is_some(), attempt == 4);
    }
    guard.observe_output(
        &call("edit", json!({})),
        &crate::runtime::tool_output::ToolOutput::structured(json!({}))
            .with_changes(["changed.rs".into()]),
    );
    assert!(guard.escalation().is_none());
    assert!(guard.observe(&check, "version = 0.3.31", false).is_none());
}

#[test]
fn unchanged_read_gets_pointer_only_while_earlier_result_is_full() {
    let mut returned = ReturnedReads::default();
    let read = call("read", json!({"path":"src/lib.rs"}));
    assert!(
        returned
            .pointer(&read, "1: fn a() {}", false, |_| true)
            .is_none()
    );
    assert!(
        returned
            .pointer(&read, "1: fn a() {}", false, |_| false)
            .is_none()
    );
    assert!(
        returned
            .pointer(&read, "1: fn a() {}", false, |_| true)
            .is_some()
    );
    assert!(
        returned
            .pointer(&read, "1: fn b() {}", false, |_| true)
            .is_none()
    );
}

fn rows(tag: &str, lines: std::ops::RangeInclusive<u32>) -> String {
    let mut text = format!("[src/lib.rs#{tag}]\n[Source raw SHA256:00]\n");
    for line in lines {
        text.push_str(&format!("{line}: code {line}\n"));
    }
    text
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        ..call("read", json!({ "path": path }))
    }
}

#[test]
fn a_read_inside_lines_already_shown_gets_a_pointer_to_that_call() {
    let mut returned = ReturnedReads::default();
    let whole = read_call("whole", "src/lib.rs");
    assert!(
        returned
            .pointer(&whole, &rows("AAAA", 1..=100), false, |_| true)
            .is_none()
    );
    let part = read_call("part", "src/lib.rs:40-60");
    let pointer = returned
        .pointer(&part, &rows("AAAA", 40..=60), false, |_| true)
        .expect("contained read is answered by a pointer");
    assert!(is_unchanged_read(&pointer) && pointer.contains("whole"));
}

#[test]
fn a_read_is_sent_in_full_when_it_adds_lines_the_file_changed_or_the_earlier_result_is_gone() {
    let mut returned = ReturnedReads::default();
    let whole = read_call("whole", "src/lib.rs:1-100");
    returned.pointer(&whole, &rows("AAAA", 1..=100), false, |_| true);
    let send = |returned: &mut ReturnedReads, id, text: String, full: bool| {
        returned.pointer(&read_call(id, id), &text, false, |_| full)
    };
    // One line past the earlier result.
    assert!(send(&mut returned, "past", rows("AAAA", 90..=101), true).is_none());
    // Same lines but the file changed since (new tag).
    assert!(send(&mut returned, "edited", rows("BBBB", 40..=60), true).is_none());
    // The earlier result is no longer shown in full.
    assert!(send(&mut returned, "pruned", rows("AAAA", 40..=60), false).is_none());
    // A result with no source rows is never replaced.
    assert!(send(&mut returned, "raw", "plain text".into(), true).is_none());
}

#[test]
fn multi_path_reads_are_replaced_only_when_every_section_is_covered() {
    let mut returned = ReturnedReads::default();
    let first = read_call("first", "a");
    let both = format!(
        "{}[src/main.rs#CCCC]\n1: fn main() {{}}\n",
        rows("AAAA", 1..=50)
    );
    assert!(returned.pointer(&first, &both, false, |_| true).is_none());
    let again = format!(
        "{}[src/main.rs#CCCC]\n1: fn main() {{}}\n",
        rows("AAAA", 5..=9)
    );
    assert!(
        returned
            .pointer(&read_call("second", "b"), &again, false, |_| true)
            .is_some()
    );
    let extra = format!("{}[src/new.rs#DDDD]\n1: new\n", rows("AAAA", 5..=9));
    assert!(
        returned
            .pointer(&read_call("third", "c"), &extra, false, |_| true)
            .is_none()
    );
}
