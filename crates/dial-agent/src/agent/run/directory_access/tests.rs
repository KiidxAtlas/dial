use super::*;
use crate::config::types::{Approval, Config, Version};
use crate::llm::provider::{Backend, ProviderKind};
use std::path::PathBuf;

#[derive(Default)]
struct Answers {
    decisions: Vec<Decision>,
    asked: Vec<(String, String)>,
    interactive: bool,
}
impl Ui for Answers {
    fn interactive_input(&self) -> bool {
        self.interactive
    }
    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        self.asked.push((tool.into(), summary.into()));
        if self.decisions.is_empty() {
            Decision::Deny
        } else {
            self.decisions.remove(0)
        }
    }
}
fn endpoint() -> Backend {
    Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    )
}
fn with_agent(f: impl FnOnce(&mut Agent<'_>, &Path)) {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let config = Config {
        state_dir: tmp.path().join("state"),
        ..Config::default()
    };
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent = Agent::hermetic(&config, endpoint(), &version, &project, None, None).unwrap();
    agent.hermetic = false; // Exercise the interactive control path without reading host/session storage.
    agent.approval = Approval::Auto;
    f(&mut agent, tmp.path());
}
fn call(path: &Path, sensitive: bool) -> ToolCall {
    ToolCall {
        id: "directory-1".into(),
        name: "ask".into(),
        arguments: json!({"action":"directory","path":path,"reason":"test directory access","sensitive":sensitive}),
        malformed_arguments: false,
    }
}

#[test]
fn outside_read_waits_for_decision_on_original_call() {
    for allow in [false, true] {
        with_agent(|agent, tmp| {
            let other = tmp.join("other");
            std::fs::create_dir(&other).unwrap();
            let file = other.join("note.txt");
            std::fs::write(&file, "approved payload\n").unwrap();
            let read = ToolCall {
                id: "pending-read".into(),
                name: "read".into(),
                arguments: json!({"path":format!("{}:1-1",file.display())}),
                malformed_arguments: false,
            };
            let mut ui = Answers {
                interactive: true,
                decisions: vec![if allow {
                    Decision::Allow
                } else {
                    Decision::Deny
                }],
                ..Default::default()
            };
            let output = agent.dispatch(&mut endpoint(), &read, &mut ui);
            assert_eq!(ui.asked.len(), 1, "{}", output.text);
            assert!(
                ui.asked[0]
                    .1
                    .contains(other.canonicalize().unwrap().to_str().unwrap())
            );
            assert_eq!(output.is_error, !allow, "{}", output.text);
            assert_eq!(output.text.contains("approved payload"), allow);
            assert_eq!(
                agent.workspace.resolve_read(file.to_str().unwrap()).is_ok(),
                allow
            );
        });
    }
}

#[test]
fn pending_batch_write_is_not_partially_executed_on_directory_denial() {
    with_agent(|agent, tmp| {
        let outside = tmp.join("other");
        std::fs::create_dir(&outside).unwrap();
        let external = outside.join("new");
        let write = ToolCall {
            id: "pending-write".into(),
            name: "write".into(),
            malformed_arguments: false,
            arguments: json!({"files":[{"path":"local","content":"a"},{"path":external,"content":"b"}]}),
        };
        let mut ui = Answers {
            interactive: true,
            ..Default::default()
        };
        let result = agent.dispatch(&mut endpoint(), &write, &mut ui);
        assert_eq!(ui.asked.len(), 1);
        assert_eq!(
            tools::ToolStatus::from_output(&result),
            tools::ToolStatus::Blocked
        );
        assert!(!external.exists() && !agent.workspace.root().join("local").exists());
    });
}

#[test]
fn inline_directory_request_respects_permission_rule_for_facade_or_member() {
    for tool in ["ask", "request_directory"] {
        with_agent(|agent, tmp| {
            agent
                .always
                .push(serde_json::from_value(json!({"effect":"deny", "tool":tool})).unwrap());
            let read = ToolCall {
                id: "denied".into(),
                name: "read".into(),
                arguments: json!({"path":tmp.join("other.txt")}),
                malformed_arguments: false,
            };
            let mut ui = Answers {
                interactive: true,
                decisions: vec![Decision::Allow],
                ..Default::default()
            };
            let result = agent.dispatch(&mut endpoint(), &read, &mut ui);
            assert_eq!(
                tools::ToolStatus::from_output(&result),
                tools::ToolStatus::Blocked
            );
            assert!(ui.asked.is_empty() && agent.workspace.additional_roots().is_empty());
        });
    }
}

#[test]
fn directory_access_auto_mode_still_requires_real_exact_allow_and_is_discoverable() {
    with_agent(|agent, tmp| {
        let other = tmp.join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(agent.catalog().contains(&"ask".into()));
        assert!(!agent.catalog().contains(&"request_directory".into()));
        let schemas = agent.schemas(&["ask".into()]).unwrap();
        assert_eq!(schemas[0]["function"]["name"], "ask");
        let mut answers = Answers {
            interactive: true,
            decisions: vec![Decision::Deny, Decision::AllowAlways, Decision::Allow],
            ..Default::default()
        };
        let mut backend = endpoint();
        assert!(
            agent
                .dispatch(&mut backend, &call(&other, false), &mut answers)
                .is_error
        );
        assert!(agent.workspace.resolve(other.to_str().unwrap()).is_err());
        assert!(
            agent
                .dispatch(&mut backend, &call(&other, false), &mut answers)
                .is_error
        );
        assert!(
            !agent
                .dispatch(&mut backend, &call(&other, false), &mut answers)
                .is_error
        );
        assert_eq!(answers.asked.len(), 3);
        assert!(
            answers
                .asked
                .iter()
                .all(|(_, s)| s.contains(other.canonicalize().unwrap().to_str().unwrap()))
        );
        assert!(
            agent
                .workspace
                .resolve(other.join("new").to_str().unwrap())
                .is_ok()
        );
    });
}

#[test]
fn directory_access_sensitive_requires_second_approval_and_never_unlocks_siblings() {
    with_agent(|agent, tmp| {
        let protected = tmp.join("private");
        let selected = protected.join("sessions");
        let sibling = protected.join("other");
        std::fs::create_dir_all(&selected).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        agent.workspace = agent.workspace.clone().with_protected_paths(&[protected]);
        let mut deny = Answers {
            interactive: true,
            decisions: vec![Decision::Allow, Decision::Deny],
            ..Default::default()
        };
        assert!(
            agent
                .request_directory(&call(&selected, true), &mut deny)
                .is_error
        );
        assert!(agent.workspace.resolve(selected.to_str().unwrap()).is_err());
        let mut allow = Answers {
            interactive: true,
            decisions: vec![Decision::Allow, Decision::Allow],
            ..Default::default()
        };
        let output = agent.request_directory(&call(&selected, true), &mut allow);
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(
            allow
                .asked
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>(),
            vec!["request_directory", "request_sensitive_directory"]
        );
        assert!(allow.asked[1].1.contains("sandboxed shells remain blocked"));
        assert!(
            agent
                .workspace
                .resolve(selected.join("new").to_str().unwrap())
                .is_ok()
        );
        assert!(agent.workspace.resolve(sibling.to_str().unwrap()).is_err());
    });
}

#[test]
fn directory_access_unattended_and_hermetic_never_grant() {
    with_agent(|agent, tmp| {
        let mut answers = Answers {
            decisions: vec![Decision::Allow],
            ..Default::default()
        };
        assert!(
            agent
                .request_directory(&call(tmp, false), &mut answers)
                .is_error
        );
        answers.interactive = true;
        agent.hermetic = true;
        assert!(
            agent
                .request_directory(&call(tmp, false), &mut answers)
                .is_error
        );
        assert!(answers.asked.is_empty());
    });
}

#[test]
fn directory_access_reusable_answers_explain_the_one_time_rule_and_grant_nothing() {
    with_agent(|agent, tmp| {
        let other = tmp.join("other");
        std::fs::create_dir(&other).unwrap();
        for decision in [Decision::AllowAlways, Decision::AllowProject] {
            let mut answers = Answers {
                interactive: true,
                decisions: vec![decision],
                ..Default::default()
            };
            let output = agent.request_directory(&call(&other, false), &mut answers);
            assert!(output.is_error);
            assert!(
                output.text.starts_with("directory access denied: ")
                    && output.text.contains("choose Allow (once)"),
                "{}",
                output.text
            );
            assert!(agent.workspace.resolve(other.to_str().unwrap()).is_err());
        }
        let mut deny = Answers {
            interactive: true,
            decisions: vec![Decision::Deny],
            ..Default::default()
        };
        let output = agent.request_directory(&call(&other, false), &mut deny);
        assert_eq!(output.text, "directory access denied");
    });
}
