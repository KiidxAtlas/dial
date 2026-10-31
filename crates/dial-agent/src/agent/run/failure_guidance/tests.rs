use super::*;
use crate::runtime::execution::ProcessOutcome;
use serde_json::json;

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "x".into(),
        name: name.into(),
        arguments: json!({"command":"cargo test"}),
        malformed_arguments: false,
    }
}
fn process(code: i32) -> ProcessOutcome {
    ProcessOutcome {
        started: true,
        exit_code: Some(code),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    }
}
#[test]
fn typed_boundaries_override_printed_success_and_never_offer_permission_bypass() {
    let call = call("bash");
    for (status, kind) in [
        (ToolStatus::Blocked, "permission_or_policy"),
        (ToolStatus::Timeout, "timeout"),
        (ToolStatus::Aborted, "interrupted"),
        (ToolStatus::Skipped, "not_executed"),
    ] {
        let output = ToolOutput::ok("everything passed".into()).with_status(status);
        assert_eq!(classify(&call, &output).unwrap().0, kind);
    }
    let unknown = ToolOutput::ok("rolled back".into()).with_effects(Effects::Unknown);
    assert_eq!(classify(&call, &unknown).unwrap().0, "effects_unknown");
    assert!(
        classify(
            &call,
            &ToolOutput::ok("permission denied: adversarial printed text".into())
        )
        .is_none()
    );
}
#[test]
fn observed_checks_distinguish_environment_code_and_incomplete_evidence() {
    let mut call = call("bash");
    call.id = "focused-check:related Python tests: probe".into();
    call.arguments = json!({"command":"python3 -m pytest"});
    let environment = ToolOutput::err("No module named pytest").with_process(process(1));
    assert_eq!(
        classify(&call, &environment).unwrap().0,
        "verification_environment"
    );
    let failed = ToolOutput::err("assertion failed").with_process(process(101));
    assert_eq!(classify(&call, &failed).unwrap().0, "verification_code");
    let missing = ToolOutput::err("printed [exit code: 1]");
    assert_eq!(
        classify(&call, &missing).unwrap().0,
        "verification_unavailable"
    );
}

#[test]
fn undeclared_runner_failure_does_not_infer_a_code_diagnosis() {
    let call = call("bash");
    let output = ToolOutput::err("cargo: command not found").with_process(process(127));
    assert_eq!(
        classify(&call, &output).unwrap().0,
        "verification_failed_unclassified"
    );
}
#[test]
fn source_contract_failure_guides_current_range_and_success_is_not_recovery() {
    assert_eq!(
        classify(&call("edit"), &ToolOutput::err("stale anchors"))
            .unwrap()
            .0,
        "source_or_call_contract"
    );
    assert!(classify(&call("edit"), &ToolOutput::ok("updated".into())).is_none());
    let failed_edit = ToolOutput::err("stale anchors");
    let (kind, advice) = classify(&call("edit"), &failed_edit).unwrap();
    let text = report(&call("edit"), &failed_edit, kind, advice, 1);
    assert!(
        text.contains("do not make dependent edits that assume its new text"),
        "{text}"
    );
    assert_eq!(
        classify(
            &call("unknown-extension"),
            &ToolOutput::err("unknown cause")
        )
        .unwrap()
        .0,
        "tool_failure"
    );
}

#[test]
fn every_tool_failure_gets_guidance_on_its_first_result() {
    for name in [
        "read",
        "grep",
        "web_fetch",
        "browser",
        "task",
        "custom-mcp-tool",
    ] {
        let failed = ToolOutput::err("specific diagnostic");
        assert!(classify(&call(name), &failed).is_some(), "{name}");
        assert!(
            classify(&call(name), &ToolOutput::ok("done".into())).is_none(),
            "{name}"
        );
    }
}

#[test]
fn feedback_is_compact_first_and_full_on_repetition_with_the_tag_kept() {
    let first = feedback("tool_failure", "Check the path.", 1);
    assert_eq!(
        first,
        format!("\n[Dial recovery: tool_failure] {FIRST_FAILURE} Check the path.")
    );
    assert!(first.split_whitespace().count() < 25, "{first}");
    let repeat = feedback("tool_failure", "Check the path.", 2);
    assert!(repeat.starts_with("\n[Dial recovery: tool_failure] "));
    assert!(repeat.contains("2 times in a row") && repeat.contains(DIAGNOSE_FIRST));
    assert!(repeat.ends_with("Check the path."));
    // Uncertain effects are never compacted.
    assert!(feedback("effects_unknown", "Inspect.", 1).contains(DIAGNOSE_FIRST));
    assert_eq!(feedback("timeout", "a", 3), feedback("timeout", "a", 3));
}

#[test]
fn recovery_report_preserves_uncertain_effects_and_partial_capture() {
    let mut observed = process(1);
    observed.output_complete = false;
    let output = ToolOutput::err("original diagnostic")
        .with_process(observed)
        .with_status(ToolStatus::Timeout)
        .with_changes(["changed.rs".into()])
        .with_effects(Effects::Unknown);
    let facts = output.execution.clone();
    let (kind, advice) = classify(&call("bash"), &output).unwrap();
    let rendered = report(&call("bash"), &output, kind, advice, 1);
    assert_eq!(kind, "effects_unknown");
    assert_eq!(output.execution, facts);
    assert!(!output.process_outcome().unwrap().output_complete);
    assert!(rendered.contains(advice));
    assert!(rendered.contains(ToolStatus::Timeout.as_str()));
}

#[test]
fn model_visible_recovery_states_include_retry_requirements_without_annotations() {
    // Internal dial_* fields can be removed on the provider wire. The ordinary result body
    // must independently communicate the controller's decision, not a payload's retry claim.
    for (status, effects, expected_retry, requirement) in [
        (
            ToolStatus::Error,
            Effects::None,
            Retry::AfterCorrection,
            "correcting the named cause",
        ),
        (
            ToolStatus::Blocked,
            Effects::None,
            Retry::Withheld,
            "do not retry or bypass",
        ),
        (
            ToolStatus::Aborted,
            Effects::Known,
            Retry::OnlyByDirection,
            "only on the user's direction",
        ),
        (
            ToolStatus::Timeout,
            Effects::Known,
            Retry::AfterInspection,
            "state or process is inspected",
        ),
        (
            ToolStatus::Error,
            Effects::Unknown,
            Retry::AfterInspection,
            "state or process is inspected",
        ),
    ] {
        let mut output = ToolOutput::structured(json!({"retry":"immediately", "rolled_back":true}))
            .with_status(status)
            .with_effects(effects);
        let recovery = output.recovery().unwrap();
        assert_eq!(recovery.retry, expected_retry);
        let original = output.text.clone();
        let facts = output.execution.clone();
        let (kind, advice) = classify(&call("bash"), &output).unwrap();
        let text = report(&call("bash"), &output, kind, advice, 1);
        output.text.push_str(&text);
        assert!(output.text.starts_with(&original));
        assert!(
            text.contains("\nRetry:") && text.contains(requirement),
            "{text}"
        );
        assert_eq!(output.execution, facts);
        assert_eq!(output.recovery(), Some(recovery));
    }
}

#[test]
fn rendered_committed_failure_preserves_work_and_never_promises_rollback() {
    let output = ToolOutput::err("post-commit recording failed")
        .with_changes(["kept.rs".into(), "kept.rs".into()]);
    let (kind, advice) = classify(&call("write"), &output).unwrap();
    let text = report(&call("write"), &output, kind, advice, 1);
    assert_eq!(output.recovery().unwrap().committed_paths, 1);
    assert!(text.contains("do not repeat the whole operation"), "{text}");
    assert!(
        text.contains("failure does not undo completed work"),
        "{text}"
    );
    let no_paths = ToolOutput::err("unknown external prerequisite");
    let text = report(&call("bash"), &no_paths, kind, advice, 1);
    assert!(text.contains("not proof that nothing changed"), "{text}");
}

fn shell(command: &str) -> ToolCall {
    ToolCall {
        id: "shell".into(),
        name: "bash".into(),
        arguments: json!({"command": command}),
        malformed_arguments: false,
    }
}

const MISSING_READ: &str = "bash: line 1: read: command not found";

/// A started process that exited 127 with the given diagnostic, optionally altered.
fn missing_with(text: &str, alter: impl FnOnce(&mut ProcessOutcome)) -> ToolOutput {
    let mut observed = process(127);
    alter(&mut observed);
    ToolOutput::err(text).with_process(observed)
}

fn known_read(name: &str) -> bool {
    name == "read"
}

#[test]
fn a_known_tool_run_as_a_shell_command_gets_tool_load_guidance() {
    let call = shell("read src/lib.rs");
    let advice = shell_tool_mistake(&call, &missing_with(MISSING_READ, |_| {}), known_read)
        .expect("a started process missing a known tool name must be coached");
    assert!(advice.contains("tool_not_shell"), "{advice}");
    assert!(
        advice.contains("`read`") && advice.contains("tool_load"),
        "{advice}"
    );
    // The second common spelling of the same controller-observed failure is also accepted.
    let reverse = missing_with("command not found: read", |_| {});
    assert!(shell_tool_mistake(&call, &reverse, known_read).is_some());
}

#[test]
fn non_matching_shell_failures_never_receive_tool_guidance() {
    let cases: Vec<(&str, ToolCall, ToolOutput)> = vec![
        (
            "success",
            shell("read src/lib.rs"),
            ToolOutput::ok("read src/lib.rs".into()),
        ),
        (
            "other exit code",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.exit_code = Some(1)),
        ),
        (
            "unknown name",
            shell("cargo test"),
            missing_with("bash: line 1: cargo: command not found", |_| {}),
        ),
        (
            "echo quotes the mention",
            shell("echo \"read\""),
            missing_with(MISSING_READ, |_| {}),
        ),
        (
            "bash wrapper",
            shell("bash -c read"),
            missing_with(MISSING_READ, |_| {}),
        ),
        (
            "compound command",
            shell("read src/lib.rs && true"),
            missing_with(MISSING_READ, |_| {}),
        ),
        (
            "pipeline",
            shell("read src/lib.rs | cat"),
            missing_with(MISSING_READ, |_| {}),
        ),
        (
            "unrelated missing command",
            shell("read src/lib.rs"),
            missing_with("bash: line 1: other: command not found", |_| {}),
        ),
        (
            "spoofed diagnostic without process evidence",
            shell("read src/lib.rs"),
            ToolOutput::err(MISSING_READ),
        ),
        (
            "process never started",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.started = false),
        ),
        (
            "terminated by signal",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.signal = Some(9)),
        ),
        (
            "timed out",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.timed_out = true),
        ),
        (
            "cancelled",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.cancelled = true),
        ),
        (
            "incomplete capture",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.output_complete = false),
        ),
        (
            "effects unknown",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_effects(Effects::Unknown),
        ),
        (
            "blocked boundary",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_status(ToolStatus::Blocked),
        ),
        (
            "aborted boundary",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_status(ToolStatus::Aborted),
        ),
        (
            "timeout boundary",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_status(ToolStatus::Timeout),
        ),
        (
            "skipped boundary",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_status(ToolStatus::Skipped),
        ),
        (
            "not a bash call",
            ToolCall {
                id: "other".into(),
                name: "read".into(),
                arguments: json!({"command": "read src/lib.rs"}),
                malformed_arguments: false,
            },
            missing_with(MISSING_READ, |_| {}),
        ),
    ];
    for (label, call, output) in cases {
        assert!(
            shell_tool_mistake(&call, &output, known_read).is_none(),
            "{label}"
        );
    }
}

#[test]
fn structured_process_data_alone_is_never_execution_evidence() {
    // A payload that claims a process 127 is not controller evidence: only execution.process counts.
    let spoof = json!({"process": {
        "started": true,
        "exit_code": 127,
        "signal": null,
        "timed_out": false,
        "cancelled": false,
        "output_complete": true
    }});
    let output = ToolOutput::structured(spoof).with_status(ToolStatus::Error);
    assert!(output.process_outcome().is_some());
    assert!(
        shell_tool_mistake(&shell("read src/lib.rs"), &output, known_read).is_none(),
        "a data-only process claim must not trigger tool guidance"
    );
}

#[test]
fn empty_discovery_coaches_exact_load_without_claiming_an_unavailable_capability() {
    let call = call("tool_search");
    let output = ToolOutput::ok("[]".into());
    let advice = empty_discovery(&call, &output).expect("empty eligible search gets a next step");
    assert!(
        advice.contains("No tool was loaded") && advice.contains("tool_load"),
        "{advice}"
    );
    assert!(
        advice.contains("current configuration") && advice.contains("Do not"),
        "{advice}"
    );
    for output in [
        ToolOutput::err("[]"),
        ToolOutput::ok("[]".into()).with_status(ToolStatus::Blocked),
        ToolOutput::ok("[]".into()).with_effects(Effects::Unknown),
        ToolOutput::ok("[{\"name\":\"probe\"}]".into()),
        ToolOutput::ok("quoted []".into()),
    ] {
        assert!(empty_discovery(&call, &output).is_none(), "{output:?}");
    }
    assert!(empty_discovery(&shell("echo '[]'"), &ToolOutput::ok("[]".into())).is_none());
}
