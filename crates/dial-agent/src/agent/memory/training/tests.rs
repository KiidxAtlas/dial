use super::*;

#[test]
fn snapshots_retain_actual_context_and_only_the_new_target() {
    let mut capture = Capture::default();
    let mut request = vec![
        json!({"role":"system","content":"working state and recalled memory"}),
        json!({"role":"user","content":"original constraints"}),
        json!({"role":"assistant","content":"previous answer"}),
        json!({"role":"user","content":"continue"}),
    ];
    capture.record(
        &request,
        &[],
        json!({"role":"assistant","content":"new answer"}),
    );
    request[0]["content"] = json!("changed after inference");
    let payload = capture.payload();
    let messages = payload["examples"][0]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0]["content"], "working state and recalled memory");
    assert_eq!(messages[1]["content"], "original constraints");
    assert_eq!(messages[4]["content"], "new answer");
    capture.clear();
    assert_eq!(capture.payload()["examples"], json!([]));
}

#[test]
fn bounded_capture_keeps_the_final_target_and_its_schema() {
    let mut capture = Capture::default();
    for i in 0..20 {
        capture.record(
            &[json!({"role":"user","content":"task"})],
            &[json!({"schema":i})],
            json!({"role":"assistant","content":i.to_string()}),
        );
    }
    assert_eq!(capture.examples.len(), MAX_EXAMPLES);
    assert_eq!(capture.examples.last().unwrap()["tools"][0]["schema"], 19);
    assert_eq!(
        capture.examples.last().unwrap()["messages"][1]["content"],
        "19"
    );
    capture.record(
        &[json!({"role":"user","content":"x".repeat(MAX_BYTES)})],
        &[],
        json!({"role":"assistant","content":"oversized"}),
    );
    assert_eq!(capture.examples.len(), MAX_EXAMPLES);
    assert!(capture.bytes <= MAX_BYTES);
}

#[test]
fn session_retention_excludes_nonordinary_unfinished_and_empty_captures() {
    use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
    use crate::agent::{NullUi, TurnOutcome, TurnReport};

    for case in [
        "hermetic",
        "domain",
        "ephemeral",
        "failed",
        "interrupted",
        "empty",
    ] {
        with_agent("", |agent| {
            agent
                .session
                .append(json!({"role":"user","content":"task"}))
                .unwrap();
            agent
                .training_capture
                .set_source_turn(agent.session.branch_leaf());
            if case != "empty" {
                agent.training_capture.record(
                    &[json!({"role":"user","content":"task"})],
                    &[],
                    json!({"role":"assistant","content":"answer"}),
                );
            }
            match case {
                "hermetic" => agent.hermetic = true,
                "domain" => agent.domain_override = Some("selfplay".into()),
                "ephemeral" => agent.session = crate::storage::session::Session::ephemeral(),
                _ => {}
            }
            let report = TurnReport {
                text: "answer".into(),
                outcome: if case == "failed" {
                    TurnOutcome::Failed
                } else {
                    TurnOutcome::Completed
                },
                interrupted: case == "interrupted",
                steps: 1,
                trajectory: None,
            };
            agent.retain_training_turn(&report, "task", &mut NullUi);
            assert!(
                agent.session.custom_records("training_turn").is_empty(),
                "case={case}"
            );
        });
    }
}
