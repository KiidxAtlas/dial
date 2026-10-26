use super::*;
use crate::agent::safety::guard::{LengthAction, StepBudget};
use crate::llm::Reply;

fn truncated(text: &str) -> Reply {
    Reply {
        text: text.into(),
        finish_reason: "length".into(),
        ..Default::default()
    }
}

fn nudge(engine: &mut Engine, reply: &Reply) -> LengthAction {
    // No room for a larger budget: both controllers reach the shared nudge policy.
    engine.length(1024, 1024, 1024, reply)
}

#[test]
fn unlimited_step_budget_still_stops_truncation_and_keeps_completed_effects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("completed.txt");
    std::fs::write(&path, "completed effect").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let call = ToolCall {
        id: "completed".into(),
        name: "write".into(),
        arguments: serde_json::json!({"path":"completed.txt","content":"completed effect"}),
        malformed_arguments: false,
    };
    // A truncated tool call is never usable progress, even if its arguments happen to parse.
    for reply in [
        truncated(""),
        Reply {
            tool_calls: vec![call.clone()],
            ..truncated("unexecuted call")
        },
    ] {
        let mut engine = Engine::default();
        let mut output = ToolOutput::ok("written".into()).with_effects(Effects::Known);
        engine.settle(&workspace, &call, &mut output, false, false, |_| false);
        let mut budget = StepBudget::new(0, StepBudget::FREE_RETRIES);
        let mut attempts = 0;
        let report = engine
            .drive(|engine| {
                assert!(budget.begin());
                attempts += 1;
                Ok(match nudge(engine, &reply) {
                    LengthAction::Nudge { partial, .. } => {
                        assert!(partial.is_none());
                        let _ = budget.refund();
                        Tick::Again
                    }
                    LengthAction::Incomplete(text) => Tick::Finished(crate::agent::TurnReport {
                        text,
                        outcome: crate::agent::TurnOutcome::Incomplete,
                        steps: attempts,
                        interrupted: false,
                        trajectory: None,
                    }),
                    LengthAction::Retry { .. } => panic!("budget cannot grow"),
                })
            })
            .unwrap()
            .unwrap();
        assert_eq!(attempts, 8);
        assert_eq!(report.outcome, crate::agent::TurnOutcome::Incomplete);
        assert!(report.text.contains("completed effects"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "completed effect");
    }
}

#[test]
fn repeated_partial_text_and_short_cycles_are_not_advancing_output() {
    for cycle in [
        vec!["same fragment", " same   fragment "],
        vec!["part one", "part two"],
    ] {
        let mut engine = Engine::default();
        let mut stopped = false;
        for text in cycle.iter().cycle().take(16) {
            if matches!(
                nudge(&mut engine, &truncated(text)),
                LengthAction::Incomplete(_)
            ) {
                stopped = true;
                break;
            }
        }
        assert!(stopped);
    }
}

#[test]
fn periodic_partial_streams_beyond_the_old_history_horizon_stop() {
    for period in [9, 17, 257] {
        let mut engine = Engine::default();
        let mut stopped = false;
        // Brent detects a periodic sequence in fewer than four periods; eight stalled nudges
        // then require a usable reply, even though every fragment differs from the last eight.
        for index in 0..(4 * period + 8) {
            let text = format!("Repeated fragment {}", index % period);
            if matches!(
                nudge(&mut engine, &truncated(&text)),
                LengthAction::Incomplete(_)
            ) {
                stopped = true;
                break;
            }
        }
        assert!(stopped, "period {period} escaped bounded recovery");
    }
}

#[test]
fn partial_novelty_cannot_rearm_recovery_after_a_detected_cycle() {
    let mut engine = Engine::default();
    assert!(matches!(
        nudge(&mut engine, &truncated("repeat")),
        LengthAction::Nudge { .. }
    ));
    assert!(matches!(
        nudge(&mut engine, &truncated("repeat")),
        LengthAction::Nudge { .. }
    ));
    for index in 0..6 {
        assert!(matches!(
            nudge(&mut engine, &truncated(&format!("new fragment {index}"))),
            LengthAction::Nudge { .. }
        ));
    }
    assert!(matches!(
        nudge(&mut engine, &truncated("another new fragment")),
        LengthAction::Incomplete(_)
    ));
    engine.length_recovered(&Reply {
        text: "A usable reply breaks the cycle".into(),
        ..Default::default()
    });
    for index in 0..128 {
        assert!(matches!(
            nudge(
                &mut engine,
                &truncated(&format!("useful new fragment {index}"))
            ),
            LengthAction::Nudge { .. }
        ));
    }
}

#[test]
fn larger_budget_and_advancing_continuations_remain_available() {
    let mut engine = Engine::default();
    assert_eq!(
        engine.length(1024, 32768, 1024, &truncated("")),
        LengthAction::Retry { max_tokens: 2048 }
    );
    for part in 0..128 {
        assert!(matches!(
            nudge(
                &mut engine,
                &truncated(&format!("Useful answer section {part}"))
            ),
            LengthAction::Nudge {
                partial: Some(_),
                ..
            }
        ));
    }
}

#[test]
fn usable_reply_rearms_length_recovery_but_empty_reply_does_not() {
    let mut engine = Engine::default();
    for _ in 0..100 {
        for _ in 0..7 {
            assert!(matches!(
                nudge(&mut engine, &truncated("")),
                LengthAction::Nudge { .. }
            ));
        }
        engine.length_recovered(&Reply {
            text: "useful answer".into(),
            ..Default::default()
        });
    }
    for _ in 0..7 {
        assert!(matches!(
            nudge(&mut engine, &truncated("")),
            LengthAction::Nudge { .. }
        ));
        engine.length_recovered(&Reply::default());
    }
    assert!(matches!(
        nudge(&mut engine, &truncated("")),
        LengthAction::Incomplete(_)
    ));
}

#[test]
fn child_adapter_reports_incomplete_after_the_same_shared_length_bound() {
    let mut engine = Engine::default();
    let mut steps = 0;
    let report = engine
        .drive(|engine| {
            steps += 1;
            Ok(match nudge(engine, &truncated("")) {
                LengthAction::Nudge { .. } => Tick::Again,
                LengthAction::Incomplete(message) => Tick::Finished(
                    crate::agent::workers::subagents::Outcome::incomplete(steps, message),
                ),
                LengthAction::Retry { .. } => panic!("budget cannot grow"),
            })
        })
        .unwrap()
        .unwrap();
    assert_eq!(steps, 8);
    assert_eq!(report.status.phase, crate::runtime::Phase::Incomplete);
    assert!(report.status.text.contains("completed effects"));
}
