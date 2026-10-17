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
