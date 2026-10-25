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
