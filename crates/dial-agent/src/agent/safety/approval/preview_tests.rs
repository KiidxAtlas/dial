use super::*;
use serde_json::json;
use std::sync::atomic::AtomicBool;

struct Prompt {
    decision: Decision,
    previews: Vec<ApprovalPreview>,
}

impl Ui for Prompt {
    fn approve_call_with_preview(
        &mut self,
        _: &str,
        _: &str,
        _: &str,
        preview: &ApprovalPreview,
        _: Option<&AtomicBool>,
    ) -> Decision {
        self.previews.push(preview.clone());
        self.decision
    }
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "preview-call".into(),
        name: name.into(),
        arguments,
        malformed_arguments: false,
    }
}

#[test]
fn preview_describes_the_saved_matcher_and_its_lifetime() {
    let root = Path::new("/project");
    for (decision, persists) in [
        (Decision::AllowAlways, false),
        (Decision::AllowProject, true),
    ] {
        for (command, prefix, matching, different) in [
            (
                "/usr/bin/cargo test --lib",
                vec!["cargo", "test"],
                "cargo test --test integration",
                "cargo build",
            ),
            (
                "custom 'two words' x",
                vec!["custom", "two words"],
                "custom 'two words' y",
                "custom two words",
            ),
        ] {
            let mut ui = Prompt {
                decision,
                previews: vec![],
            };
            let mut rules = vec![];
            let requested = call("bash", json!({"command":command}));
            assert_eq!(
                authorize(Approval::Ask, &mut rules, root, &requested, "", &mut ui).ok(),
                Some(persists)
            );
            let [rule] = rules.as_slice() else {
                panic!("expected one rule for {command}")
            };
            let [preview] = ui.previews.as_slice() else {
                panic!("expected one preview for {command}")
            };
            assert_eq!(rule.command_prefix, prefix, "{command}");
            let matcher = format!("{:?}", rule.command_prefix);
            assert!(preview.session.as_ref().unwrap().contains(&matcher));
            assert!(preview.project.as_ref().unwrap().contains(&matcher));
            assert!(preview.session.as_ref().unwrap().contains("not saved"));
            assert!(
                preview
                    .project
                    .as_ref()
                    .unwrap()
                    .contains("future sessions, until revoked")
            );
            assert!(preview.unavailable_reason.is_none());
            assert!(rule.matches(&call("bash", json!({"command":matching})), root));
            assert!(!rule.matches(&call("bash", json!({"command":different})), root));
            assert!(!rule.matches(
                &call("bash", json!({"command":format!("{matching}; rm file")})),
                root
            ));
        }
    }
}

#[test]
fn unsupported_scopes_are_absent_and_cannot_be_saved_by_legacy_choices() {
    let mut calls: Vec<_> = [
        "rm -rf file",
        "git reset --hard",
        "cargo test && cargo build",
        "cargo test > output",
        "env cargo test",
        "env",
        "sudo cargo test",
        "sh -c 'cargo test'",
        "python script.py",
        "node script.js",
        "cargo --version",
        "git -C elsewhere status",
        "FOO=bar cargo test",
        "cargo $TASK",
        "",
    ]
    .into_iter()
    .map(|command| call("bash", json!({"command":command})))
    .collect();
    calls.push(call("bash", json!({})));
    calls.push(call("monitor", json!({"action":"start"})));
    calls.push(call("request_directory", json!({"path":"/outside"})));
    calls.push(call(
        "request_sensitive_directory",
        json!({"path":"/outside"}),
    ));
    for requested in calls {
        for decision in [Decision::AllowAlways, Decision::AllowProject] {
            let mut ui = Prompt {
                decision,
                previews: vec![],
            };
            let mut rules = vec![];
            assert_eq!(
                authorize(
                    Approval::Ask,
                    &mut rules,
                    Path::new("/project"),
                    &requested,
                    "",
                    &mut ui
                )
                .ok(),
                Some(false),
                "{}",
                requested.arguments
            );
            assert!(rules.is_empty(), "{}", requested.arguments);
            let preview = &ui.previews[0];
            assert!(preview.session.is_none(), "{}", requested.arguments);
            assert!(preview.project.is_none(), "{}", requested.arguments);
            assert!(
                preview.unavailable_reason.is_some(),
                "{}",
                requested.arguments
            );
            assert!(preview.once.contains("once"));
        }
    }
}

#[test]
fn tool_wide_preview_discloses_unrestricted_arguments_and_once_saves_nothing() {
    for decision in [Decision::Allow, Decision::AllowProject, Decision::Deny] {
        let mut ui = Prompt {
            decision,
            previews: vec![],
        };
        let mut rules = vec![];
        let requested = call("write", json!({"path":"one.rs", "content":"x"}));
        let result = authorize(
            Approval::Ask,
            &mut rules,
            Path::new("/project"),
            &requested,
            "",
            &mut ui,
        );
        let preview = &ui.previews[0];
        assert!(
            preview
                .session
                .as_ref()
                .unwrap()
                .contains("Every write call, with any arguments and paths")
        );
        if decision == Decision::AllowProject {
            assert_eq!(result.ok(), Some(true));
            assert!(rules[0].matches(
                &call("write", json!({"path":"other.rs", "content":"y"})),
                Path::new("/project")
            ));
        } else {
            assert!(rules.is_empty());
            assert_eq!(result.is_ok(), decision == Decision::Allow);
        }
    }
}

#[test]
fn preview_masks_credentials_without_changing_the_actual_matcher() {
    let credential = format!("ghp_{}", "a1B2".repeat(9));
    let requested = call(
        "bash",
        json!({"command":format!("custom {credential} argument")}),
    );
    let mut ui = Prompt {
        decision: Decision::AllowProject,
        previews: vec![],
    };
    let mut rules = vec![];
    assert_eq!(
        authorize(
            Approval::Ask,
            &mut rules,
            Path::new("/project"),
            &requested,
            "",
            &mut ui
        )
        .ok(),
        Some(true)
    );
    let preview = &ui.previews[0];
    for text in [&preview.session, &preview.project] {
        let text = text.as_ref().unwrap();
        assert!(!text.contains(&credential));
        assert!(text.contains("[REDACTED:"));
    }
    assert_eq!(rules[0].command_prefix, ["custom", credential.as_str()]);
    assert!(rules[0].matches(&requested, Path::new("/project")));
}
