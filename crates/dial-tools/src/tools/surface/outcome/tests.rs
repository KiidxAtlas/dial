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

#[test]
fn edit_projection_is_named_digest_bound_and_never_applied_to_errors() {
    let model = "[a.txt#ABCD]\nupdated (+1 -1 lines)\n1:new\n";
    let raw = format!("{model}\nchange preview (+1 -1 lines)\n-old\n+new\n");
    let output = edit_projection(raw.clone(), model.into());
    assert_eq!(message_for("edit", "id", &output)["content"], model);
    assert_eq!(message("id", &output)["content"], raw);
    for name in ["bash", "extension_edit", "write"] {
        assert_eq!(message_for(name, "id", &output)["content"], raw);
    }
    for case in 0..5 {
        let mut changed = output.clone();
        match case {
            0 => changed.is_error = true,
            1 => changed.text.push_str("\nwarning: hook annotation"),
            2 => changed.execution.status = Some(ToolStatus::Blocked),
            3 => changed.data.as_mut().unwrap()[EDIT_PROJECTION]["producer"] = "extension".into(),
            4 => {
                changed.data.as_mut().unwrap()[EDIT_PROJECTION]["anchored_text"] =
                    "unobserved".into()
            }
            _ => unreachable!(),
        }
        assert_eq!(
            message_for("edit", "id", &changed)["content"],
            changed.text,
            "case {case}"
        );
    }
}
