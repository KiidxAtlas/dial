use super::*;

#[test]
fn token_pricing_splits_cache_writes_by_ttl() {
    let pricing = TokenPricing {
        prompt: 1e-6,
        completion: 2e-6,
        cache_read: Some(1e-7),
        cache_write: Some(3.75e-6),
    };
    let mut split = crate::llm::Reply {
        prompt_tokens: Some(1_000_000),
        completion_tokens: Some(0),
        cache_creation_tokens: Some(1_000_000),
        ..Default::default()
    };
    split.cache_creation_5m_tokens = Some(400_000);
    split.cache_creation_1h_tokens = Some(600_000);
    // 400k at the 5m rate (3.75e-6) plus 600k at the 1h rate (2x prompt = 2e-6).
    assert!((pricing.reply_cost(&split).unwrap() - 2.7).abs() < 1e-9);
    // Without a split, the whole write prices at the default write rate.
    let flat = crate::llm::Reply {
        cache_creation_5m_tokens: None,
        cache_creation_1h_tokens: None,
        ..split
    };
    assert!((pricing.reply_cost(&flat).unwrap() - 3.75).abs() < 1e-9);
}

#[test]
fn contradictory_cache_write_buckets_leave_cost_unknown() {
    let pricing = TokenPricing {
        prompt: 1e-6,
        completion: 2e-6,
        cache_read: Some(1e-7),
        cache_write: Some(1.25e-6),
    };
    let bundled = bundled_model("anthropic", "claude-opus-4-6")
        .unwrap()
        .list_price
        .unwrap();
    let mut reply = crate::llm::Reply {
        prompt_tokens: Some(100),
        completion_tokens: Some(1),
        cache_creation_tokens: Some(10),
        cache_creation_5m_tokens: Some(6),
        cache_creation_1h_tokens: Some(5),
        ..Default::default()
    };
    assert!(pricing.reply_cost(&reply).is_none());
    assert!(bundled.reply_cost(&reply, 0).is_none());
    reply.cache_creation_tokens = Some(u64::MAX);
    reply.prompt_tokens = Some(u64::MAX);
    reply.cache_creation_5m_tokens = Some(u64::MAX);
    reply.cache_creation_1h_tokens = Some(1);
    assert!(pricing.reply_cost(&reply).is_none());
    assert!(bundled.reply_cost(&reply, 0).is_none());
    // Partial buckets are valid: the remaining writes retain the default TTL price.
    reply.prompt_tokens = Some(100);
    reply.cache_creation_tokens = Some(10);
    reply.cache_creation_5m_tokens = Some(6);
    reply.cache_creation_1h_tokens = Some(3);
    assert!(pricing.reply_cost(&reply).is_some());
    assert!(bundled.reply_cost(&reply, 0).is_some());
}
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
};
#[test]
fn picker_capabilities_preserve_declarations_and_unknown_cached_fields() {
    let info = metadata(&serde_json::json!({"capabilities":["vision","tools","thinking"]}));
    assert_eq!(
        (info.vision, info.tools, info.reasoning),
        (Some(true), Some(true), Some(true))
    );
    let info = metadata(
        &serde_json::json!({"supported_parameters":["temperature","tools","reasoning_effort"]}),
    );
    assert_eq!((info.tools, info.reasoning), (Some(true), Some(true)));
    let info = metadata(&serde_json::json!({"tools":false,"supported_parameters":["tools"]}));
    assert_eq!(info.tools, Some(false));
    let info: ModelMetadata = serde_json::from_value(serde_json::json!({"vision":true})).unwrap();
    assert_eq!((info.tools, info.reasoning), (None, None));
    assert_eq!(
        metadata(&serde_json::json!({"id":"vision-reasoning-tools-model"})).tools,
        None
    );
}

#[test]
fn model_kind_policy_is_shared_and_serialized_snake_case() {
    assert!(ModelKind::Chat.supports_chat());
    assert!(ModelKind::Unknown.supports_chat());
    for kind in [
        ModelKind::Embedding,
        ModelKind::Rerank,
        ModelKind::Image,
        ModelKind::Audio,
    ] {
        assert!(!kind.supports_chat(), "{}", kind.label());
    }
    assert_eq!(ModelKind::default(), ModelKind::Unknown);
    assert_eq!(ModelKind::Embedding.label(), "embedding");
    assert_eq!(
        serde_json::to_string(&ModelKind::Rerank).unwrap(),
        "\"rerank\""
    );
    let entry = |kind| ModelEntry {
        selection: ModelSelection::default(),
        label: "m".into(),
        source: "s".into(),
        available: true,
        metadata: ModelMetadata {
            kind,
            ..Default::default()
        },
    };
    assert!(entry(ModelKind::Embedding).is_embedding());
    assert!(!entry(ModelKind::Embedding).supports_chat());
    assert!(!entry(ModelKind::Chat).is_embedding());
    assert!(entry(ModelKind::Unknown).supports_chat());
    // Cache rows written before the field existed read back as Unknown.
    let legacy: ModelMetadata = serde_json::from_str(r#"{"vision":true}"#).unwrap();
    assert_eq!(legacy.kind, ModelKind::Unknown);
}

#[test]
fn declared_kinds_match_only_exact_documented_tokens() {
    assert_eq!(kind_from_declared("embeddings"), ModelKind::Embedding);
    assert_eq!(
        kind_from_declared("feature-extraction"),
        ModelKind::Embedding
    );
    assert_eq!(kind_from_declared("llm"), ModelKind::Chat);
    assert_eq!(kind_from_declared("text-generation"), ModelKind::Chat);
    assert_eq!(kind_from_declared("text-to-image"), ModelKind::Image);
    assert_eq!(kind_from_declared("text-to-speech"), ModelKind::Audio);
    assert_eq!(kind_from_declared("reranker"), ModelKind::Rerank);
    // Names and near-misses are never classified by their words.
    for token in [
        "Embedding Model",
        "my-embeddings-llm",
        "text-embedding-3-small",
        "",
    ] {
        assert_eq!(kind_from_declared(token), ModelKind::Unknown, "{token}");
    }
    let kind_of = |json: Value| metadata(&json).kind;
    assert_eq!(
        kind_of(serde_json::json!({"type": "embeddings"})),
        ModelKind::Embedding
    );
    assert_eq!(
        kind_of(serde_json::json!({"task": "feature-extraction"})),
        ModelKind::Embedding
    );
    assert_eq!(
        kind_of(serde_json::json!({"pipeline_tag": "text-generation"})),
        ModelKind::Chat
    );
    assert_eq!(
        kind_of(serde_json::json!({"id": "text-embedding-3-small"})),
        ModelKind::Unknown
    );
}

#[test]
fn lmstudio_served_embedders_stay_available_for_embedding_roles() {
    let served = |id: &str, available: bool| ModelEntry {
        selection: ModelSelection {
            provider: "lm-studio".into(),
            model: id.into(),
            base_url: Some("http://127.0.0.1:1234/v1".into()),
            ..Default::default()
        },
        label: id.into(),
        source: "LM Studio".into(),
        available,
        metadata: ModelMetadata::default(),
    };
    let listed = |id: &str, kind: &str| LmStudioModel {
        id: id.into(),
        kind: kind.into(),
        tool_use: false,
        loaded: false,
        loaded_context: None,
    };
    let mut models = vec![
        served("nomic-embed", true),
        served("qwen-chat", true),
        served("cached-embed", false),
    ];
    annotate_lmstudio(
        &mut models,
        &[
            listed("nomic-embed", "embeddings"),
            listed("qwen-chat", "llm"),
            listed("cached-embed", "embeddings"),
        ],
    );
    assert!(models[0].available && models[0].is_embedding());
    assert!(!models[0].supports_chat());
    assert!(models[1].available && models[1].supports_chat());
    // Annotation never changes availability: an unverified row stays unavailable.
    assert!(!models[2].available && models[2].is_embedding());
}

#[test]
fn local_embedding_checkpoints_are_classified_and_unavailable_until_served() {
    let root = tempfile::tempdir().unwrap();
    let encoder = root.path().join("encoder");
    fs::create_dir_all(&encoder).unwrap();
    fs::write(
        encoder.join("config.json"),
        r#"{"architectures":["BertModel"],"model_type":"bert"}"#,
    )
    .unwrap();
    let chat = root.path().join("chat");
    fs::create_dir_all(&chat).unwrap();
    fs::write(
        chat.join("config.json"),
        r#"{"architectures":["LlamaForCausalLM"],"model_type":"llama"}"#,
    )
    .unwrap();
    assert_eq!(local_model_metadata(&encoder).kind, ModelKind::Embedding);
    assert_eq!(local_model_metadata(&chat).kind, ModelKind::Unknown);
    let checkpoint = |path: &Path| ModelEntry {
        selection: ModelSelection {
            provider: "mlx".into(),
            model: path.to_string_lossy().into_owned(),
            ..Default::default()
        },
        label: "checkpoint".into(),
        source: "Hugging Face cache (MLX)".into(),
        available: true,
        metadata: local_model_metadata(path),
    };
    let mut models = vec![checkpoint(&encoder), checkpoint(&chat)];
    annotate_embedding_checkpoints(&mut models);
    assert!(!models[0].available && models[0].is_embedding());
    assert!(models[0].source.contains("not served"));
    assert!(models[1].available && models[1].supports_chat());
}

#[test]
fn embedding_checkpoints_are_detected_from_metadata_not_names() {
    let root = tempfile::tempdir().unwrap();
    let sentence = root.path().join("plain-name");
    fs::create_dir_all(sentence.join("1_Pooling")).unwrap();
    fs::write(sentence.join("config.json"), r#"{"model_type":"qwen3"}"#).unwrap();
    assert!(is_embedding_checkpoint(&sentence));
    let encoder = root.path().join("encoder");
    fs::create_dir_all(&encoder).unwrap();
    fs::write(
        encoder.join("config.json"),
        r#"{"architectures":["XLMRobertaModel"],"model_type":"xlm-roberta"}"#,
    )
    .unwrap();
    assert!(is_embedding_checkpoint(&encoder));
    let chat = root.path().join("embed-named-chat");
    fs::create_dir_all(&chat).unwrap();
    fs::write(
        chat.join("config.json"),
        r#"{"architectures":["BertForCausalLM"],"model_type":"bert"}"#,
    )
    .unwrap();
    assert!(!is_embedding_checkpoint(&chat));
}

#[test]
fn local_picker_capabilities_read_templates_without_loading_weights() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("config.json"), r#"{"vision_config":{}}"#).unwrap();
    fs::write(
        root.path().join("tokenizer_config.json"),
        serde_json::json!({
            "chat_template":[{"name":"tool_use","template":"{% if tools %}<tool_call>{% endif %}"},
                {"name":"default","template":"{% if enable_thinking %}<think></think>{% endif %}"}]
        })
        .to_string(),
    )
    .unwrap();
    let info = local_model_metadata(root.path());
    assert_eq!(
        (info.vision, info.tools, info.reasoning),
        (Some(true), Some(true), Some(true))
    );
    fs::write(
        root.path().join("config.json"),
        r#"{"tools":false,"reasoning":false}"#,
    )
    .unwrap();
    let info = local_model_metadata(root.path());
    assert_eq!(
        (info.vision, info.tools, info.reasoning),
        (Some(false), Some(false), Some(false))
    );
    fs::remove_file(root.path().join("tokenizer_config.json")).unwrap();
    fs::write(root.path().join("config.json"), "{}").unwrap();
    fs::write(
        root.path().join("chat_template.jinja"),
        "{% if tools %}<tool_call>{% endif %}",
    )
    .unwrap();
    assert_eq!(local_model_metadata(root.path()).tools, Some(true));
}

#[test]
fn provider_metadata_preserves_supported_prices_and_vision() {
    let info = metadata(
        &serde_json::json!({"architecture":{"input_modalities":["text","image"]},"pricing":{"prompt":"0.000001","completion":"0.000003"}}),
    );
    assert_eq!(info.vision, Some(true));
    assert!((info.pricing.unwrap().cost(1000, 500).unwrap() - 0.0025).abs() < 1e-12);
    assert_eq!(
        metadata(&serde_json::json!({"input_modalities":["text"]})).vision,
        Some(false)
    );
    assert_eq!(metadata(&serde_json::json!({})).vision, None);
    for value in [
        serde_json::json!({"prompt":"NaN","completion":"1"}),
        serde_json::json!({"prompt":-1,"completion":1}),
        serde_json::json!({"prompt":"1"}),
    ] {
        assert!(
            metadata(&serde_json::json!({"pricing":value}))
                .pricing
                .is_none()
        );
    }
}
#[test]
fn claude_subscription_keeps_cached_models_selectable_when_model_listing_is_unavailable() {
    let mut probe = test_probe(super::super::anthropic::BASE_URL.into(), "anthropic");
    probe.token = Some("sk-ant-oat01-test".into());
    let mut provider_status = status(&probe, "unavailable", "Endpoint returned HTTP 404", 0);
    let models = anthropic_subscription_catalog(&probe, &mut provider_status, None).unwrap();
    assert_eq!(provider_status.state, "cached");
    assert!(
        provider_status
            .detail
            .contains("inference verifies model access")
    );
    assert!(
        models
            .iter()
            .any(|model| model.selection.model == "claude-sonnet-5-5" && model.available)
    );

    let mut cached = ModelEntry {
        selection: ModelSelection {
            provider: "anthropic".into(),
            model: "claude-sonnet-5-5".into(),
            base_url: Some(super::super::anthropic::BASE_URL.into()),
            ..Default::default()
        },
        label: "Sonnet".into(),
        source: "cached".into(),
        available: false,
        metadata: ModelMetadata::default(),
    };
    let mut provider_status = status(&probe, "unavailable", "endpoint offline", 0);
    let recovered = anthropic_subscription_catalog(
        &probe,
        &mut provider_status,
        Some(std::slice::from_mut(&mut cached)),
    )
    .unwrap();
    assert_eq!(provider_status.state, "cached");
    assert_eq!(
        recovered[0].source,
        "Claude subscription catalog (cached; inference verifies model access)"
    );

    probe.token = Some("sk-ant-api03-key".into());
    let mut provider_status = status(&probe, "unavailable", "endpoint offline", 0);
    assert!(
        anthropic_subscription_catalog(&probe, &mut provider_status, None).is_none(),
        "API-key discovery failures must not be treated as Claude subscription login"
    );
}
#[test]
fn claude_oauth_token_replaces_missing_api_key_block() {
    let mut probe = test_probe(super::super::anthropic::BASE_URL.into(), "anthropic");
    probe.blocked = Some((
        "auth-required",
        "Set the configured credential environment variable or save a URL-scoped token",
    ));
    apply_anthropic_oauth_access(
        &mut probe,
        Ok(super::super::auth::SecretToken::new(
            "sk-ant-oat01-test".into(),
        )),
    );
    assert_eq!(probe.token.as_deref(), Some("sk-ant-oat01-test"));
    assert!(probe.blocked.is_none());
}

#[test]
fn claude_discovery_refresh_failure_clears_api_key_and_blocks_probe() {
    let mut probe = test_probe(super::super::anthropic::BASE_URL.into(), "anthropic");
    probe.token = Some("sk-ant-api03-key-test".into());
    apply_anthropic_oauth_access(
        &mut probe,
        Err(anyhow::anyhow!("refresh rejected: secret-test-response")),
    );
    assert!(probe.token.is_none());
    let (status, models) = probe_models(&probe);
    assert_eq!(status.state, "auth-required");
    assert!(!status.detail.contains("secret-test-response"));
    assert!(models.is_empty());
}

fn claude_auth_fixture(dir: &Path) {
    assert!(std::env::var_os(crate::config::paths::SUBSCRIPTION_AUTH_HOME).is_none());
    crate::config::settings_store::write_private(
        &super::super::anthropic_oauth::credential_path(dir),
        &serde_json::to_vec(&serde_json::json!({"tokens": {
            "access_token": "sk-ant-oat01-subscription-test",
            "refresh_token": "refresh-test",
            "expires_at": u64::MAX
        }}))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn claude_discovery_subscription_wins_over_saved_key_and_signout_restores_key() {
    let home = tempfile::tempdir().unwrap();
    let base = super::super::anthropic::BASE_URL;
    save_credential(home.path(), base, "sk-ant-api03-key-test").unwrap();
    claude_auth_fixture(home.path());
    let spec = ProviderSpec {
        base_url: base.into(),
        api_key_env: Some("DIAL_TEST_UNUSED_ANTHROPIC_AUTH_KEY".into()),
        ..ProviderSpec::default()
    };
    let probe = prepare("anthropic".into(), spec.clone(), home.path());
    assert!(probe.blocked.is_none());
    assert_eq!(
        probe.token.as_deref(),
        Some("sk-ant-oat01-subscription-test")
    );
    super::super::anthropic_oauth::sign_out(home.path()).unwrap();
    let probe = prepare("anthropic".into(), spec, home.path());
    assert!(probe.blocked.is_none());
    assert_eq!(probe.token.as_deref(), Some("sk-ant-api03-key-test"));
}

#[test]
fn claude_discovery_subscription_ignores_unrelated_key_errors_and_rejects_wrong_protocol() {
    let home = tempfile::tempdir().unwrap();
    claude_auth_fixture(home.path());
    crate::config::settings_store::write_private(
        &home.path().join("credentials.json"),
        b"{invalid-api-key-storage",
    )
    .unwrap();
    let mut spec = ProviderSpec {
        base_url: super::super::anthropic::BASE_URL.into(),
        // This variable belongs to a different origin and must never be resolved here.
        api_key_env: Some("OPENAI_API_KEY".into()),
        ..ProviderSpec::default()
    };
    let probe = prepare("anthropic".into(), spec.clone(), home.path());
    assert!(probe.blocked.is_none());
    assert_eq!(
        probe.token.as_deref(),
        Some("sk-ant-oat01-subscription-test")
    );
    spec.protocol = Some(ApiProtocol::ChatCompletions);
    let probe = prepare("anthropic".into(), spec, home.path());
    assert!(probe.token.is_none());
    assert_eq!(probe.blocked.map(|(state, _)| state), Some("unavailable"));
}

#[test]
fn claude_discovery_corrupt_subscription_does_not_fall_back_to_saved_key() {
    assert!(std::env::var_os(crate::config::paths::SUBSCRIPTION_AUTH_HOME).is_none());
    let home = tempfile::tempdir().unwrap();
    let base = super::super::anthropic::BASE_URL;
    save_credential(home.path(), base, "sk-ant-api03-key-test").unwrap();
    crate::config::settings_store::write_private(
        &super::super::anthropic_oauth::credential_path(home.path()),
        b"{invalid-subscription-secret",
    )
    .unwrap();
    let spec = ProviderSpec {
        base_url: base.into(),
        ..ProviderSpec::default()
    };
    let probe = prepare("anthropic".into(), spec, home.path());
    assert!(
        probe.token.is_none(),
        "must not silently select API-key billing"
    );
    assert_eq!(probe.blocked.map(|(state, _)| state), Some("auth-required"));
    let (status, models) = probe_models(&probe);
    assert_eq!(status.state, "auth-required");
    assert!(!status.detail.contains("invalid-subscription-secret"));
    assert!(models.is_empty());
}

#[test]
fn claude_discovery_custom_endpoint_keeps_its_key_and_disabled_stays_disabled() {
    let home = tempfile::tempdir().unwrap();
    claude_auth_fixture(home.path());
    let spec = ProviderSpec {
        base_url: "https://proxy.example/v1".into(),
        ..ProviderSpec::default()
    };
    save_credential(home.path(), &spec.base_url, "proxy-key-test").unwrap();
    let probe = prepare("anthropic".into(), spec, home.path());
    assert_eq!(probe.token.as_deref(), Some("proxy-key-test"));
    assert!(probe.blocked.is_none());
    let spec = ProviderSpec {
        base_url: super::super::anthropic::BASE_URL.into(),
        enabled: false,
        ..ProviderSpec::default()
    };
    let probe = prepare("anthropic".into(), spec, home.path());
    assert!(probe.token.is_none());
    assert_eq!(probe.blocked.map(|(state, _)| state), Some("disabled"));
}

#[test]
fn explicit_claude_discovery_auth_is_independent_of_saved_credentials() {
    use super::super::model_config::AuthMethod;
    let home = tempfile::tempdir().unwrap();
    let base = super::super::anthropic::BASE_URL;
    save_credential(home.path(), base, "sk-ant-api03-key-test").unwrap();
    claude_auth_fixture(home.path());
    let mut spec = ProviderSpec {
        base_url: base.into(),
        auth: AuthMethod::ApiKey,
        ..Default::default()
    };
    let probe = prepare("anthropic".into(), spec.clone(), home.path());
    assert_eq!(probe.token.as_deref(), Some("sk-ant-api03-key-test"));
    assert!(probe.blocked.is_none());
    assert!(super::super::anthropic_oauth::signed_in_checked(home.path()).unwrap());
    spec.auth = AuthMethod::OAuth;
    let probe = prepare("anthropic".into(), spec.clone(), home.path());
    assert_eq!(
        probe.token.as_deref(),
        Some("sk-ant-oat01-subscription-test")
    );
    super::super::anthropic_oauth::sign_out(home.path()).unwrap();
    let probe = prepare("anthropic".into(), spec.clone(), home.path());
    assert!(probe.token.is_none());
    assert_eq!(probe.blocked.map(|(s, _)| s), Some("auth-required"));
    crate::config::settings_store::write_private(
        &super::super::anthropic_oauth::credential_path(home.path()),
        b"corrupt-subscription",
    )
    .unwrap();
    spec.auth = AuthMethod::ApiKey;
    let probe = prepare("anthropic".into(), spec, home.path());
    assert_eq!(probe.token.as_deref(), Some("sk-ant-api03-key-test"));
    assert!(probe.blocked.is_none());
}
fn server(code: u16, body: String) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let job = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            request.push_str(&line);
        }
        write!(stream, "HTTP/1.1 {code} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        request
    });
    (format!("http://{address}/v1"), job)
}
fn test_probe(url: String, id: &str) -> Probe {
    Probe {
        id: id.into(),
        spec: ProviderSpec {
            label: id.into(),
            base_url: url,
            ..ProviderSpec::default()
        },
        token: None,
        blocked: None,
    }
}
#[test]
fn probes_real_http_models_and_keeps_unknown_context_unknown() {
    let (url, job) = server(
        200,
        r#"{"data":[{"id":"one","context_length":12345},{"id":"two"},{"id":"one"}]}"#.into(),
    );
    let (status, models) = probe_models(&test_probe(url, "custom"));
    assert_eq!(status.state, "reachable");
    assert_eq!(
        models
            .iter()
            .map(|m| (&*m.selection.model, m.selection.context_window))
            .collect::<Vec<_>>(),
        [("one", Some(12345)), ("two", None)]
    );
    assert!(job.join().unwrap().starts_with("GET /v1/models "));
    let (url, job) = server(200, r#"{"models":[{"name":"llama:latest"}]}"#.into());
    let (_, models) = probe_models(&test_probe(url, "ollama"));
    assert_eq!(models[0].selection.model, "llama:latest");
    assert!(job.join().unwrap().starts_with("GET /api/tags "));
}
#[test]
fn ollama_models_take_their_purpose_from_api_show_capabilities() {
    let url = routed(vec![
        (
            "/api/tags",
            r#"{"models":[{"name":"embeddinggemma:latest"}]}"#.into(),
        ),
        ("/api/show", r#"{"capabilities":["embedding"]}"#.into()),
    ]);
    let (status, models) = probe_models(&test_probe(url, "ollama"));
    assert_eq!(status.state, "reachable");
    assert!(models[0].is_embedding(), "{:?}", models[0].metadata.kind);
    assert!(models[0].available);
    let url = routed(vec![
        ("/api/tags", r#"{"models":[{"name":"qwen3:4b"}]}"#.into()),
        (
            "/api/show",
            r#"{"capabilities":["completion","tools"]}"#.into(),
        ),
    ]);
    let (_, models) = probe_models(&test_probe(url, "ollama"));
    assert_eq!(models[0].metadata.kind, ModelKind::Chat);
    // The `/api/show` list is Ollama's own declaration: listed is supported, absent is not.
    assert_eq!(
        (
            models[0].metadata.tools,
            models[0].metadata.vision,
            models[0].metadata.reasoning
        ),
        (Some(true), Some(false), Some(false))
    );
    let url = routed(vec![
        ("/api/tags", r#"{"models":[{"name":"gemma3:4b"}]}"#.into()),
        (
            "/api/show",
            r#"{"capabilities":["completion","vision","thinking"]}"#.into(),
        ),
    ]);
    let (_, models) = probe_models(&test_probe(url, "ollama"));
    assert_eq!(
        (
            models[0].metadata.tools,
            models[0].metadata.vision,
            models[0].metadata.reasoning
        ),
        (Some(false), Some(true), Some(true))
    );
    // No /api/show (404): the model is still listed, with an unknown purpose.
    let url = routed(vec![("/api/tags", r#"{"models":[{"name":"m"}]}"#.into())]);
    let (_, models) = probe_models(&test_probe(url, "ollama"));
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].metadata.kind, ModelKind::Unknown);
    assert_eq!(
        (
            models[0].metadata.tools,
            models[0].metadata.vision,
            models[0].metadata.reasoning
        ),
        (None, None, None)
    );
}
#[test]
fn auth_redirect_invalid_and_oversized_responses_are_explicit_failures() {
    for (code, body, expected) in [
        (401, "secret must not appear".into(), "auth-required"),
        (302, "redirect".into(), "unavailable"),
        (200, "invalid".into(), "unavailable"),
        (200, "x".repeat(MAX_RESPONSE as usize + 1), "unavailable"),
    ] {
        let (url, job) = server(code, body);
        let (status, models) = probe_models(&test_probe(url, "custom"));
        assert_eq!(status.state, expected);
        assert!(models.is_empty());
        assert!(!status.detail.contains("secret"));
        job.join().unwrap();
    }
}
#[test]
fn first_party_key_verification_uses_provider_auth_and_saves_only_after_success() {
    let home = tempfile::tempdir().unwrap();
    let (url, job) = server(200, r#"{"data":[{"id":"claude-test"}]}"#.into());
    let mut key = "test-secret".to_owned();
    verify_and_save_credential(home.path(), "anthropic", &url, &mut key).unwrap();
    assert!(key.is_empty());
    let request = job.join().unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /v1/models "));
    assert!(
        request.contains("x-api-key: test-secret")
            && request.contains("anthropic-version: 2023-06-01")
    );
    assert!(!request.contains("authorization:"));
    assert_eq!(
        credential_token(home.path(), &url).unwrap().as_deref(),
        Some("test-secret")
    );

    let rejected_home = tempfile::tempdir().unwrap();
    let (url, job) = server(401, r#"{"error":"invalid key"}"#.into());
    let mut key = "bad-secret".to_owned();
    let error = verify_and_save_credential(rejected_home.path(), "openai", &url, &mut key)
        .unwrap_err()
        .to_string();
    let request = job.join().unwrap().to_ascii_lowercase();
    assert!(request.contains("authorization: bearer bad-secret"));
    assert!(error.contains("rejected") && !error.contains("bad-secret"));
    assert!(key.is_empty());
    assert_eq!(
        credential_token(rejected_home.path(), &url).unwrap(),
        None,
        "rejected keys are never persisted"
    );
}
#[test]
fn openrouter_key_validation_precedes_persistence() {
    let home = tempfile::tempdir().unwrap();
    let (url, job) = server(200, r#"{"data":{"label":"Dial"}}"#.into());
    let validate = url.replace("/v1", "/api/v1/auth/key");
    let mut key = "sk-or-valid".to_owned();
    verify_and_save_credential_at(
        home.path(),
        "openrouter",
        OPENROUTER_BASE_URL,
        &mut key,
        &validate,
    )
    .unwrap();
    assert!(key.is_empty());
    let request = job.join().unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /api/v1/auth/key "));
    assert!(request.contains("authorization: bearer sk-or-valid"));
    assert_eq!(
        credential_token(home.path(), OPENROUTER_BASE_URL)
            .unwrap()
            .as_deref(),
        Some("sk-or-valid")
    );

    let rejected_home = tempfile::tempdir().unwrap();
    let (url, job) = server(401, r#"{"error":"no"}"#.into());
    let mut key = "sk-or-invalid".to_owned();
    let validate = url.replace("/v1", "/api/v1/auth/key");
    let error = verify_and_save_credential_at(
        rejected_home.path(),
        "openrouter",
        OPENROUTER_BASE_URL,
        &mut key,
        &validate,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("rejected") && !error.contains("sk-or-invalid"));
    assert!(key.is_empty());
    job.join().unwrap();
    assert_eq!(
        credential_token(rejected_home.path(), OPENROUTER_BASE_URL).unwrap(),
        None
    );
}
#[test]
fn key_verification_rejects_unreachable_metadata_without_persisting() {
    let home = tempfile::tempdir().unwrap();
    let (url, job) = server(500, r#"{"error":"upstream"}"#.into());
    let mut key = "test-secret".to_owned();
    let error = verify_and_save_credential(home.path(), "anthropic", &url, &mut key)
        .unwrap_err()
        .to_string();
    assert!(error.contains("HTTP 500") && !error.contains("test-secret"));
    assert!(
        key.is_empty(),
        "the caller's key buffer is cleared whether verification succeeds or fails"
    );
    job.join().unwrap();
    assert_eq!(credential_token(home.path(), &url).unwrap(), None);
}
#[test]
fn credentials_are_url_scoped_private_and_preserve_other_entries() {
    let dir = tempfile::tempdir().unwrap();
    save_credential(dir.path(), "https://first.test/v1", "first-secret").unwrap();
    save_credential(dir.path(), "https://second.test/v1", "second-secret").unwrap();
    remove_credential(dir.path(), "https://first.test/v1").unwrap();
    assert_eq!(
        credential_token(dir.path(), "https://first.test/v1").unwrap(),
        None
    );
    assert_eq!(
        credential_token(dir.path(), "https://second.test/v1")
            .unwrap()
            .as_deref(),
        Some("second-secret")
    );
    assert_eq!(
        credential_token(dir.path(), "https://second.test/other").unwrap(),
        None
    );
    for url in [
        "https://user:secret@first.test/v1",
        "https://first.test/v1?key=secret",
        "https://first.test/v1#secret",
    ] {
        assert!(save_credential(dir.path(), url, "ok").is_err());
    }
    assert_eq!(
        crate::platform::fs::mode(&fs::metadata(dir.path().join("credentials.json")).unwrap())
            & 0o777,
        0o600
    );
    let uri = "https://unrelated.test/v1".parse().unwrap();
    assert!(env_token("OPENAI_API_KEY", &uri).is_err());
}
#[test]
fn hf_main_revision_wins_and_incomplete_snapshots_are_ignored() {
    let cache = tempfile::tempdir().unwrap();
    let repo = cache.path().join("models--mlx-community--example");
    fs::create_dir_all(repo.join("refs")).unwrap();
    fs::write(repo.join("refs/main"), "current\n").unwrap();
    for name in ["old", "current"] {
        let path = repo.join("snapshots").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("config.json"),
            r#"{"max_position_embeddings":8192}"#,
        )
        .unwrap();
        fs::write(path.join("model.safetensors"), "weights").unwrap();
    }
    let entries = local_models(cache.path(), None);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].selection.model.ends_with("/current"));
    assert_eq!(entries[0].selection.context_window, Some(8192));
    fs::write(
        repo.join("snapshots/current/model.safetensors.index.json"),
        r#"{"weight_map":{"w":"missing.safetensors"}}"#,
    )
    .unwrap();
    assert!(
        local_models(cache.path(), None)[0]
            .selection
            .model
            .ends_with("/old")
    );
}
#[test]
fn local_resolution_uses_complete_snapshot_without_main_ref() {
    let cache = tempfile::tempdir().unwrap();
    assert!(
        resolve_local_model_in("mlx-community/missing", Some(cache.path()), None)
            .unwrap_err()
            .to_string()
            .contains("no complete downloaded weights")
    );
    let path = cache
        .path()
        .join("models--mlx-community--example/snapshots/cached");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("config.json"), "{}").unwrap();
    fs::write(path.join("model.safetensors"), "weights").unwrap();
    let resolved =
        resolve_local_model_in("mlx-community/example", Some(cache.path()), None).unwrap();
    assert_eq!(Path::new(&resolved), fs::canonicalize(&path).unwrap());
    assert_eq!(
        resolve_local_model_in(&resolved, None, None).unwrap(),
        resolved
    );
    fs::write(
        path.join("model.safetensors.index.json"),
        r#"{"weight_map":{"w":"missing.safetensors"}}"#,
    )
    .unwrap();
    let error =
        resolve_local_model_in("mlx-community/example", Some(cache.path()), None).unwrap_err();
    assert!(error.to_string().contains("no complete downloaded weights"));
    assert!(error.to_string().contains("Architecture model"));
}

#[test]
fn lm_studio_mlx_downloads_are_listed_and_other_formats_skipped() {
    let folder = tempfile::tempdir().unwrap();
    let mlx = folder.path().join("lmstudio-community/Qwen-MLX-8bit");
    fs::create_dir_all(&mlx).unwrap();
    fs::write(
        mlx.join("config.json"),
        r#"{"model_type":"qwen3_5","vision_config":{},"text_config":{"max_position_embeddings":32768}}"#,
    )
    .unwrap();
    let gemma = folder.path().join("lmstudio-community/gemma-MLX");
    fs::create_dir_all(&gemma).unwrap();
    fs::write(
        gemma.join("config.json"),
        r#"{"text_config":{"model_type":"gemma4_unified"}}"#,
    )
    .unwrap();
    fs::write(gemma.join("model.safetensors"), "weights").unwrap();
    fs::write(mlx.join("model.safetensors"), "weights").unwrap();
    let gguf = folder.path().join("lmstudio-community/Qwen-GGUF");
    fs::create_dir_all(&gguf).unwrap();
    fs::write(gguf.join("model-Q4_K_M.gguf"), "weights").unwrap();
    fs::create_dir_all(folder.path().join("incoai/Bundle/target")).unwrap();
    let architectures = folder.path().join("mlx_lm/models");
    fs::create_dir_all(&architectures).unwrap();
    fs::write(architectures.join("qwen3_5.py"), "").unwrap();
    let entries = lmstudio_mlx_models(folder.path(), Some(&architectures));
    assert_eq!(entries[0].metadata.vision, Some(true));
    assert_eq!(entries[1].metadata.vision, Some(false));
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.label.as_str(), e.metadata.lora))
            .collect::<Vec<_>>(),
        [
            ("lmstudio-community/Qwen-MLX-8bit", Some(true)),
            ("lmstudio-community/gemma-MLX", None)
        ]
    );
    assert_eq!(
        lmstudio_mlx_models(folder.path(), None)[0].metadata.lora,
        None,
        "unknown without an mlx-lm install"
    );
    assert_eq!(entries[0].selection.model, mlx.to_string_lossy());
    assert_eq!(entries[0].selection.context_window, Some(32768));
    assert!(
        entries[0].selection.base_url.is_none(),
        "served by dial's MLX server, not LM Studio"
    );
}
#[test]
fn gguf_downloads_keep_the_local_route_but_are_named_by_their_format() {
    let studio = tempfile::tempdir().unwrap();
    let mlx = studio.path().join("lmstudio-community/Qwen-MLX");
    fs::create_dir_all(&mlx).unwrap();
    fs::write(mlx.join("config.json"), r#"{"model_type":"qwen3"}"#).unwrap();
    fs::write(mlx.join("model.safetensors"), "weights").unwrap();
    let studio_gguf = studio.path().join("lmstudio-community/Qwen-GGUF");
    fs::create_dir_all(&studio_gguf).unwrap();
    fs::write(studio_gguf.join("model-Q4_K_M.gguf"), b"GGUF weights").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let repo = cache.path().join("models--org--Qwen-GGUF");
    let snapshot = repo.join("snapshots/abc123");
    fs::create_dir_all(&snapshot).unwrap();
    fs::create_dir_all(repo.join("refs")).unwrap();
    fs::write(repo.join("refs/main"), "abc123").unwrap();
    fs::write(snapshot.join("model-Q4_K_M.gguf"), b"GGUF weights").unwrap();

    let entries = [
        lmstudio_mlx_models(studio.path(), None).remove(0),
        lmstudio_gguf_models(studio.path()).remove(0),
        local_models(cache.path(), None).remove(0),
    ];
    let shown: Vec<_> = entries
        .iter()
        .map(|e| {
            (
                e.selection.provider.as_str(),
                e.display_provider(),
                e.metadata.format,
            )
        })
        .collect();
    let local = |shown, format| ("mlx", shown, Some(format));
    assert_eq!(
        shown,
        [
            local("mlx", LocalFormat::Mlx),
            local("gguf", LocalFormat::Gguf),
            local("gguf", LocalFormat::Gguf),
        ],
        "the route id stays `mlx` (configs and resume select it); only the shown name changes"
    );
    assert!(entries[1].source.starts_with("LM Studio download (GGUF)"));
    assert!(entries[2].source.starts_with("Hugging Face cache (GGUF)"));
}
#[test]
fn disabled_endpoint_is_never_contacted() {
    let dir = tempfile::tempdir().unwrap();
    let probe = prepare(
        "custom".into(),
        ProviderSpec {
            base_url: "http://127.0.0.1:1".into(),
            enabled: false,
            ..ProviderSpec::default()
        },
        dir.path(),
    );
    let (status, models) = probe_models(&probe);
    assert_eq!(status.state, "disabled");
    assert!(models.is_empty());
}
#[test]
fn failed_live_probe_retains_cached_and_configured_models_as_unverified() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let (url, job) = server(401, "{}".into());
    let selection = ModelSelection {
        provider: "custom".into(),
        model: "saved".into(),
        base_url: Some(url.clone()),
        ..ModelSelection::default()
    };
    persist_cache(
        &layout.user_dir,
        BTreeMap::from([(
            url.clone(),
            vec![ModelEntry {
                selection: selection.clone(),
                label: "saved".into(),
                source: "prior live probe".into(),
                available: true,
                metadata: ModelMetadata::default(),
            }],
        )]),
    )
    .unwrap();
    let mut configured = BTreeMap::new();
    for id in ["lm-studio", "ollama", "llama-cpp"]
        .into_iter()
        .chain(KNOWN.iter().map(|row| row.0))
    {
        configured.insert(
            id.into(),
            ProviderSpec {
                enabled: false,
                ..ProviderSpec::default()
            },
        );
    }
    configured.insert(
        "custom".into(),
        ProviderSpec {
            base_url: url,
            ..ProviderSpec::default()
        },
    );
    let mut another = selection.clone();
    another.model = "not-in-cache".into();
    let result = discover(&layout, &configured, &[selection, another]);
    assert_eq!(
        result
            .providers
            .iter()
            .find(|s| s.id == "custom")
            .unwrap()
            .state,
        "auth-required"
    );
    let models: Vec<_> = result
        .models
        .iter()
        .filter(|m| m.selection.provider == "custom")
        .collect();
    assert_eq!(models.len(), 2);
    assert!(models.iter().all(|m| !m.available));
    assert!(
        models
            .iter()
            .any(|m| m.selection.model == "saved" && m.source.contains("cached"))
    );
    assert!(models.iter().any(|m| m.selection.model == "not-in-cache"));
    job.join().unwrap();
}
#[test]
fn simultaneous_credential_saves_do_not_lose_other_urls() {
    let home = tempfile::tempdir().unwrap();
    std::thread::scope(|scope| {
        for n in 0..8 {
            let path = home.path();
            scope.spawn(move || {
                save_credential(path, &format!("https://provider{n}.test/v1"), "private-key")
                    .unwrap()
            });
        }
    });
    for n in 0..8 {
        assert_eq!(
            credential_token(home.path(), &format!("https://provider{n}.test/v1"))
                .unwrap()
                .as_deref(),
            Some("private-key")
        );
    }
}
#[test]
fn metadata_caps_models_and_rejects_invalid_context_values() {
    let probe = test_probe("http://127.0.0.1:1234/v1".into(), "custom");
    let rows: Vec<_> = (0..MAX_MODELS + 20).map(|i| serde_json::json!({"id":format!("model-{i}"), "context_length":if i == 0 { u64::MAX } else { 0 }})).collect();
    let models = parse_models(&serde_json::json!({"data":rows}), &probe).unwrap();
    assert_eq!(models.len(), MAX_MODELS);
    assert!(
        models
            .iter()
            .all(|entry| entry.selection.context_window.is_none())
    );
    assert!(
        models
            .iter()
            .all(|entry| entry.selection.model != format!("model-{MAX_MODELS}"))
    );
}

#[test]
fn anthropic_catalog_reads_input_context_without_confusing_output_limits() {
    let probe = test_probe(super::super::anthropic::BASE_URL.into(), "anthropic");
    let entries = parse_models(
        &serde_json::json!({"data":[
            {"id":"claude-opus-5-5","max_input_tokens":1_000_000,"max_tokens":128_000},
            {"id":"claude-custom","max_input_tokens":750_000,"max_tokens":32_000},
            {"id":"claude-unknown","max_input_tokens":null,"max_tokens":128_000}
        ]}),
        &probe,
    )
    .unwrap();
    assert_eq!(entries[0].selection.context_window, Some(1_000_000));
    assert_eq!(entries[1].selection.context_window, Some(750_000));
    assert_eq!(entries[2].selection.context_window, None);
    assert_eq!(entries[0].metadata.max_output_tokens, Some(128_000));
    assert_eq!(entries[1].metadata.max_output_tokens, Some(32_000));
}

#[test]
fn hosted_catalog_capabilities_match_what_requests_assume() {
    // Anthropic: `capabilities` is an object of `{ "supported": bool }` records; live values win.
    let probe = test_probe(super::super::anthropic::BASE_URL.into(), "anthropic");
    let entries = parse_models(
        &serde_json::json!({"data":[
            {"id":"claude-custom","capabilities":{
                "image_input":{"supported":false},
                "thinking":{"supported":true,"types":{}}
            }},
            {"id":"claude-opus-5-5"},
            {"id":"claude-unlisted"}
        ]}),
        &probe,
    )
    .unwrap();
    assert_eq!(
        (entries[0].metadata.vision, entries[0].metadata.reasoning),
        (Some(false), Some(true))
    );
    // Without live fields the picker shows the bundled record the runtime also resolves.
    let selection = &entries[1].selection;
    let runtime = bundled::complete_metadata(selection, ModelMetadata::default());
    assert_eq!(entries[1].metadata.vision, Some(true));
    assert_eq!(entries[1].metadata.reasoning, Some(true));
    assert_eq!(
        (entries[1].metadata.vision, entries[1].metadata.reasoning),
        (runtime.vision, runtime.reasoning)
    );
    // Discovery does not attach a list price as if the provider had reported one.
    assert!(entries[1].metadata.pricing.is_none());
    // A model no source describes stays unknown rather than guessed from its name.
    assert_eq!(
        (
            entries[2].metadata.vision,
            entries[2].metadata.tools,
            entries[2].metadata.reasoning
        ),
        (None, None, None)
    );
    // Gemini reports `thinking` as a top-level boolean.
    assert_eq!(
        metadata(&serde_json::json!({"name":"models/x","thinking":false})).reasoning,
        Some(false)
    );
    assert_eq!(
        metadata(&serde_json::json!({"name":"models/x","thinking":true})).reasoning,
        Some(true)
    );
}

#[test]
fn old_claude_cache_recovers_context_without_network_or_rewriting_saved_metadata() {
    for model in ["claude-opus-5-5", "claude-sonnet-4-6"] {
        let home = tempfile::tempdir().unwrap();
        let selection = ModelSelection {
            provider: "anthropic".into(),
            model: model.into(),
            base_url: Some(super::super::anthropic::BASE_URL.into()),
            ..Default::default()
        };
        let mut row = ModelEntry {
            selection: selection.clone(),
            label: model.into(),
            source: "cached".into(),
            available: false,
            metadata: Default::default(),
        };
        let path = home.path().join("model-catalog.json");
        let bytes = serde_json::to_vec(&BTreeMap::from([(
            super::super::anthropic::BASE_URL,
            vec![row.clone()],
        )]))
        .unwrap();
        write_private(&path, &bytes).unwrap();
        assert_eq!(
            model_context_window(home.path(), &selection),
            Some(1_000_000),
            "{model}: recover capacity"
        );
        assert_eq!(
            cached(home.path())[&selection_cache_key(&selection)][0]
                .selection
                .context_window,
            Some(1_000_000),
            "{model}: repair legacy cache in memory"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "{model}: preserve saved cache"
        );
        row.selection.context_window = Some(400_000);
        write_private(
            &path,
            &serde_json::to_vec(&BTreeMap::from([(
                super::super::anthropic::BASE_URL,
                vec![row],
            )]))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            model_context_window(home.path(), &selection),
            Some(400_000),
            "{model}: live capacity wins"
        );
        let mut selected = selection.clone();
        selected.context_window = Some(64_000);
        assert_eq!(
            model_context_window(home.path(), &selected),
            Some(64_000),
            "{model}: selected capacity wins"
        );
        selected.context_window = None;
        selected.base_url = Some("https://proxy.example/v1".into());
        assert_eq!(
            model_context_window(home.path(), &selected),
            None,
            "{model}: custom route"
        );
        selected.base_url = selection.base_url;
        selected.model = "claude-future-99".into();
        assert_eq!(
            model_context_window(home.path(), &selected),
            None,
            "{model}: unknown model"
        );
    }
}

/// A catalog saved before a Gemini image model was documented lists it as an image model on the
/// next load, without a rescan; an explicit saved value and custom endpoints are left alone.
#[test]
fn old_gemini_cache_recovers_documented_image_output_without_rewriting_it() {
    use crate::llm::profiles::ImageGenerationApi;
    let home = tempfile::tempdir().unwrap();
    let base_url = bundled::builtin_provider("gemini")
        .unwrap()
        .base_url
        .clone();
    let row = |model: &str, base_url: &str, image_generation| ModelEntry {
        selection: ModelSelection {
            provider: "gemini".into(),
            model: model.into(),
            base_url: Some(base_url.into()),
            ..Default::default()
        },
        label: model.into(),
        source: "gemini".into(),
        available: true,
        metadata: ModelMetadata {
            image_generation,
            ..Default::default()
        },
    };
    let rows = vec![
        row("gemini-3.1-flash-image", &base_url, None),
        row(
            "gemini-3-pro-image",
            &base_url,
            Some(ImageGenerationApi::Disabled),
        ),
        row("gemini-2.5-flash", &base_url, None),
        row("gemini-3.1-flash-image", "https://proxy.example/v1", None),
    ];
    let path = home.path().join("model-catalog.json");
    let bytes = serde_json::to_vec(&BTreeMap::from([("saved", rows.clone())])).unwrap();
    write_private(&path, &bytes).unwrap();
    let loaded: Vec<_> = cached(home.path()).into_values().flatten().collect();
    let image = |model: &str, url: &str| {
        loaded
            .iter()
            .find(|e| e.selection.model == model && e.selection.base_url.as_deref() == Some(url))
            .unwrap()
            .metadata
            .image_generation
    };
    assert_eq!(
        image("gemini-3.1-flash-image", &base_url),
        Some(ImageGenerationApi::Gemini)
    );
    assert_eq!(
        image("gemini-3-pro-image", &base_url),
        Some(ImageGenerationApi::Disabled)
    );
    assert_eq!(image("gemini-2.5-flash", &base_url), None);
    assert_eq!(
        image("gemini-3.1-flash-image", "https://proxy.example/v1"),
        None
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
}
#[test]
fn malformed_credentials_fail_closed_without_echoing_content() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("credentials.json"),
        "secret-token malformed",
    )
    .unwrap();
    let error = save_credential(home.path(), "https://provider.test/v1", "new-token").unwrap_err();
    assert!(!format!("{error:#}").contains("secret-token"));
    assert_eq!(
        fs::read_to_string(home.path().join("credentials.json")).unwrap(),
        "secret-token malformed"
    );
}
/// Answers any number of requests by path; unknown paths get 404 like a server without the native API.
pub(crate) fn routed(routes: Vec<(&'static str, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).ok();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            // Drain a POST body so closing the socket cannot reset the connection before the reply is read.
            std::io::Read::read_exact(&mut reader, &mut vec![0u8; length]).ok();
            let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
            let (code, body) = routes
                .iter()
                .find(|(route, _)| *route == path)
                .map(|(_, body)| (200, body.clone()))
                .unwrap_or((404, "{}".into()));
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{address}/v1")
}
fn lm_studio_routes() -> Vec<(&'static str, String)> {
    let native = serde_json::json!({"data":[
        {"id":"warm","type":"llm","state":"loaded","max_context_length":1_048_576,"loaded_context_length":8192},
        {"id":"cold","type":"vlm","state":"not-loaded","max_context_length":262_144,"capabilities":["tool_use"]},
        {"id":"embedder","type":"embeddings","state":"not-loaded","max_context_length":2048},
    ]});
    vec![
        ("/v1/models", r#"{"data":[{"id":"warm"},{"id":"cold"},{"id":"embedder"}]}"#.into()),
        ("/api/v0/models", native.to_string()),
        ("/api/v0/models/warm", serde_json::json!({"id":"warm","type":"llm","state":"loaded","max_context_length":1_048_576,"loaded_context_length":8192}).to_string()),
        ("/api/v0/models/cold", serde_json::json!({"id":"cold","type":"vlm","state":"not-loaded","max_context_length":262_144}).to_string()),
    ]
}
#[test]
fn lm_studio_discovery_keeps_embedders_available_and_reports_only_loaded_context() {
    let url = routed(lm_studio_routes());
    let (status, models) = probe_models(&test_probe(url, "lm-studio"));
    assert_eq!(status.state, "reachable");
    let by = |id: &str| models.iter().find(|m| m.selection.model == id).unwrap();
    assert_eq!(
        by("warm").selection.context_window,
        Some(8192),
        "loaded size is the budget"
    );
    assert!(by("warm").source.contains("loaded") && by("warm").available);
    assert_eq!(
        by("cold").selection.context_window,
        None,
        "an unloaded model's maximum must never become its window"
    );
    assert!(by("cold").source.contains("loads on first use") && by("cold").available);
    assert!(
        by("embedder").available
            && by("embedder").is_embedding()
            && !by("embedder").supports_chat(),
        "served embedders are available for embedding roles, not chat"
    );
    assert!(
        models
            .iter()
            .all(|m| m.selection.context_window != Some(1_048_576)
                && m.selection.context_window != Some(262_144))
    );
}
#[test]
fn lm_studio_native_metadata_declares_image_input_and_tool_use() {
    let url = routed(lm_studio_routes());
    let (_, models) = probe_models(&test_probe(url, "lm-studio"));
    let meta = |id: &str| {
        let m = &models
            .iter()
            .find(|m| m.selection.model == id)
            .unwrap()
            .metadata;
        (m.vision, m.tools)
    };
    assert_eq!(
        meta("cold"),
        (Some(true), Some(true)),
        "vlm declaring tool_use"
    );
    assert_eq!(
        meta("warm"),
        (Some(false), None),
        "text-only llm; no tool_use is not a refusal"
    );
    assert_eq!(meta("embedder"), (None, None), "embedders make no claim");
}
#[test]
fn llama_server_is_recognised_by_props_on_local_hosts_only() {
    let llama = routed(vec![(
        "/props",
        r#"{"default_generation_settings":{"n_ctx":4096}}"#.into(),
    )]);
    assert!(is_llama_server(&llama, None, &[]));
    let other = routed(vec![]);
    assert!(
        !is_llama_server(&other, None, &[]),
        "/props 404 is not llama.cpp"
    );
    assert!(!is_llama_server(
        "https://api.example.invalid/v1",
        Some("secret"),
        &[]
    ));
}
#[test]
fn llama_props_report_the_templates_native_tool_support() {
    let parse = |props: serde_json::Value| LlamaProps::parse(&props);
    assert_eq!(parse(serde_json::json!({"total_slots": 1})), None);
    let caps = |tools: bool, calls: bool| {
        serde_json::json!({
            "default_generation_settings": {"n_ctx": 4096},
            "chat_template_caps": {"supports_tools": tools, "supports_tool_calls": calls},
        })
    };
    assert_eq!(
        parse(caps(true, true)).unwrap().native_tool_template,
        Some(true)
    );
    assert_eq!(
        parse(caps(false, true)).unwrap().native_tool_template,
        Some(false)
    );
    assert_eq!(
        parse(caps(true, false)).unwrap().native_tool_template,
        Some(false)
    );
    // Older builds report no caps: unknown, not unsupported.
    let old = parse(serde_json::json!({"default_generation_settings": {}})).unwrap();
    assert_eq!(old.native_tool_template, None);

    let llama = routed(vec![("/props", caps(false, false).to_string())]);
    assert!(known_llama_props(&llama).is_none(), "nothing probed yet");
    assert_eq!(
        llama_server_props(&llama, None, &[])
            .unwrap()
            .native_tool_template,
        Some(false)
    );
    assert_eq!(
        known_llama_props(&format!("{llama}/"))
            .unwrap()
            .native_tool_template,
        Some(false)
    );
}
#[test]
fn servers_without_the_native_api_are_listed_unchanged() {
    let url = routed(vec![(
        "/v1/models",
        r#"{"data":[{"id":"a"},{"id":"b"}]}"#.into(),
    )]);
    let (status, models) = probe_models(&test_probe(url, "llama-cpp"));
    assert_eq!(status.state, "reachable");
    assert_eq!(models.len(), 2);
    assert!(
        models.iter().all(|m| m.available
            && m.selection.context_window.is_none()
            && m.source == "llama-cpp")
    );
}
#[test]
fn loaded_context_is_exact_local_only_and_rejects_hostile_model_names() {
    let url = routed(lm_studio_routes());
    assert_eq!(loaded_context(&url, "warm", None, &[]), Some(8192));
    assert_eq!(loaded_context(&url, "cold", None, &[]), None, "not loaded");
    assert_eq!(loaded_context(&url, "missing", None, &[]), None);
    for hostile in ["warm?x=1", "warm#frag", "warm%2e", "", "a\nb"] {
        assert_eq!(
            loaded_context(&url, hostile, None, &[]),
            None,
            "{hostile:?}"
        );
    }
    // A hosted API is never sent LM Studio probes, and the answer cannot come from the network.
    assert_eq!(
        loaded_context("https://api.example.com/v1", "warm", None, &[]),
        None
    );
}
#[test]
fn llama_cpp_and_ollama_report_the_context_they_actually_serve() {
    // llama.cpp advertises its training context in /v1/models; the served per-slot size is in /props.
    let llama = routed(vec![
        (
            "/v1/models",
            r#"{"data":[{"id":"qwen","meta":{"n_ctx_train":131072}}]}"#.into(),
        ),
        (
            "/props",
            r#"{"default_generation_settings":{"n_ctx":4096}}"#.into(),
        ),
    ]);
    assert_eq!(loaded_context(&llama, "qwen", None, &[]), Some(4096));
    let ollama = routed(vec![("/api/ps", r#"{"models":[{"name":"qwen2.5:7b","model":"qwen2.5:7b","context_length":4096},{"name":"llama3:latest","model":"llama3:latest","context_length":8192}]}"#.into())]);
    assert_eq!(loaded_context(&ollama, "qwen2.5:7b", None, &[]), Some(4096));
    assert_eq!(
        loaded_context(&ollama, "llama3", None, &[]),
        Some(8192),
        ":latest is Ollama's implicit tag"
    );
    assert_eq!(
        loaded_context(&ollama, "mistral", None, &[]),
        None,
        "not loaded yet"
    );
    let cold = routed(vec![
        ("/api/ps", r#"{"models":[]}"#.into()),
        (
            "/api/show",
            r#"{"model_info":{"general.architecture":"qwen3","qwen3.context_length":40960}}"#
                .into(),
        ),
    ]);
    assert_eq!(
        loaded_context(&cold, "qwen3:8b", None, &[]),
        Some(40960),
        "an unloaded Ollama model is bounded by its trained context"
    );
    assert_eq!(
        loaded_context(&routed(vec![]), "anything", None, &[]),
        None,
        "a server with none of the native APIs"
    );
}

#[test]
fn image_generation_metadata_is_independent_of_image_input() {
    use crate::llm::profiles::ImageGenerationApi;
    assert_eq!(
        image_generation_api(&serde_json::json!({"input_modalities":["text","image"]})),
        None
    );
    assert_eq!(
        metadata(&serde_json::json!({"architecture":{"output_modalities":["text","image"]}}))
            .image_generation,
        Some(ImageGenerationApi::ChatCompletions)
    );
    assert_eq!(
        image_generation_api(&serde_json::json!({"output_modalities":["text"]})),
        Some(ImageGenerationApi::Disabled)
    );
    assert_eq!(
        image_generation_api(
            &serde_json::json!({"experimental_supported_tools":["image_generation"],"output_modalities":["text"]})
        ),
        Some(ImageGenerationApi::Responses)
    );
    // OpenRouter's auto router lists its pool's union of output modalities but has no image endpoint.
    assert_eq!(
        image_generation_api(&serde_json::json!({"architecture":{
            "tokenizer":"Router","output_modalities":["text","image"]
        }})),
        None
    );
}

#[test]
fn native_mlx_serve_image_metadata_works_under_any_endpoint_provider() {
    use crate::llm::profiles::ImageGenerationApi;
    for id in [
        "ddalcu/Krea-2-Turbo-MLX-Serve-mixed-4-8",
        "mlx-community/flux2-klein-9b-4bit",
    ] {
        let row = serde_json::json!({"id": id, "owned_by":"mlx-serve", "loaded":false, "capabilities":["image"]});
        assert_eq!(declared_kind(&row), ModelKind::Image);
        assert_eq!(
            image_generation_api(&row),
            Some(ImageGenerationApi::MlxServe)
        );
    }
    let vision = serde_json::json!({"owned_by":"mlx-serve", "capabilities":["chat","vision"]});
    assert_eq!(declared_kind(&vision), ModelKind::Chat);
    assert_eq!(image_generation_api(&vision), None);
    assert_eq!(
        declared_kind(&serde_json::json!({"owned_by":"mlx-serve", "capabilities":["audio"]})),
        ModelKind::Audio
    );
}

#[test]
fn nested_diffusion_snapshots_resolve_without_root_config_and_reject_missing_shards() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    for part in ["transformer", "vae", "text_encoder"] {
        fs::create_dir_all(path.join(part)).unwrap();
        fs::write(path.join(part).join("0.safetensors"), b"weights").unwrap();
        fs::write(
            path.join(part).join("model.safetensors.index.json"),
            r#"{"weight_map":{"w":"0.safetensors"}}"#,
        )
        .unwrap();
    }
    assert_eq!(snapshot(path), Some(None));
    assert_eq!(local_model_metadata(path).kind, ModelKind::Image);
    assert_eq!(
        Path::new(&resolve_local_model_in(path.to_str().unwrap(), None, None).unwrap()),
        fs::canonicalize(path).unwrap()
    );
    fs::remove_file(path.join("vae/0.safetensors")).unwrap();
    assert!(snapshot(path).is_none());
    fs::write(
        path.join("vae/model.safetensors.index.json"),
        r#"{"weight_map":{"w":"../transformer/0.safetensors"}}"#,
    )
    .unwrap();
    assert!(
        snapshot(path).is_none(),
        "index cannot escape its component"
    );
}

#[test]
fn krea_root_transformer_layout_is_an_image_not_chat() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("config.json"),
        r#"{"model_type":"krea2_turbo"}"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("transformer_mixed_4_8.safetensors"),
        b"weights",
    )
    .unwrap();
    for part in ["vae", "text_encoder"] {
        fs::create_dir_all(dir.path().join(part)).unwrap();
        fs::write(dir.path().join(part).join("model.safetensors"), b"weights").unwrap();
    }
    assert_eq!(snapshot(dir.path()), Some(None));
    assert_eq!(local_model_metadata(dir.path()).kind, ModelKind::Image);
}

/// Direct provider discovery: an exact documented id is annotated only when the live row declares
/// no image output, and the row's own value (including `Disabled`) always wins.
#[test]
fn direct_image_providers_are_annotated_by_exact_documented_ids() {
    use crate::llm::profiles::ImageGenerationApi;
    let openai = test_probe("https://api.openai.com/v1".into(), "openai");
    let entries = parse_models(
        &serde_json::json!({"data":[
            {"id":"gpt-image-1"},
            {"id":"gpt-image-2.5-flare"},
            {"id":"gpt-4o"},
            {"id":"gpt-image-1-custom"}
        ]}),
        &openai,
    )
    .unwrap();
    let api = |i: usize| entries[i].metadata.image_generation;
    assert_eq!(api(0), Some(ImageGenerationApi::Images));
    assert_eq!(api(1), Some(ImageGenerationApi::Images));
    assert_eq!(api(2), None);
    assert_eq!(api(3), None);
    assert_eq!(entries[0].metadata.kind, ModelKind::Image);
    assert!(!entries[0].supports_chat());
    assert!(entries[0].supports_images());

    let xai = test_probe("https://api.x.ai/v1".into(), "xai");
    let entries = parse_models(
        &serde_json::json!({"data":[{"id":"grok-imagine-image-2.0"},{"id":"grok-4"}]}),
        &xai,
    )
    .unwrap();
    assert_eq!(
        entries[0].metadata.image_generation,
        Some(ImageGenerationApi::Images)
    );
    assert_eq!(entries[1].metadata.image_generation, None);
    assert_eq!(entries[0].metadata.kind, ModelKind::Image);
    assert!(!entries[0].supports_chat());
    assert!(entries[0].supports_images());

    // Gemini lists purpose only through generation methods; the native image transport is exact.
    let gemini = test_probe(
        "https://generativelanguage.googleapis.com/v1beta".into(),
        "gemini",
    );
    let entries = parse_models(
        &serde_json::json!({"models":[
            {"name":"models/gemini-2.5-flash-image","supportedGenerationMethods":["generateContent"]},
            {"name":"models/gemini-3-pro-image-preview","supportedGenerationMethods":["generateContent"]},
            {"name":"models/gemini-3.1-flash-image","supportedGenerationMethods":["generateContent"]},
            {"name":"models/gemini-2.5-flash","supportedGenerationMethods":["generateContent"]}
        ]}),
        &gemini,
    )
    .unwrap();
    assert_eq!(entries[0].selection.model, "gemini-2.5-flash-image");
    for entry in &entries[..3] {
        assert_eq!(
            entry.metadata.image_generation,
            Some(ImageGenerationApi::Gemini),
            "{}",
            entry.selection.model
        );
        assert!(entry.accepts_image_role(), "{}", entry.selection.model);
    }
    assert_eq!(entries[3].metadata.image_generation, None);
    assert!(!entries[3].accepts_image_role());
}

/// Documented ids never apply on a custom or local endpoint, and a row that declares text-only
/// output stays disabled. Vision input never implies image output.
#[test]
fn image_annotation_ignores_custom_endpoints_disabled_rows_and_vision() {
    use crate::llm::profiles::ImageGenerationApi;
    let custom = test_probe("http://127.0.0.1:1234/v1".into(), "openai");
    let entries =
        parse_models(&serde_json::json!({"data":[{"id":"gpt-image-1"}]}), &custom).unwrap();
    assert_eq!(entries[0].metadata.image_generation, None);

    let openai = test_probe("https://api.openai.com/v1".into(), "openai");
    let entries = parse_models(
        &serde_json::json!({"data":[
            {"id":"gpt-image-1","output_modalities":["text"]},
            {"id":"gpt-4o","input_modalities":["text","image"]}
        ]}),
        &openai,
    )
    .unwrap();
    assert_eq!(
        entries[0].metadata.image_generation,
        Some(ImageGenerationApi::Disabled)
    );
    assert_eq!(entries[1].metadata.image_generation, None);
    assert_eq!(entries[1].metadata.vision, Some(true));
}

/// MLX-Serve rows keep their native declaration; the documented-id fallback never touches them.
#[test]
fn mlx_serve_image_annotation_is_unchanged_by_direct_discovery() {
    use crate::llm::profiles::ImageGenerationApi;
    let probe = test_probe("http://127.0.0.1:8080/v1".into(), "mlx");
    let entries = parse_models(
        &serde_json::json!({"data":[
            {"id":"mlx-community/flux2-klein-9b-4bit","owned_by":"mlx-serve","capabilities":["image"]},
            {"id":"gpt-image-1","owned_by":"mlx-serve","capabilities":["chat","vision"]}
        ]}),
        &probe,
    )
    .unwrap();
    assert_eq!(
        entries[0].metadata.image_generation,
        Some(ImageGenerationApi::MlxServe)
    );
    assert_eq!(entries[0].metadata.kind, ModelKind::Image);
    assert_eq!(entries[1].metadata.image_generation, None);
}

/// `supports_images` follows the explicit declaration: `Disabled` vetoes even an image-kind row,
/// any declared transport enables it, and only an undeclared row falls back to its purpose.
#[test]
fn supports_images_lets_explicit_disabled_veto_image_kind() {
    use crate::llm::profiles::ImageGenerationApi;
    let openai = test_probe("https://api.openai.com/v1".into(), "openai");
    let entries = parse_models(
        &serde_json::json!({"data":[
            {"id":"gpt-image-1","type":"image","output_modalities":["text"]},
            {"id":"gpt-image-1-mini","type":"image"},
            {"id":"custom-image-model","type":"image"},
            {"id":"gpt-4o","type":"chat"}
        ]}),
        &openai,
    )
    .unwrap();
    assert_eq!(entries[0].metadata.kind, ModelKind::Image);
    assert_eq!(
        entries[0].metadata.image_generation,
        Some(ImageGenerationApi::Disabled)
    );
    assert!(!entries[0].supports_images());
    assert_eq!(
        entries[1].metadata.image_generation,
        Some(ImageGenerationApi::Images)
    );
    assert!(entries[1].supports_images());
    assert_eq!(entries[2].metadata.image_generation, None);
    assert!(entries[2].supports_images());
    assert!(!entries[3].supports_images());
    assert!(entries[3].supports_chat());
}
