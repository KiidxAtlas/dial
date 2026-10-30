use crate::config::thinking::CliThinking;
use crate::config::thinking::apply_thinking;
use crate::config::thinking::parse_thinking;

use super::*;
use crate::llm::profiles::ReasoningEffort;

#[test]
fn thinking_accepts_selectors_and_unambiguous_prefixes() {
    assert_eq!(parse_thinking("off"), Ok(CliThinking::Off));
    assert_eq!(parse_thinking("minimal"), Ok(CliThinking::Minimal));
    assert_eq!(parse_thinking("xhigh"), Ok(CliThinking::XHigh));
    assert_eq!(parse_thinking("auto"), Ok(CliThinking::Auto));
    assert_eq!(parse_thinking("med"), Ok(CliThinking::Medium));
    assert_eq!(parse_thinking("ma"), Ok(CliThinking::Max));
    assert_eq!(parse_thinking("xh"), Ok(CliThinking::XHigh));
}

#[test]
fn thinking_rejects_unknown_short_and_case_variants() {
    assert!(parse_thinking("").is_err());
    assert!(parse_thinking("m").is_err());
    assert!(parse_thinking("galaxy").is_err());
    assert!(parse_thinking("HIGH").is_err());
    assert!(parse_thinking("inherit").is_err());
}

#[test]
fn thinking_maps_to_reasoning_effort() {
    assert_eq!(
        CliThinking::Off.reasoning_effort(),
        Some(ReasoningEffort::None)
    );
    assert_eq!(
        CliThinking::XHigh.reasoning_effort(),
        Some(ReasoningEffort::Xhigh)
    );
    assert_eq!(CliThinking::Auto.reasoning_effort(), None);
}

#[test]
fn models_split_and_trim() {
    assert_eq!(
        parse_models(" a/b , c ,d").unwrap(),
        ModelPatterns(vec!["a/b".into(), "c".into(), "d".into()])
    );
    assert!(parse_models("").is_err());
    assert!(parse_models("a,,b").is_err());
    assert!(parse_models("a, ").is_err());
}

#[test]
fn provider_charset() {
    assert_eq!(parse_provider("mlx").unwrap(), "mlx");
    assert_eq!(parse_provider("open-ai.v1_x").unwrap(), "open-ai.v1_x");
    assert!(parse_provider("").is_err());
    assert!(parse_provider("bad name").is_err());
    assert!(parse_provider("a/b").is_err());
}

#[test]
fn api_key_rejects_empty_whitespace_and_control() {
    assert!(parse_api_key("sk-abc").is_ok());
    assert!(parse_api_key("").is_err());
    assert!(parse_api_key("sk abc").is_err());
    assert!(parse_api_key("sk\u{7}").is_err());
    assert_eq!(
        format!("{:?}", parse_api_key("sk-secret").unwrap()),
        "ApiKey(<redacted>)"
    );
}

#[test]
fn role_model_trims_and_bounds() {
    assert_eq!(parse_role_model("  qwen-small ").unwrap(), "qwen-small");
    assert!(parse_role_model("   ").is_err());
    assert!(parse_role_model(&"m".repeat(513)).is_err());
    assert!(parse_role_model("a\nb").is_err());
}

#[test]
fn service_tier_matches_openai_set() {
    assert_eq!(parse_service_tier("priority"), Ok(ServiceTier::Priority));
    assert_eq!(parse_service_tier("ultrafast"), Ok(ServiceTier::Ultrafast));
    assert_eq!(parse_service_tier("none"), Ok(ServiceTier::None));
    assert!(parse_service_tier("turbo").is_err());
    assert!(parse_service_tier("Priority").is_err());
}

#[test]
fn max_time_accepts_seconds_and_units() {
    assert_eq!(parse_max_time("30"), Ok(30.0));
    assert_eq!(parse_max_time("1.5"), Ok(1.5));
    assert_eq!(parse_max_time("5s"), Ok(5.0));
    assert_eq!(parse_max_time("10m"), Ok(600.0));
    assert_eq!(parse_max_time("1h"), Ok(3600.0));
    assert_eq!(parse_max_time("2.5m"), Ok(150.0));
    assert_eq!(parse_max_time(" 5s "), Ok(5.0));
}

#[test]
fn max_time_rejects_nonpositive_and_malformed() {
    for bad in [
        "",
        "0",
        "0s",
        "-5",
        "5x",
        "5S",
        ".5s",
        "1e3s",
        "abc",
        "inf",
        "NaN",
        "1h2m",
        "100000000000000000000",
        "2000000000h",
    ] {
        assert!(parse_max_time(bad).is_err(), "{bad} should be rejected");
    }
}

#[test]
fn api_key_with_invalid_token_fails_before_any_override() {
    let flags = ModelFlags {
        api_key: Some(ApiKey("bad key with spaces".into())),
        ..Default::default()
    };
    assert!(apply(Config::default(), &flags).is_err());
}

#[test]
fn max_time_becomes_a_future_deadline() {
    let flags = ModelFlags {
        max_time: Some(60.0),
        ..Default::default()
    };
    let deadline = flags.deadline().expect("deadline set");
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    assert!(
        remaining > std::time::Duration::from_secs(50),
        "{remaining:?}"
    );
    assert!(ModelFlags::default().deadline().is_none());
    assert!(apply(Config::default(), &flags).is_ok());
}

#[test]
fn service_tier_flag_sets_config_tier() {
    let flags = ModelFlags {
        service_tier: Some(ServiceTier::Priority),
        ..Default::default()
    };
    let config = apply(Config::default(), &flags).expect("service tier applies");
    assert_eq!(config.service_tier, "priority");
}

#[test]
fn thinking_without_declared_profile_is_an_error() {
    let flags = ModelFlags {
        thinking: Some(CliThinking::High),
        ..Default::default()
    };
    let err = apply(Config::default(), &flags).unwrap_err().to_string();
    assert!(err.contains("model_profiles"), "{err}");
}

#[test]
fn openrouter_thinking_off_reaches_request_without_manual_profile() {
    let config = Config {
        provider: Some("openrouter".into()),
        model: "anthropic/claude-haiku-5.5".into(),
        ..Default::default()
    };
    let original = config.clone();
    let configured = apply_thinking(config, CliThinking::Off).unwrap();
    let mut wire = serde_json::json!({"model":configured.model,"temperature":0.2});
    configured
        .active_model_profile()
        .unwrap()
        .apply_openai(&mut wire);
    assert_eq!(wire["reasoning_effort"], "none");
    assert!(original.model_profiles.is_empty());
    assert!(apply_thinking(original, CliThinking::High).is_err());
}

#[test]
fn anthropic_thinking_off_uses_disabled_thinking_not_none_effort() {
    let config = Config {
        provider: Some("anthropic".into()),
        model: "claude-sonnet-4-6".into(),
        ..Default::default()
    };
    let configured = apply_thinking(config, CliThinking::Off).unwrap();
    let profile = configured.active_model_profile().unwrap();
    assert_eq!(profile.thinking, Some(false));
    assert_eq!(profile.reasoning_effort, None);
    // The second application exercises the existing-profile branch used by model switching.
    let configured = apply_thinking(configured, CliThinking::Off).unwrap();
    assert_eq!(
        configured.active_model_profile().unwrap().thinking,
        Some(false)
    );
}

#[test]
fn disk_reload_keeps_launch_selection_and_thinking_without_freezing_other_settings() {
    use crate::config::paths::Layout;
    use crate::config::settings_store::SettingsReload;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let path = home.path().join("config.toml");
    std::fs::write(&path, "model = 'anthropic/claude-haiku-5.5'\nprovider = 'openrouter'\n[harness]\nmax_tokens = 8192\n").unwrap();
    let before = Config::load(&layout, None).unwrap();
    let selected = apply(
        before.clone(),
        &ModelFlags {
            thinking: Some(CliThinking::Off),
            service_tier: Some(ServiceTier::Priority),
            ..Default::default()
        },
    )
    .unwrap();
    let mut overlay = toml::Table::new();
    capture_overlay(&mut overlay, &before, &selected).unwrap();
    let mut reload = SettingsReload::new(&layout, &overlay, &selected);
    let (same, errors) = reload.reload(&layout, &overlay, None);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(same.service_tier, "priority");
    let mut wire = serde_json::json!({});
    same.active_model_profile().unwrap().apply_openai(&mut wire);
    assert_eq!(wire["reasoning_effort"], "none");
    std::fs::write(&path, "model = 'anthropic/claude-haiku-5.5'\nprovider = 'openrouter'\n[harness]\nmax_tokens = 8192\ntemperature = 0.4\n").unwrap();
    let (changed, errors) = reload.reload(&layout, &overlay, None);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(changed.harness.temperature, 0.4);
    assert_eq!(changed.service_tier, "priority");
    assert_eq!(
        changed.active_model_profile().unwrap().thinking,
        Some(false)
    );
    assert!(
        !std::fs::read_to_string(path)
            .unwrap()
            .contains("__cli_thinking")
    );
}

#[test]
fn thinking_auto_is_stored_without_a_profile() {
    let flags = ModelFlags {
        thinking: Some(CliThinking::Auto),
        ..Default::default()
    };
    let auto = apply(Config::default(), &flags).unwrap();
    assert!(auto.thinking_auto);
    assert!(auto.model_profile.is_none());
}

#[test]
fn provider_switch_resolves_a_fresh_route_and_preserves_same_provider_overrides() {
    use crate::config::types::EndpointConfig;
    use crate::llm::model_config::ApiProtocol;
    let mut config = Config {
        provider: Some("chatgpt".into()),
        model: "claude-sonnet-4-6".into(),
        endpoint: Some(EndpointConfig {
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            api_key_env: Some("OLD_ROUTE_KEY".into()),
            protocol: Some(ApiProtocol::Responses),
            headers: Some(vec![crate::llm::model_config::RequestHeader {
                name: "X-Old-Route".into(),
                value: crate::llm::model_config::HeaderValue::Literal("stale".into()),
            }]),
            account_id: Some("old-account".into()),
            allow_insecure: true,
        }),
        ..Default::default()
    };
    let selected = apply(
        config.clone(),
        &ModelFlags {
            provider: Some("anthropic".into()),
            ..Default::default()
        },
    )
    .expect("changing providers must discard the previous route");
    let endpoint = selected.endpoint.unwrap();
    assert_eq!(endpoint.base_url, "https://api.anthropic.com/v1");
    assert_eq!(endpoint.protocol, Some(ApiProtocol::Anthropic));
    assert_eq!(endpoint.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    assert!(endpoint.account_id.is_none());
    assert!(endpoint.headers.is_none());
    assert!(!endpoint.allow_insecure);
    // A valid custom API route retains all its metadata when re-selecting the same provider.
    config.provider = Some("openai".into());
    config.endpoint.as_mut().unwrap().account_id = None;
    let unchanged = apply(
        config.clone(),
        &ModelFlags {
            provider: Some("openai".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(unchanged.selection(), config.selection());
}

#[test]
fn provider_switch_survives_config_reload_without_old_endpoint_metadata() {
    use crate::config::paths::Layout;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    std::fs::write(home.path().join("config.toml"), "provider='chatgpt'\nmodel='claude-sonnet-4-6'\n[endpoint]\nbase_url='https://chatgpt.com/backend-api'\nprotocol='responses'\n").unwrap();
    let before = Config::load(&layout, None).unwrap();
    let selected = apply(
        before.clone(),
        &ModelFlags {
            provider: Some("anthropic".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut overlay = toml::Table::new();
    capture_overlay(&mut overlay, &before, &selected).unwrap();
    let reloaded = Config::load(&layout, Some(overlay)).unwrap();
    assert_eq!(reloaded.provider, selected.provider);
    assert_eq!(reloaded.endpoint, selected.endpoint);
    assert_eq!(reloaded.model, selected.model);
}
