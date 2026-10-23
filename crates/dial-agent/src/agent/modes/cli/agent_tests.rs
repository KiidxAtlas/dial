use std::path::PathBuf;

use super::Launch;
use crate::agent::{Agent, Backend, ProviderKind, Version};
use crate::config::types::Config;

/// Build a hermetic agent over an unreachable endpoint. These tests inspect offered
/// tools and the system prompt only, so no model request is made.
fn with_agent<T>(f: impl FnOnce(&mut Agent<'_>) -> T) -> T {
    let config = Config::default();
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
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
        Agent::hermetic(&config, backend, &version, workspace.path(), None, None).unwrap();
    f(&mut agent)
}

#[test]
fn system_prompt_flag_replaces_the_guidance_in_the_system_message() {
    with_agent(|agent| {
        agent
            .set_launch(Launch {
                system_prompt: Some("LAUNCH_PROMPT_MARKER".into()),
                ..Launch::default()
            })
            .unwrap();
        let text = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(text.contains("LAUNCH_PROMPT_MARKER"), "{text}");
    });
}

#[test]
fn tools_allowlist_removes_unlisted_tools_from_the_offered_set() {
    with_agent(|agent| {
        agent
            .set_launch(Launch {
                tools: Some(["read".to_string()].into_iter().collect()),
                ..Launch::default()
            })
            .unwrap();
        let names = agent.offered();
        assert!(!names.iter().any(|n| n == "bash"), "{names:?}");
        assert!(!names.iter().any(|n| n == "find"), "{names:?}");
        assert!(!names.iter().any(|n| n == "task"), "{names:?}");
    });
}

#[test]
fn no_skills_withholds_the_skill_tools_and_catalog() {
    with_agent(|agent| {
        agent
            .set_launch(Launch {
                skills: Some(Default::default()),
                ..Launch::default()
            })
            .unwrap();
        assert!(!agent.offered().iter().any(|n| n == "skill"));
        let text = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(!text.contains("## Skills"), "{text}");
    });
}

#[test]
fn deferred_tools_are_checked_before_changing_launch_or_prompt() {
    with_agent(|agent| {
        let prior = agent.parts.overrides.0.clone();
        let error = agent
            .set_launch(Launch {
                tools: Some(["mcp__demo__invented".into()].into()),
                deferred_tools: ["mcp__demo__invented".into(), "extension_invented".into()].into(),
                system_prompt: Some("must not apply".into()),
                ..Default::default()
            })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("mcp__demo__invented") && error.contains("extension_invented"),
            "{error}"
        );
        assert!(agent.modes.launch.tools.is_none());
        assert_eq!(agent.parts.overrides.0, prior);
    });
}

#[test]
fn registered_deferred_tools_are_allowed_without_bypassing_approval() {
    use crate::tools::integrations::ext::{Tool, ToolContext};
    struct DynamicTool;
    impl Tool for DynamicTool {
        fn name(&self) -> &str {
            "mcp__demo__FetchItem"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"function", "function":{"name":self.name(), "parameters":{"type":"object"}}})
        }
        fn needs_approval(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn call(
            &self,
            _: &ToolContext,
            _: &serde_json::Value,
        ) -> crate::runtime::tool_output::ToolOutput {
            panic!("name validation must not execute a tool")
        }
    }
    with_agent(|agent| {
        agent.ext.register(std::sync::Arc::new(DynamicTool));
        agent
            .set_launch(Launch {
                tools: Some(["mcp__demo__FetchItem".into()].into()),
                deferred_tools: ["mcp__demo__FetchItem".into()].into(),
                ..Default::default()
            })
            .unwrap();
        assert!(agent.tool_eligible("mcp__demo__FetchItem"));
        assert!(!agent.offered().contains(&"bash".into()));
        assert!(
            agent
                .ext
                .get("mcp__demo__FetchItem")
                .unwrap()
                .needs_approval(&serde_json::json!({}))
        );
        // RPC swaps writers; ACP also forks/replaces the tool session. Both retain launch restrictions.
        let next_tools = agent.fresh_session_tools();
        agent.replace_session_tools(next_tools);
        agent.swap_session(crate::storage::session::Session::ephemeral());
        assert!(agent.tool_eligible("mcp__demo__FetchItem"));
        assert!(!agent.offered().contains(&"bash".into()));
    });
}

#[test]
fn no_tools_survives_protocol_session_switches() {
    with_agent(|agent| {
        agent
            .set_launch(Launch {
                tools: Some(Default::default()),
                ..Default::default()
            })
            .unwrap();
        agent.swap_session(crate::storage::session::Session::ephemeral());
        assert!(!agent.offered().contains(&"read".into()));
        assert!(!agent.offered().contains(&"bash".into()));
        assert!(agent.modes.launch.deferred_tools.is_empty());
    });
}
