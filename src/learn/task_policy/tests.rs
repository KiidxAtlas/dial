use super::*;
#[test]
fn malformed_edit_range_gets_syntax_repair_not_another_read() {
    let hint = recovery_hint(
        "edit",
        "line 2: range end: expected a line number, got \"\"",
    )
    .unwrap();
    assert!(hint.contains("PUT N.=N:"));
    assert!(hint.contains("PUT >N:"));
    assert!(hint.contains("not the file"));
    assert!(!hint.contains("refresh the exact edit region"));
    assert!(
        recovery_hint("edit", "stale tag")
            .unwrap()
            .contains("refresh the exact edit region")
    );
    assert!(
        recovery_hint("read", "no such file")
            .unwrap()
            .contains("verify the path")
    );
}

#[test]
fn recovery_guidance_distinguishes_selector_environment_and_permissions() {
    assert!(
        recovery_hint("read", "Invalid line selector")
            .unwrap()
            .contains("numeric line selector")
    );
    assert!(
        recovery_hint("bash", "No module named pytest")
            .unwrap()
            .contains("environment/runner")
    );
    assert!(
        recovery_hint("bash", "permission denied")
            .unwrap()
            .contains("do not bypass")
    );
    assert!(
        recovery_hint("bash", "timed out")
            .unwrap()
            .contains("Do not start duplicate")
    );
    assert!(
        !recovery_hint("bash", "AssertionError: answer incorrect")
            .unwrap()
            .contains("environment/runner")
    );
}

#[test]
fn mechanical_precondition_errors_get_no_debugging_checkpoint() {
    for (tool, error) in [
        (
            "edit",
            "the last `PUT ...:` operation needs at least one `+` row",
        ),
        (
            "edit",
            "line 4: a `PUT ...:` operation needs at least one `+` row (a blank line is a lone `+`)",
        ),
        (
            "edit",
            "empty patch: expected at least one [PATH#TAG] section",
        ),
        (
            "codebase_memory",
            "supporting source `src/a.rs` changed; refresh and inspect it before saving this finding",
        ),
        (
            "todo",
            "Step not closed: the last focused checks failed. Repair the failure before closing.",
        ),
        (
            "todo",
            "reason is 1400 chars; the limit is 1000. Shorten it",
        ),
        (
            "grep",
            "invalid tool call: unknown or unavailable tool: grep",
        ),
        (
            "read",
            "invalid tool call: tool arguments were not a valid JSON object",
        ),
        ("browser", "invalid artifact identifier"),
    ] {
        assert_eq!(recovery_hint(tool, error), None, "{tool}: {error}");
    }
}

#[test]
fn hypothesis_checkpoint_is_kept_for_failed_runs() {
    for (tool, error) in [
        ("bash", "error: test failed, to rerun pass `--test agent`"),
        ("bg", "process exited with status 101"),
        ("eval", "AssertionError: expected 3, got 4"),
    ] {
        assert!(
            recovery_hint(tool, error)
                .is_some_and(|hint| hint.contains("what the failure disproves")),
            "{tool}: {error}"
        );
    }
    assert!(
        recovery_hint("read", "path \"/etc/x\" is outside the workspace")
            .unwrap()
            .contains("boundary")
    );
}

#[test]
fn lightweight_policy_is_conservative_for_complex_and_debugging_work() {
    assert!(Policy::for_prompt("Read src/router.rs", 60).lightweight);
    assert!(!Policy::for_prompt("debug a crash", 60).lightweight);
    assert!(!Policy::for_prompt("migrate across multiple modules", 60).lightweight);
    assert!(!Policy::for_prompt(&"detail ".repeat(100), 60).lightweight);
}
#[test]
fn bounded_policy_preserves_operator_limits_and_allows_complex_investigation() {
    assert_eq!(Policy::for_prompt("hello", 5).initial_reasoning_secs, 5);
    assert_eq!(Policy::for_prompt("hello", 60).initial_reasoning_secs, 10);
    assert_eq!(
        Policy::for_prompt("review architecture across modules", 60).initial_reasoning_secs,
        60
    );
    assert_eq!(Kind::classify("prefixing values"), Kind::General);
}
