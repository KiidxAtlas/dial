use super::strip_scratchpad;

#[test]
fn strips_scratchpad_blocks_and_keeps_visible_text() {
    assert_eq!(
        strip_scratchpad("<scratchpad>secret</scratchpad>\nHello"),
        "Hello"
    );
    assert_eq!(
        strip_scratchpad("A<scratchpad>x</scratchpad>B<scratchpad>y</scratchpad>C"),
        "ABC"
    );
    assert_eq!(strip_scratchpad("plain reply"), "plain reply");
}

#[test]
fn unterminated_scratchpad_hides_the_remainder() {
    assert_eq!(strip_scratchpad("shown<scratchpad>private"), "shown");
}

#[test]
fn toggling_rebuilds_the_system_prompt_for_the_next_request() {
    use crate::agent::Agent;
    use crate::config::types::{Config, Version};
    use crate::llm::provider::{Backend, ProviderKind};
    let config = Config {
        model: "test".into(),
        ..Config::default()
    };
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
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
    let has_instruction = |agent: &Agent<'_>| {
        agent.system_messages().iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains(super::INSTRUCTION.trim()))
        })
    };
    assert!(!has_instruction(&agent));
    agent.set_external_thinking(true);
    assert!(agent.external_thinking_enabled() && has_instruction(&agent));
    agent.set_external_thinking(false);
    assert!(!agent.external_thinking_enabled() && !has_instruction(&agent));
}
