use super::*;
use crate::config::types::{Config, Version};
use crate::llm::ToolCall;
use crate::llm::provider::{Backend, ProviderKind};
use serde_json::json;
use std::path::PathBuf;

#[derive(Default)]
struct Answers {
    answers: Vec<Decision>,
    asked: Vec<(String, String)>,
    notices: Vec<String>,
}

impl Ui for Answers {
    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        self.asked.push((tool.to_owned(), summary.to_owned()));
        self.answers.remove(0)
    }
    fn notice(&mut self, message: &str) {
        self.notices.push(message.to_owned());
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

#[test]
fn plan_review_re_asks_with_the_plan_stored_by_exit_plan() {
    let config = Config::default();
    let workspace = tempfile::tempdir().unwrap();
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent =
        Agent::hermetic(&config, endpoint(), &version, workspace.path(), None, None).unwrap();
    // Hermetic agents refuse plan mode through `set_plan_mode`; the flag is set directly so the
    // exit_plan path under test runs.
    agent.plan_mode = true;
    let mut backend = endpoint();
    let call = ToolCall {
        id: "call-1".into(),
        name: "exit_plan".into(),
        arguments: json!({"plan": "PLAN"}),
        malformed_arguments: false,
    };

    let mut rejecting = Answers {
        answers: vec![Decision::Deny],
        ..Default::default()
    };
    let _ = agent.dispatch(&mut backend, &call, &mut rejecting);
    assert!(agent.plan_mode, "a denied plan keeps plan mode");

    let mut reviewer = Answers {
        answers: vec![Decision::Allow],
        ..Default::default()
    };
    agent.review_plan(&mut reviewer).unwrap();

    assert_eq!(
        reviewer.asked,
        vec![("exit_plan".to_owned(), "PLAN".to_owned())]
    );
    assert!(!agent.plan_mode);
    assert!(
        agent
            .pending_plan
            .as_deref()
            .is_some_and(|plan| plan.starts_with("PLAN"))
    );
    assert_eq!(reviewer.notices, vec!["Plan approved:\nPLAN".to_owned()]);
}
