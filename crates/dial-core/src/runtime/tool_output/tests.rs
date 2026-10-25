use super::*;
use crate::runtime::execution::ProcessOutcome;

fn outcome(code: i32) -> ProcessOutcome {
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
fn controller_process_facts_take_precedence_over_legacy_payload() {
    let mut output = ToolOutput::ok("observed".into()).with_process(outcome(3));
    assert_eq!(output.data.as_ref().unwrap()["process"]["exit_code"], 3);
    output.data = Some(serde_json::json!({"process": outcome(0)}));
    assert_eq!(output.process_outcome(), Some(outcome(3)));
    assert!(
        !output.is_error,
        "observing a failed child need not fail the observation"
    );
}

#[test]
fn old_payload_only_process_facts_remain_readable() {
    let output = ToolOutput::structured(serde_json::json!({"process": outcome(4)}));
    assert_eq!(output.process_outcome(), Some(outcome(4)));
}
