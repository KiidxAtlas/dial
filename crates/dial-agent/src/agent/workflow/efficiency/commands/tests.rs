use super::*;
use crate::runtime::execution::{OutputIntegrity, ProcessOutcome};
use serde_json::json;

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "check".into(),
        name: name.into(),
        arguments: json!({"command":"cargo test"}),
        malformed_arguments: false,
    }
}

fn output(tail: &str) -> ToolOutput {
    ToolOutput::ok(format!(
        "{}{tail}",
        "   Compiling sample v0.1.0 (/workspace/sample)\n".repeat(200)
    ))
    .with_process(ProcessOutcome {
        started: true,
        exit_code: Some(0),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    })
}

fn compact(output: &ToolOutput) -> Option<String> {
    compact_command(&call("bash"), output, Some("artifact://complete"))
}

#[test]
fn recognized_noise_is_removed_but_counts_and_unknown_lines_remain() {
    let tail = "    Checking sample v0.1.0\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.23s\nrunning 2 tests\ntest sample::one ... ok\ntest sample::two ... ok\ncustom important observation\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n[exit code: 0]\n";
    let output = output(tail);
    for name in ["bash", "bg"] {
        let result = compact_command(&call(name), &output, Some("artifact://complete")).unwrap();
        assert!(!result.contains("Compiling sample"));
        assert!(!result.contains("Checking sample"));
        assert!(!result.contains("Finished `test`"));
        assert!(!result.contains("test sample::one ... ok"));
        assert!(result.contains("running 2 tests"));
        assert!(result.contains("custom important observation"));
        assert!(result.contains("2 passed; 0 failed"));
        assert!(result.contains("[exit code: 0]"));
        assert!(result.contains(
            "[Complete tool evidence: artifact://complete; retrieve with artifact_read]"
        ));
    }
}

#[test]
fn warning_in_the_middle_preserves_its_entire_continuation() {
    let diagnostic = "warning: unused variable\n --> src/main.rs:12:1\n  |\n  | test misleading ... ok\n\n   Checking source_excerpt v0.1.0\n  = help: do not drop this continuation\n";
    let result = compact(&output(diagnostic)).unwrap();
    assert!(result.contains(diagnostic));
    assert!(!result.contains("Compiling sample"));
}

#[test]
fn zero_test_counts_and_no_test_messages_are_not_hidden_or_called_passing() {
    for message in [
        "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored\n",
        "no tests ran\n",
        "No tests were found!!!\n",
    ] {
        let result = compact(&output(message)).unwrap();
        assert!(result.contains(message));
        assert!(!result.contains("Successful check"));
        assert!(!result.contains("passed check"));
    }
}

#[test]
fn arbitrary_successful_output_and_small_results_are_unchanged() {
    let mut arbitrary = output("");
    arbitrary.text = "application-specific successful output\n".repeat(200);
    assert!(compact(&arbitrary).is_none());
    arbitrary.text = "test small ... ok\n".into();
    assert!(compact(&arbitrary).is_none());
    assert!(compact_command(&call("read"), &output(""), Some("artifact://full")).is_none());
    assert!(compact_command(&call("bash"), &output(""), None).is_none());
}

#[test]
fn printed_or_payload_success_does_not_supply_controller_facts() {
    let mut prose = output("[exit code: 0]\n");
    prose.execution.process = None;
    assert!(
        compact(&prose).is_none(),
        "data.process is not controller authority"
    );
}

#[test]
fn errors_unknown_effects_and_all_incomplete_process_lifecycles_are_unchanged() {
    let original = output("");
    let mut error = original.clone();
    error.is_error = true;
    assert!(compact(&error).is_none());
    let mut unknown = original.clone();
    unknown.execution.effects = Effects::Unknown;
    assert!(compact(&unknown).is_none());
    for status in [
        ToolStatus::Error,
        ToolStatus::Timeout,
        ToolStatus::Aborted,
        ToolStatus::Blocked,
        ToolStatus::Skipped,
    ] {
        let mut output = original.clone();
        output.execution.status = Some(status);
        assert!(compact(&output).is_none(), "{status:?}");
    }
    let process = original.execution.process.as_ref().unwrap();
    for incomplete in [
        ProcessOutcome {
            started: false,
            ..process.clone()
        },
        ProcessOutcome {
            exit_code: None,
            ..process.clone()
        },
        ProcessOutcome {
            exit_code: Some(1),
            ..process.clone()
        },
        ProcessOutcome {
            signal: Some(9),
            ..process.clone()
        },
        ProcessOutcome {
            timed_out: true,
            ..process.clone()
        },
        ProcessOutcome {
            cancelled: true,
            ..process.clone()
        },
        ProcessOutcome {
            output_complete: false,
            ..process.clone()
        },
    ] {
        let output = original.clone().with_process(incomplete);
        assert!(compact(&output).is_none(), "{:?}", output.execution.process);
    }
}

#[test]
fn incomplete_capture_or_presentation_and_partial_retention_are_unchanged() {
    let complete = OutputIntegrity {
        capture: Completeness::Complete,
        presentation: Completeness::Complete,
        retention: Completeness::Complete,
        ..OutputIntegrity::default()
    };
    assert!(compact(&output("").with_output_integrity(complete.clone())).is_some());
    for incomplete in [
        OutputIntegrity {
            capture: Completeness::Partial,
            ..complete.clone()
        },
        OutputIntegrity {
            capture: Completeness::Unknown,
            ..complete.clone()
        },
        OutputIntegrity {
            presentation: Completeness::Partial,
            ..complete.clone()
        },
        OutputIntegrity {
            presentation: Completeness::Unknown,
            ..complete.clone()
        },
        OutputIntegrity {
            retention: Completeness::Partial,
            ..complete
        },
    ] {
        assert!(compact(&output("").with_output_integrity(incomplete)).is_none());
    }
}

#[test]
fn ansi_output_is_preserved_without_interpreting_terminal_controls() {
    let ansi = "\x1b[32m   Compiling colored v0.1.0\x1b[0m\n\x1b[33mwarning\x1b[0m: important\n\x1b[1m  --> file.rs:10\x1b[0m\n";
    let result = compact(&output(ansi)).unwrap();
    assert!(result.contains(ansi));
    let mut all_ansi = output("");
    all_ansi.text = "\x1b[32mtest colored ... ok\x1b[0m\n".repeat(200);
    assert!(compact(&all_ansi).is_none());
}
