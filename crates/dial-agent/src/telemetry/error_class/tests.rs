use super::*;
use crate::runtime::execution::ProcessOutcome;
use serde_json::json;

fn failed(data: Option<serde_json::Value>) -> ToolOutput {
    ToolOutput {
        data,
        ..ToolOutput::err("diagnostic prose that must not be read")
    }
}

fn process(started: bool, exit_code: Option<i32>, signal: Option<i32>) -> ToolOutput {
    let mut output = failed(None);
    output.execution.process = Some(ProcessOutcome {
        started,
        exit_code,
        signal,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    });
    output
}

#[test]
fn only_errored_calls_get_a_class() {
    let output = failed(None);
    for status in [
        ToolStatus::Ok,
        ToolStatus::Blocked,
        ToolStatus::Skipped,
        ToolStatus::Timeout,
        ToolStatus::Aborted,
    ] {
        assert_eq!(error_class("bash", status, &output), None, "{status:?}");
    }
    assert_eq!(
        error_class("read", ToolStatus::Error, &output),
        Some("tool_error")
    );
}

#[test]
fn edits_are_classed_by_their_typed_outcome() {
    let class = |data| error_class("edit", ToolStatus::Error, &failed(data));
    assert_eq!(
        class(Some(json!({"edit":{"outcome":"source_changed"}}))),
        Some("edit_source_changed")
    );
    assert_eq!(
        class(Some(json!({"edit":{"outcome":"target_not_found"}}))),
        Some("edit_target_not_found")
    );
    assert_eq!(class(None), Some("edit_refused"));
    assert_eq!(
        error_class("ast_edit", ToolStatus::Error, &failed(None)),
        Some("edit_refused")
    );
}

#[test]
fn a_failing_check_is_not_a_broken_verify() {
    let class = |data| error_class("verify", ToolStatus::Error, &failed(data));
    assert_eq!(
        class(Some(json!({"counts":{"passed":2,"failed":1}}))),
        Some("check_failed")
    );
    assert_eq!(
        class(Some(json!({"counts":{"passed":0,"failed":0}}))),
        Some("verify_refused")
    );
    assert_eq!(class(None), Some("verify_refused"));
}

#[test]
fn processes_are_classed_by_how_they_ended() {
    let class = |output: &ToolOutput| error_class("bash", ToolStatus::Error, output);
    assert_eq!(class(&process(false, None, None)), Some("not_started"));
    assert_eq!(class(&process(true, None, Some(9))), Some("signalled"));
    assert_eq!(class(&process(true, Some(2), None)), Some("nonzero_exit"));
    assert_eq!(class(&process(true, Some(0), None)), Some("tool_error"));
}

#[test]
fn unknown_effects_outrank_every_other_class() {
    let mut output = failed(Some(json!({"edit":{"outcome":"target_not_found"}})));
    output.execution.effects = Effects::Unknown;
    assert_eq!(
        error_class("edit", ToolStatus::Error, &output),
        Some("effects_unknown")
    );
}
