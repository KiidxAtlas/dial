use super::*;
#[test]
fn structured_output_defaults_by_provider_and_explicit_none_disables() {
    let caps = ModelCapabilities::default();
    assert_eq!(
        structured_output_for("lm-studio", "m", &caps),
        StructuredOutput::JsonSchema
    );
    assert_eq!(
        structured_output_for("ollama", "m", &caps),
        StructuredOutput::OllamaFormat
    );
    assert_eq!(
        structured_output_for("mlx", "m", &caps),
        StructuredOutput::Disabled
    );
    let off = ModelCapabilities {
        structured_output: Some(StructuredOutput::Disabled),
        ..Default::default()
    };
    assert_eq!(
        structured_output_for("lm-studio", "m", &off),
        StructuredOutput::Disabled
    );
}

#[test]
fn detected_llama_server_defaults_to_json_schema_unless_the_profile_declares_otherwise() {
    let mut profile = ModelProfile::minimal("openai", "qwen");
    assert_eq!(profile.structured_output(), StructuredOutput::Disabled);
    profile.endpoint_structured_output = Some(StructuredOutput::JsonSchema);
    assert_eq!(profile.structured_output(), StructuredOutput::JsonSchema);
    profile.capabilities.structured_output = Some(StructuredOutput::Disabled);
    assert_eq!(profile.structured_output(), StructuredOutput::Disabled);

    profile.capabilities.structured_output = None;
    profile.tool_repair = true;
    let mut body = serde_json::json!({"model": "qwen", "messages": [], "tools": [
        {"type": "function", "function": {"name": "read_file", "parameters": {
            "type": "object", "properties": {"path": {"type": "string"}}}}}
    ]});
    profile.apply_openai(&mut body);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert!(body.get("tools").is_none());
}

#[test]
fn local_gguf_models_get_json_schema_repair_and_llama_cpp_sampling_names() {
    let root = tempfile::tempdir().unwrap();
    let gguf = root.path().join("model.gguf");
    std::fs::write(&gguf, b"GGUF\x03\0\0\0").unwrap();
    let model = gguf.to_string_lossy().into_owned();
    let caps = ModelCapabilities::default();
    assert_eq!(
        structured_output_for("mlx", &model, &caps),
        StructuredOutput::JsonSchema
    );
    let mut profile = ModelProfile::minimal("mlx", &model);
    profile.local_sampling = Some(crate::llm::local_sampling::LocalSampling {
        repetition_penalty: Some(1.1),
        repetition_context_size: Some(64),
        presence_context_size: Some(32),
        top_k: Some(20),
        ..Default::default()
    });
    let mut body = serde_json::json!({"messages": []});
    profile.apply_openai(&mut body);
    assert_eq!(body["repeat_penalty"], 1.1);
    assert_eq!(body["repeat_last_n"], 64);
    assert_eq!(body["top_k"], 20);
    assert!(body.get("repetition_penalty").is_none());
    assert!(body.get("presence_context_size").is_none());
}

#[test]
fn repair_flag_adds_response_format_only_for_the_retry() {
    let base = serde_json::json!({
        "model": "m",
        "messages": [],
        "tools": [{"type": "function", "function": {"name": "read", "parameters": {
            "type": "object", "properties": {"path": {"type": "string"}}}}}],
        "tool_choice": "auto",
        "parallel_tool_calls": true
    });
    let mut body = base.clone();
    ModelProfile::minimal("lm-studio", "m").apply_openai(&mut body);
    assert!(body.get("response_format").is_none());
    assert!(body.get("tools").is_some());

    let mut repair = ModelProfile::minimal("lm-studio", "m");
    repair.tool_repair = true;
    let mut body = base;
    repair.apply_openai(&mut body);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("parallel_tool_calls").is_none());
}

#[test]
fn legacy_cache_booleans_and_retention_round_trip_without_affecting_other_providers() {
    for value in [true, false] {
        let text = format!(
            "provider = 'anthropic'\nmodel = 'exact'\nprompt_cache = {value}\n[capabilities]\nprompt_cache = true\n"
        );
        let profile: ModelProfile = toml::from_str(&text).unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.prompt_cache, Some(value));
        let restored: ModelProfile = toml::from_str(&toml::to_string(&profile).unwrap()).unwrap();
        assert_eq!(restored.prompt_cache, Some(value));
    }
    let profile: ModelProfile =
        toml::from_str("provider = 'anthropic'\nmodel = 'exact'\ncache_retention = 'long'\n")
            .unwrap();
    assert_eq!(profile.prompt_cache, None);
    assert_eq!(profile.cache_retention, CacheRetention::Long);
    let restored: ModelProfile = toml::from_str(&toml::to_string(&profile).unwrap()).unwrap();
    assert_eq!(restored.prompt_cache, None);
    assert_eq!(restored.cache_retention, CacheRetention::Long);
    let req = super::super::Chat {
        messages: vec![],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 8192,
        seed: None,
    };
    profile.validate_request(&req, true).unwrap();
    assert!(profile.validate_request(&req, false).is_err());
    assert!(toml::from_str::<ModelProfile>("cache_retention = 'forever'").is_err());
}
#[test]
fn unknown_capabilities_do_not_enable_controls() {
    let p = ModelProfile {
        provider: "custom".into(),
        model: "o3-not-a-known-model".into(),
        temperature: Some(0.2),
        ..Default::default()
    };
    assert!(p.validate().is_err());
}
#[test]
fn explicit_reasoning_profile_omits_unsupported_sampling() {
    let p = ModelProfile {
        provider: "openai".into(),
        model: "explicit".into(),
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::OpenaiEffort,
            reasoning_efforts: vec![ReasoningEffort::High],
            output_limit: OutputLimit::MaxCompletionTokens,
            ..Default::default()
        },
        reasoning_effort: Some(ReasoningEffort::High),
        ..Default::default()
    };
    p.validate().unwrap();
    let mut body = serde_json::json!({"temperature":0.2,"max_tokens":8192,"seed":1});
    p.apply_openai(&mut body);
    assert_eq!(
        body,
        serde_json::json!({"max_completion_tokens":8192,"reasoning_effort":"high"})
    );
}
#[test]
fn named_profiles_are_explicit_and_migrate_with_model_selection() {
    use crate::config::types::{Config, EditMode};
    let mut config = Config {
        provider: Some("custom".into()),
        model: "exact".into(),
        ..Default::default()
    };
    let fast = ModelProfile {
        provider: "custom".into(),
        model: "exact".into(),
        max_tokens: Some(16000),
        edit_mode: Some(EditMode::Hashline),
        tools: Some(vec!["read".into()]),
        ..Default::default()
    };
    config.model_profiles.insert("fast".into(), fast.clone());
    config.model_profiles.insert(
        "deep".into(),
        ModelProfile {
            max_tokens: Some(32000),
            ..fast
        },
    );
    assert!(
        config.active_model_profile().is_none(),
        "ambiguous profiles never select by map order"
    );
    config.model_profile = Some("fast".into());
    let harness = config.effective_harness(&config.harness).unwrap();
    assert_eq!(harness.max_tokens, 16000);
    assert_eq!(harness.edit_mode, Some(EditMode::Hashline));
    assert_eq!(harness.tools, vec!["read"]);
    let switched = config.with_selection(&super::super::model_config::ModelSelection {
        provider: "custom".into(),
        model: "different".into(),
        ..Default::default()
    });
    assert!(switched.model_profile.is_none());
}
#[test]
fn thinking_toggle_preserves_budget_but_rejects_unsupported_effort() {
    let mut profile = ModelProfile {
        provider: "anthropic".into(),
        model: "exact".into(),
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::AnthropicBudget,
            ..Default::default()
        },
        thinking: Some(false),
        thinking_budget: Some(1024),
        max_tokens: Some(4096),
        ..Default::default()
    };
    profile.validate().unwrap();
    profile.thinking = Some(true);
    profile.validate().unwrap();
    profile.reasoning_effort = Some(ReasoningEffort::High);
    assert!(profile.validate().is_err());
    profile.capabilities.reasoning_efforts = vec![ReasoningEffort::High];
    profile.validate().unwrap();
}
#[test]
fn unsupported_tools_can_be_removed_without_claiming_model_capabilities() {
    let mut config = crate::config::types::Config {
        provider: Some("p".into()),
        model: "m".into(),
        ..Default::default()
    };
    config.model_profiles.insert(
        "plain".into(),
        ModelProfile {
            provider: "p".into(),
            model: "m".into(),
            capabilities: ModelCapabilities {
                tools: Some(false),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert!(
        config
            .effective_harness(&config.harness)
            .unwrap()
            .tools
            .is_empty()
    );
}

#[test]
fn non_default_tier_without_declared_profile_reaches_the_request_body() {
    use crate::config::types::Config;
    use crate::llm::{Chat, Control, Endpoint, chat_at_controlled, fake};
    use serde_json::json;

    let server = fake::serve(vec![fake::json(
        200,
        &json!({"choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": "ok"}}]}),
    )]);
    let config = Config {
        provider: Some("openai".into()),
        model: "gpt-test".into(),
        service_tier: "priority".into(),
        ..Config::default()
    };
    let selection = config.selection();
    let profile =
        request_tiers_for(None, &selection, config.effective_service_tier(), false).unwrap();
    assert!(
        profile.is_some(),
        "a non-default tier needs a request profile"
    );
    let req = Chat {
        messages: vec![json!({"role": "user", "content": "hi"})],
        tools: Vec::new(),
        temperature: 0.2,
        max_tokens: 64,
        seed: None,
    };
    chat_at_controlled(
        Endpoint {
            url: &server.url,
            auth: None,
            model: "gpt-test",
            headers: &[],
        },
        &req,
        5,
        Control {
            interrupt: None,
            profile: profile.as_ref(),
            reasoning_guard: None,
            on_event: &mut |_| true,
        },
    )
    .unwrap();
    let requests = server.requests();
    let body = fake::body(&requests[0]);
    assert_eq!(body["service_tier"], "priority");
    assert_eq!(body["temperature"], 0.2);
}

#[test]
fn local_hints_fill_gaps_never_override_profile_values_and_stay_off_hosted_routes() {
    use crate::llm::local_hints::{LocalHints, LocalServer};
    use serde_json::json;
    let hints = LocalHints {
        thinking: Some(true),
        keep_alive_secs: Some(60),
        cache_slot: Some(2),
        prompt_progress: true,
        presence_penalty: Some(0.9),
        repetition_penalty: Some(1.3),
    };
    // The profile's own presence penalty wins; the hint fills the missing LM Studio names only.
    let mut lm = ModelProfile::minimal("lm-studio", "m");
    lm.local_sampling = Some(crate::llm::local_sampling::LocalSampling {
        presence_penalty: Some(0.2),
        ..Default::default()
    });
    lm.local_hints = hints.clone();
    lm.local_server = Some(LocalServer::LmStudio);
    lm.validate().unwrap();
    let mut body = json!({"model":"m","max_tokens":8});
    lm.apply_openai(&mut body);
    assert_eq!(body["presence_penalty"], 0.2);
    assert_eq!(body["repeat_penalty"], 1.3);
    assert_eq!(body["ttl"], 60);
    assert!(body.get("cache_prompt").is_none());

    // An explicit `thinking` owns the thinking switch, so the hint's enable_thinking is dropped.
    let mut llama = ModelProfile::minimal("llama-cpp", "m");
    llama.thinking = Some(false);
    llama.local_hints = hints.clone();
    llama.local_server = Some(LocalServer::LlamaCpp);
    // An explicit `thinking` on a profile without native reasoning is rejected by validate()
    // (existing rule), so the hint-precedence case applies the profile directly.
    let mut body = json!({"model":"m","max_tokens":8});
    llama.apply_openai(&mut body);
    assert_eq!(body["reasoning_effort"], "none");
    assert!(body.get("chat_template_kwargs").is_none());
    assert_eq!(body["id_slot"], 2);
    assert_eq!(body["return_progress"], true);

    // Hosted: a profile without a local server never receives hint fields.
    let mut hosted = ModelProfile::minimal("openai", "gpt-x");
    hosted.local_hints = hints;
    let mut body = json!({"model":"gpt-x","max_tokens":8});
    hosted.apply_openai(&mut body);
    for key in [
        "cache_prompt",
        "id_slot",
        "return_progress",
        "ttl",
        "chat_template_kwargs",
        "presence_penalty",
        "repeat_penalty",
        "repetition_penalty",
        "think",
        "keep_alive",
    ] {
        assert!(body.get(key).is_none(), "{key} reached a hosted route");
    }
}
