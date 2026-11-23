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

#[test]
fn only_an_explicit_verify_word_is_a_verification_request() {
    for prompt in [
        "Fix amount_due and verify its public behavior.",
        "Fix the bug. Verified behavior matters.",
        "fix it; include verification",
    ] {
        assert!(
            Policy::for_prompt(prompt, 60).verification_requested,
            "{prompt}"
        );
    }
    for prompt in [
        "Fix the arithmetic bug in calc.add; check the result.",
        "Fix the failing test",
        "the claim is unverifiable",
    ] {
        assert!(
            !Policy::for_prompt(prompt, 60).verification_requested,
            "{prompt}"
        );
    }
}

#[test]
fn diagnosis_only_requests_ask_why_without_asking_for_a_repair() {
    for prompt in [
        "Run tests/smoke.sh and tell me why it fails",
        "Diagnose the failing build",
        "Explain the cause of this crash",
    ] {
        assert!(diagnosis_only(prompt), "{prompt}");
    }
    for prompt in [
        "Run tests/smoke.sh and fix why it fails",
        "Why does this fail? Make it pass.",
        "Run the build",
        "Add a flag",
    ] {
        assert!(!diagnosis_only(prompt), "{prompt}");
    }
}

#[test]
fn negated_repair_words_request_no_repair() {
    for prompt in [
        "Don't fix it, just tell me why it fails",
        "Do not change anything; explain why the build fails",
        "Without changing the code, explain why the test fails",
        "Run the suite, stop editing, and diagnose the failure",
        "Investigate why it fails and do not make any changes",
    ] {
        assert!(diagnosis_only(prompt), "{prompt}");
    }
}

#[test]
fn repair_and_edit_nouns_name_the_subject_not_the_request() {
    for prompt in [
        "Explain why the fix for the parser fails",
        "Explain the update to the parser that breaks in CI",
        "Investigate why the make target fails",
        "Explain why the build fails after you change the config",
        "Give me a diagnosis, not a fix",
        "Explain why `fix --test` fails",
        "Explain why `make` fails, but do not touch the code",
    ] {
        assert!(diagnosis_only(prompt), "{prompt}");
    }
}

#[test]
fn repair_in_request_position_is_not_diagnosis_only() {
    for prompt in [
        "Don't just explain it, fix it",
        "Tell me why the test fails, and make it pass",
        "Explain why it fails; do not explain anything, change the parser",
        "Explain why it fails and fix it",
        "Explain why it fails. Make it pass.",
        "Explain why it fails and add a regression test",
        "Explain why it fails, then rename the helper",
    ] {
        assert!(!diagnosis_only(prompt), "{prompt}");
    }
}

#[test]
fn kind_follows_requested_outcome_not_subject_nouns() {
    for (prompt, kind) in [
        ("Make the change to the parser", Kind::Editing),
        ("Add a flag", Kind::Editing),
        (
            "Don't change anything, explain why the build fails",
            Kind::Investigation,
        ),
        ("Explain the fix for the parser", Kind::Investigation),
        ("Read the change log and summarise", Kind::Investigation),
        ("Don't fix the bug", Kind::Debugging),
        ("Fixing the crash now", Kind::Debugging),
        ("Don't edit anything", Kind::General),
    ] {
        assert_eq!(Kind::classify(prompt), kind, "{prompt}");
    }
}
