use super::ToolOutput;
use crate::agent::Agent;
use crate::config::types::{Config, Version};
use crate::llm::provider::{Backend, ProviderKind};
use std::path::PathBuf;

fn agent<'a>(config: &'a Config, workspace: &std::path::Path) -> Agent<'a> {
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    Agent::hermetic(config, backend, &version, workspace, None, None).unwrap()
}

#[test]
fn every_image_the_tool_wrote_inside_the_workspace_is_previewed_in_order() {
    let config = Config::default();
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(&config, dir.path());
    let root = agent.workspace().root().to_path_buf();
    let outside = tempfile::tempdir().unwrap();
    let output = ToolOutput::ok("wrote".to_owned()).with_changes([
        root.join("art/fox.png"),
        root.join("notes.txt"),
        outside.path().join("elsewhere.png"),
        root.join("art/fox-2.png"),
    ]);
    assert_eq!(
        agent.written_images(&output, Some("art/fox.png")),
        [PathBuf::from("art/fox.png"), PathBuf::from("art/fox-2.png")]
    );
    // An output without recorded changes falls back to the requested file.
    assert_eq!(
        agent.written_images(&ToolOutput::ok("wrote".to_owned()), Some("fox.png")),
        [PathBuf::from("fox.png")]
    );
    assert!(
        agent
            .written_images(&ToolOutput::ok("wrote".to_owned()), None)
            .is_empty()
    );
}
