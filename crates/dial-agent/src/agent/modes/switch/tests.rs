use super::*;
use crate::agent::NullUi;
use crate::config::types::Version;
use crate::llm::model_config::ApiProtocol;
use crate::llm::provider::{Backend, ProviderKind};
use serde_json::json;
use std::path::PathBuf;

fn reply(text: &str) -> crate::llm::fake::Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

#[test]
fn switch_applies_to_later_turns_and_leaves_the_config_untouched() {
    let config = Config::default();
    let server = crate::llm::fake::serve(vec![reply("one"), reply("two")]);
    let base_url = format!("{}/v1", server.url);
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: base_url.clone(),
            model: "alpha".into(),
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
    agent.turn("first", &mut NullUi).unwrap();

    let selection = ModelSelection {
        provider: "switch-test".into(),
        model: "beta".into(),
        base_url: Some(base_url),
        protocol: Some(ApiProtocol::ChatCompletions),
        ..Default::default()
    };
    agent.switch_model(&selection, None).unwrap();
    agent.turn("second", &mut NullUi).unwrap();

    assert_eq!(agent.effective_config().selection().model, "beta");
    assert_eq!(config.selection(), Config::default().selection());
    drop(agent);
    let requests = server.requests();
    assert_eq!(crate::llm::fake::body(&requests[0])["model"], "alpha");
    assert_eq!(crate::llm::fake::body(&requests[1])["model"], "beta");
}

#[test]
fn parse_target_splits_only_known_effort_levels() {
    assert_eq!(
        parse_target("anthropic/claude:high"),
        Target {
            selector: "anthropic/claude".into(),
            effort: Some(ReasoningEffort::High)
        }
    );
    assert_eq!(
        parse_target("model:v2"),
        Target {
            selector: "model:v2".into(),
            effort: None
        }
    );
}

#[test]
fn set_model_accepts_only_known_models() {
    let mut config = Config::default();
    config.model_favorites.push(ModelSelection {
        provider: "lab".into(),
        model: "pinned-1".into(),
        ..Default::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let known = ModelSelection {
        provider: "lab".into(),
        model: "pinned-1".into(),
        ..Default::default()
    };
    let unknown = ModelSelection {
        provider: "lab".into(),
        model: "zz-unlisted-9000".into(),
        ..Default::default()
    };
    assert!(is_known_model(&config, dir.path(), &known));
    assert!(!is_known_model(&config, dir.path(), &unknown));
}
