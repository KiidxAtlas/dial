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
        name: "request_directory".into(),
        arguments: json!({"path":path,"reason":"test directory access","sensitive":sensitive}),
        malformed_arguments: false,
    }
}

#[test]
fn directory_access_auto_mode_still_requires_real_exact_allow_and_is_discoverable() {
    with_agent(|agent, tmp| {
        let other = tmp.join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(agent.catalog().contains(&"request_directory".into()));
        let schemas = agent.schemas(&["request_directory".into()]).unwrap();
        assert_eq!(schemas[0]["function"]["name"], "request_directory");
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
        assert!(allow.asked[1].1.contains("Shell commands remain blocked"));
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
