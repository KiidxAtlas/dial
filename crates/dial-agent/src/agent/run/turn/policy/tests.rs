use super::*;
use crate::config::types::{Config, Version};
use crate::llm::provider::{Backend, ProviderKind};

fn test_backend() -> Backend {
    Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    )
}

#[test]
fn hermetic_policy_keeps_batch_mode_and_disables_lightweight() {
    let config = Config {
        model: "test".into(),
        ..Config::default()
    };
    let mut backend = test_backend();
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
        Agent::hermetic(&config, test_backend(), &version, workspace.path(), None, None).unwrap();
    let mut ui = NullUi;
    let turn = agent
        .begin_turn(&mut backend, &[], 5, "test goal", &mut ui)
        .unwrap();
    let mut control = json!({});
    let resolved = agent.resolve_request_policy(&turn, &backend, 0, &mut control);
    assert!(!resolved.single_call);
    assert!(!resolved.measured_single);
    assert!(!resolved.lightweight);
    assert_eq!(control["reliability"]["call_policy"]["mode"], "batch");
    assert_eq!(control["reliability"]["call_policy"]["reasons"], json!([]));
}
