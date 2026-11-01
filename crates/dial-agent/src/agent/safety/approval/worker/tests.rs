use super::*;
use crate::agent::{
    Decision,
    safety::approval::{Effect, PermissionRule},
};
use serde_json::json;

struct NeverPrompt;
impl Ui for NeverPrompt {
    fn approve(&mut self, _: &str, _: &str) -> Decision {
        panic!("policy must decide before prompting")
    }
}

#[test]
fn worker_policy_checks_cancellation_deny_rules_and_readonly_before_safe_shortcuts() {
    let call = ToolCall {
        id: "worker-call".into(),
        name: "write".into(),
        arguments: json!({"path":"file"}),
        malformed_arguments: false,
    };
    for (cancelled, denied, readonly, needs, provenance, expected) in [
        (
            true,
            false,
            false,
            false,
            CallProvenance::Structured,
            "cancel",
        ),
        (
            false,
            true,
            false,
            false,
            CallProvenance::Structured,
            "rule",
        ),
        (
            false,
            false,
            true,
            true,
            CallProvenance::Structured,
            "readonly",
        ),
        (
            false,
            false,
            true,
            false,
            CallProvenance::Structured,
            "safe",
        ),
        (
            true,
            true,
            false,
            false,
            CallProvenance::RecoveredText,
            "cancel",
        ),
        (
            false,
            true,
            false,
            false,
            CallProvenance::RecoveredText,
            "rule",
        ),
        (
            false,
            false,
            true,
            true,
            CallProvenance::RecoveredText,
            "readonly",
        ),
    ] {
        let cancel = AtomicBool::new(cancelled);
        let mut rules = if denied {
            vec![PermissionRule {
                effect: Effect::Deny,
                tool: "write".into(),
                command_prefix: vec![],
                paths: vec![],
            }]
        } else {
            vec![]
        };
        let result = authorize_worker_call(
            WorkerPolicy {
                approval: Approval::Ask,
                rules: &mut rules,
                root: Path::new("/project"),
                read_only: readonly,
                cancel: Some(&cancel),
                interrupt: None,
            },
            &call,
            "write file",
            needs,
            provenance,
            &mut NeverPrompt,
        );
        assert!(
            matches!(
                (expected, result),
                ("cancel", Err(Denial::User))
                    | ("rule", Err(Denial::Rule))
                    | ("readonly", Err(Denial::ReadOnly))
                    | ("safe", Ok(()))
            ),
            "{expected}"
        );
    }
}

#[test]
fn cancelled_prompt_cannot_leave_a_session_grant() {
    struct RacingUi<'a>(&'a AtomicBool);
    impl Ui for RacingUi<'_> {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            self.0.store(true, Ordering::SeqCst);
            Decision::AllowAlways
        }
    }
    for provenance in [CallProvenance::Structured, CallProvenance::RecoveredText] {
        let cancel = AtomicBool::new(false);
        let mut rules = Vec::new();
        let call = ToolCall {
            id: "race".into(),
            name: "write".into(),
            arguments: json!({"path":"file"}),
            malformed_arguments: false,
        };
        let result = authorize_worker_call(
            WorkerPolicy {
                approval: Approval::Ask,
                rules: &mut rules,
                root: Path::new("/project"),
                read_only: false,
                cancel: Some(&cancel),
                interrupt: None,
            },
            &call,
            "write file",
            true,
            provenance,
            &mut RacingUi(&cancel),
        );
        assert!(matches!(result, Err(Denial::User)));
        assert!(rules.is_empty());
    }
}

#[test]
fn recovered_text_requires_human_once_despite_modes_and_grants() {
    struct View {
        decision: Decision,
        prompts: usize,
    }
    impl Ui for View {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            self.prompts += 1;
            self.decision
        }
    }
    let call = ToolCall {
        id: "recovered".into(),
        name: "read".into(),
        arguments: json!({"path":"file"}),
        malformed_arguments: false,
    };
    for mode in [
        Approval::Ask,
        Approval::Write,
        Approval::Auto,
        Approval::Readonly,
    ] {
        for decision in [
            Decision::Allow,
            Decision::AllowAlways,
            Decision::AllowProject,
            Decision::Deny,
        ] {
            let mut rules = vec![PermissionRule {
                effect: Effect::Allow,
                tool: "read".into(),
                command_prefix: vec![],
                paths: vec![],
            }];
            let mut view = View {
                decision,
                prompts: 0,
            };
            let result = authorize_worker_call(
                WorkerPolicy {
                    approval: mode,
                    rules: &mut rules,
                    root: Path::new("/project"),
                    read_only: true,
                    cancel: None,
                    interrupt: None,
                },
                &call,
                "read file",
                false,
                CallProvenance::RecoveredText,
                &mut view,
            );
            assert_eq!(
                result.is_ok(),
                mode != Approval::Readonly && decision != Decision::Deny,
                "{mode:?} {decision:?}"
            );
            assert_eq!(
                view.prompts,
                usize::from(mode != Approval::Readonly),
                "{mode:?} {decision:?}"
            );
            assert_eq!(
                rules.len(),
                1,
                "a recovered call must not create reusable grants"
            );
        }
    }
}
