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
fn a_weights_driver_runs_the_turn_but_keeps_the_previous_model_as_the_edit_target() {
    let config = Config::default();
    let server = crate::llm::fake::serve(vec![reply("driven")]);
    let base_url = format!("{}/v1", server.url);
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: base_url.clone(),
            model: "local-main".into(),
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
    let local = ModelSelection {
        provider: "switch-test".into(),
        model: "local-main".into(),
        base_url: Some(base_url.clone()),
        protocol: Some(ApiProtocol::ChatCompletions),
        ..Default::default()
    };
    agent.switch_model(&local, None).unwrap();
    let driver = ModelSelection {
        provider: "switch-test".into(),
        model: "frontier".into(),
        base_url: Some(base_url),
        protocol: Some(ApiProtocol::ChatCompletions),
        ..Default::default()
    };
    let target = workspace.path().join("checkpoint");

    let prior = agent.drive_weights_with(&driver, &target).unwrap();
    assert_eq!(prior.as_ref().unwrap().selection().model, "local-main");
    assert_eq!(agent.effective_config().selection().model, "frontier");
    assert_eq!(
        agent.effective_config().weights.model,
        target.to_string_lossy()
    );
    agent.turn("improve", &mut NullUi).unwrap();
    assert_eq!(
        crate::llm::fake::body(&server.requests()[0])["model"],
        "frontier"
    );

    agent.restore_model_override(prior).unwrap();
    assert_eq!(agent.effective_config().selection().model, "local-main");
    assert!(agent.effective_config().weights.model.is_empty());
    let saved = agent
        .modes
        .switch
        .as_ref()
        .expect("the /switch choice is kept");
    assert_eq!(saved.selection.model, "local-main");
    assert_eq!(config.selection(), Config::default().selection());
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

#[test]
fn picker_ids_for_discovered_local_providers_resolve_without_configuration() {
    let config = Config::default();
    for selector in [
        "lm-studio/google/gemma-4-e4b",
        "ollama/qwen3:8b",
        "mlx/mlx-community/Qwen2.5-7B-Instruct-4bit",
    ] {
        let resolved = resolve(&config, selector).unwrap();
        let (provider, model) = selector.split_once('/').unwrap();
        assert_eq!(
            (resolved.provider.as_str(), resolved.model.as_str()),
            (provider, model)
        );
    }
    assert!(resolve(&config, "nowhere/model").is_err());
}

#[test]
fn a_reasoning_level_needs_no_hand_written_profile_when_the_catalog_declares_levels() {
    let config = Config::default();
    let (provider, bundled) = ["openai", "anthropic", "gemini", "xai", "deepseek"]
        .into_iter()
        .find_map(|provider| {
            crate::llm::catalog::bundled_models(provider)
                .into_iter()
                .find(|model| model.thinking_efforts.len() > 1)
                .map(|model| (provider, model))
        })
        .expect("a bundled model with several reasoning levels");
    let selection = ModelSelection {
        provider: provider.into(),
        model: bundled.id.clone(),
        ..Default::default()
    };
    let effort = *bundled
        .thinking_efforts
        .iter()
        .rfind(|effort| **effort != ReasoningEffort::None)
        .unwrap();
    let next = switched_config(&config, &selection, Some(effort)).unwrap();
    assert_eq!(
        next.active_model_profile().unwrap().reasoning_effort,
        Some(effort)
    );
    // Session-only: the operator's configuration gains no profile.
    assert!(config.model_profiles.is_empty());

    let unknown = ModelSelection {
        provider: "nowhere".into(),
        model: "x".into(),
        ..Default::default()
    };
    let refused = switched_config(&config, &unknown, Some(effort)).unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("reasoning metadata is unavailable")
    );
}

#[test]
fn reset_thinking_restores_the_configured_level_and_keeps_the_model() {
    let mut config = Config::default();
    let server = crate::llm::fake::serve(vec![]);
    let base_url = format!("{}/v1", server.url);
    let selection = ModelSelection {
        provider: "switch-test".into(),
        model: "beta".into(),
        base_url: Some(base_url.clone()),
        protocol: Some(ApiProtocol::ChatCompletions),
        ..Default::default()
    };
    let profile: crate::llm::profiles::ModelProfile = serde_json::from_value(json!({
        "provider":"switch-test","model":"beta","reasoning_effort":"low",
        "capabilities":{"reasoning":"openai-effort","reasoning_efforts":["low","high"]}
    }))
    .unwrap();
    config.model_profiles.insert("beta".into(), profile);
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url,
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
    agent.switch_model(&selection, None).unwrap();
    agent
        .switch_model(&selection, Some(ReasoningEffort::High))
        .unwrap();
    let effort = |agent: &Agent| {
        agent
            .effective_config()
            .active_model_profile()
            .unwrap()
            .reasoning_effort
    };
    assert_eq!(effort(&agent), Some(ReasoningEffort::High));

    agent.reset_thinking().unwrap();
    assert_eq!(agent.effective_config().selection().model, "beta");
    assert_eq!(effort(&agent), Some(ReasoningEffort::Low));
    assert_eq!(agent.modes.switch.as_ref().unwrap().effort, None);
}
