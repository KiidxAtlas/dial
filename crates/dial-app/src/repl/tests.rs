use super::*;
use crate::agent::safety::approval::{self, PolicyExplanation, PolicyReason};

#[test]
fn headless_policy_observation_cannot_override_explicit_allowlist_or_deny_rules() {
    let call = crate::llm::ToolCall {
        id: "print-call".into(),
        name: "write".into(),
        arguments: serde_json::json!({"path":"x", "content":"x"}),
        malformed_arguments: false,
    };
    for allowed in [false, true] {
        let mut ui = PrintUi::new(
            OutputFormat::Json,
            if allowed {
                vec!["write".into()]
            } else {
                vec![]
            },
        );
        // A fabricated observation must have no effect on permission.
        ui.policy_explanation(
            &call.id,
            &call.name,
            &PolicyExplanation {
                reason: PolicyReason::AutomaticMode,
                rule_scope: None,
            },
        );
        let mut rules = vec![];
        assert_eq!(
            approval::authorize(
                crate::config::types::Approval::Ask,
                &mut rules,
                std::path::Path::new("."),
                &call,
                "",
                &mut ui
            )
            .is_ok(),
            allowed
        );
        assert!(rules.is_empty());
        rules.push(approval::PermissionRule {
            effect: approval::Effect::Deny,
            tool: "write".into(),
            command_prefix: vec![],
            paths: vec![],
        });
        assert!(matches!(
            approval::authorize(
                crate::config::types::Approval::Auto,
                &mut rules,
                std::path::Path::new("."),
                &call,
                "",
                &mut ui
            ),
            Err(approval::Denial::Rule)
        ));
        assert_eq!(rules.len(), 1);
    }
}

#[test]
fn out_of_order_same_tool_results_keep_their_own_timing_and_failure() {
    let mut ui = PrintUi::new(OutputFormat::Json, vec![]);
    ui.tool_call_with_id("slow", "read", "large source");
    ui.tool_call_with_id("fast", "read", "missing source");
    ui.tool_timing("fast", "read", 0.25);
    ui.tool_result_with_id("fast", "read", &ToolOutput::err("file not found"));
    ui.tool_timing("slow", "read", 2.0);
    ui.tool_result_with_id("slow", "read", &ToolOutput::ok("source contents".into()));
    let results = ui
        .calls
        .iter()
        .map(|call| {
            (
                call["call_id"].as_str().unwrap(),
                call["seconds"].as_f64().unwrap(),
                call["is_error"].as_bool().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(results, [("slow", 2.0, false), ("fast", 0.25, true)]);
}

#[test]
fn delivered_answer_succeeds_without_completed_work_but_not_when_rejected_or_denied() {
    for (outcome, text, interrupted, denied, error, expected) in [
        (
            crate::agent::TurnOutcome::Answered,
            "reply",
            false,
            false,
            None,
            0,
        ),
        (
            crate::agent::TurnOutcome::Answered,
            "",
            false,
            false,
            None,
            2,
        ),
        (
            crate::agent::TurnOutcome::Answered,
            "reply",
            true,
            false,
            None,
            2,
        ),
        (
            crate::agent::TurnOutcome::Answered,
            "reply",
            false,
            true,
            None,
            3,
        ),
        (
            crate::agent::TurnOutcome::Answered,
            "reply",
            false,
            false,
            Some("failed"),
            1,
        ),
        (
            crate::agent::TurnOutcome::Incomplete,
            "reply",
            false,
            false,
            None,
            2,
        ),
    ] {
        let mut ui = PrintUi::new(OutputFormat::Json, vec![]);
        ui.set_completed(false);
        ui.denied = denied;
        let report = crate::agent::TurnReport {
            outcome,
            text: text.into(),
            interrupted,
            steps: 1,
            trajectory: None,
        };
        assert_eq!(
            ui.finish(Some(&report), error, None, std::time::Duration::ZERO)
                .unwrap(),
            expected,
            "{outcome:?} text={text:?} interrupted={interrupted} denied={denied} error={error:?}"
        );
    }
}
