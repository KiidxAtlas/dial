use super::*;

#[test]
fn short_circuited_round_stalls_progress_without_advancing_failure_streak() {
    let mut stall = guard::StallGuard::default();
    let mut pace = guard::PaceGuard::default();
    let mut reliability = crate::reliability::Stats::default();
    let mut gradient = crate::agent::run::outcome_gradient::Gradient::default();

    let eval = evaluate_round(
        RoundSignals {
            raw_progress: RoundProgress::Neutral,
            short_circuited: true,
            narration_warning: None,
            round_succeeded: false,
            tool_calls: &[],
            repeat_streak: None,
            form_kinds: &[],
            execution_failed: false,
            observed_check_failure: false,
            round_checked: false,
            mutations: 0,
            failed_checks: 0,
            failing_tests: 0,
            unnamed_failures: 0,
            gradient_rounds: 0,
            reliability_enabled: true,
            max_contract_failures: 3,
            hermetic: false,
        },
        &mut stall,
        &mut pace,
        &mut reliability,
        &mut gradient,
    );
    assert_eq!(eval.effective_progress, RoundProgress::Stalled);
    assert!(eval.tool_round_failed);
    assert!(eval.hard_stop.is_none());
    assert!(eval.advisories.is_empty());
}

#[test]
fn contract_failure_threshold_emits_failure_escalation_and_short_circuit_stop_blocks() {
    let mut stall = guard::StallGuard::default();
    let mut pace = guard::PaceGuard::default();
    let mut reliability = crate::reliability::Stats {
        consecutive_contract_failures: 2,
        ..Default::default()
    };
    let mut gradient = crate::agent::run::outcome_gradient::Gradient::default();
    let mut messages = Vec::new();
    for index in 0..=(stream_guards::TOOL_CALL_LOOP_THRESHOLD + 2) {
        let id = format!("call-{index}");
        messages.push(json!({
            "role": "assistant",
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}
            }]
        }));
        messages.push(json!({
            "role": "tool",
            "tool_call_id": format!("call-{index}"),
            "content": "contents"
        }));
    }
    let streak = guard::RepeatedToolCall::detect(
        &messages,
        stream_guards::TOOL_CALL_LOOP_THRESHOLD,
        stream_guards::TOOL_CALL_LOOP_EXEMPT_TOOLS,
    )
    .expect("expected repeated call streak");
    let form_kinds = vec!["unknown_tool".to_owned()];

    let eval = evaluate_round(
        RoundSignals {
            raw_progress: RoundProgress::Neutral,
            short_circuited: true,
            narration_warning: Some("Narration warning".into()),
            round_succeeded: false,
            tool_calls: &[],
            repeat_streak: Some(&streak),
            form_kinds: &form_kinds,
            execution_failed: false,
            observed_check_failure: false,
            round_checked: false,
            mutations: 0,
            failed_checks: 0,
            failing_tests: 0,
            unnamed_failures: 0,
            gradient_rounds: 0,
            reliability_enabled: true,
            max_contract_failures: 3,
            hermetic: false,
        },
        &mut stall,
        &mut pace,
        &mut reliability,
        &mut gradient,
    );
    let stop = eval.hard_stop.expect("expected repeated-call hard stop");
    assert_eq!(stop.outcome, TurnOutcome::Blocked);
    assert_eq!(stop.event_kind, "repeated_call_stop");
    assert!(eval.advisories.iter().any(|(p, _)| *p == Priority::Failure));
    assert!(eval.advisories.iter().any(|(p, _)| *p == Priority::Loop));
}
