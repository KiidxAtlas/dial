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
