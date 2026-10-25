use super::*;
#[test]
fn coherent_runtime_work_and_worker_control_have_independent_outcomes() {
    let output = ToolOutput {
        execution: Default::default(),
        text: "partial work".into(),
        is_error: true,
        data: Some(serde_json::json!({"jobs":[{"status":"incomplete"}]})),
    };
    assert_eq!(
        result_phase("task", &serde_json::json!({"prompt":"inspect"}), &output),
        Phase::Incomplete
    );
    let observed = ToolOutput {
        is_error: false,
        ..output
    };
    assert_eq!(
        result_phase("task", &serde_json::json!({"action":"status"}), &observed),
        Phase::Completed
    );
    assert_eq!(normalize(Phase::Running, &observed), Phase::Incomplete);
    assert_eq!(
        normalize(Phase::Completed, &ToolOutput::err("failed execution")),
        Phase::Failed
    );
}

#[test]
fn model_history_keeps_controller_execution_and_integrity_facts() {
    use crate::runtime::execution::{
        Completeness, ObservationKind, OutputIntegrity, ProcessOutcome,
    };
    let output = ToolOutput::ok("observed child exit".into())
        .with_observation(ObservationKind::Wait, true)
        .with_process(ProcessOutcome {
            started: true,
            exit_code: Some(3),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        })
        .with_output_integrity(OutputIntegrity {
            capture: Completeness::Complete,
            retention: Completeness::Partial,
            presentation: Completeness::Partial,
            ..Default::default()
        });
    let history = message("call-1", &output);
    assert_eq!(history["dial_execution"]["observation"]["satisfied"], true);
    assert_eq!(history["dial_execution"]["process"]["exit_code"], 3);
    assert_eq!(history["dial_execution"]["output"]["capture"], "complete");
    assert_eq!(history["dial_execution"]["output"]["retention"], "partial");
    assert_eq!(history["dial_execution_status"], "ok");
}
