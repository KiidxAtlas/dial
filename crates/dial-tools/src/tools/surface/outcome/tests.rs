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
