use super::*;

#[test]
fn exact_kinds_come_from_documented_ids_and_never_from_families() {
    let openai = |model: &str| ModelSelection {
        provider: "openai".into(),
        model: model.into(),
        base_url: Some("https://api.openai.com/v1".into()),
        ..Default::default()
    };
    for id in [
        "text-embedding-3-small",
        "text-embedding-3-large",
        "text-embedding-ada-002",
    ] {
        assert_eq!(
            known_kind(&openai(id), ModelKind::Unknown),
            ModelKind::Embedding
        );
    }
    // Family prefixes, unlisted ids and a live-reported purpose are not overridden by the table.
    assert_eq!(
        known_kind(&openai("text-embedding-9"), ModelKind::Unknown),
        ModelKind::Unknown
    );
    assert_eq!(
        known_kind(&openai("gpt-4o"), ModelKind::Unknown),
        ModelKind::Unknown
    );
    assert_eq!(
        known_kind(&openai("text-embedding-3-small"), ModelKind::Chat),
        ModelKind::Chat
    );
    // The same id on a local or unrelated endpoint is not the documented OpenAI model.
    let local = ModelSelection {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        base_url: Some("http://127.0.0.1:8000/v1".into()),
        ..Default::default()
    };
    assert_eq!(known_kind(&local, ModelKind::Unknown), ModelKind::Unknown);
}
fn reply(prompt: u64, completion: u64, read: Option<u64>, write: Option<u64>) -> Reply {
    Reply {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        cached_prompt_tokens: read,
        cache_creation_tokens: write,
        ..Reply::default()
    }
}

#[test]
fn bundled_data_parses_and_registers_legacy_and_new_providers() {
    assert!(catalog().len() > 50);
    for id in [
        "gemini",
        "openai",
        "anthropic",
        "openrouter",
        "groq",
        "mistral",
        "deepseek",
    ] {
        assert!(builtin_provider(id).is_some_and(|p| p.listed), "{id}");
    }
    let (url, env, protocol) = provider_endpoint("together").unwrap();
    assert_eq!(
        (url, env, protocol),
        (
            "https://api.together.xyz/v1",
            Some("TOGETHER_API_KEY"),
            ApiProtocol::ChatCompletions
        )
    );
    assert_eq!(
        provider_endpoint("fireworks").unwrap().1,
        Some("FIREWORKS_API_KEY")
    );
    assert_eq!(
        provider_endpoint("anthropic").unwrap().0,
        "https://api.anthropic.com/v1"
    );
    assert!(builtin_provider("chatgpt").is_none() && builtin_provider("mlx").is_none());
    for provider in builtin_providers() {
        assert!(provider.base_url.starts_with("https://"), "{}", provider.id);
        assert!(
            provider.env_keys().next().is_some() || crate::llm::oauth::flow(&provider.id).is_some(),
            "{}",
            provider.id
        );
        if provider.protocol == ApiProtocol::Anthropic {
            assert!(provider.base_url.ends_with("/v1"), "{}", provider.id);
        }
    }
}

#[test]
fn hosted_openai_compatible_providers_resolve_to_their_documented_origins() {
    for (id, url, env) in [
        ("chutes", "https://llm.chutes.ai/v1", "CHUTES_API_KEY"),
        (
            "featherless",
            "https://api.featherless.ai/v1",
            "FEATHERLESS_API_KEY",
        ),
        (
            "friendli",
            "https://api.friendli.ai/serverless/v1",
            "FRIENDLI_TOKEN",
        ),
        (
            "github-models",
            "https://models.github.ai/inference",
            "GITHUB_MODELS_TOKEN",
        ),
        (
            "hyperbolic",
            "https://api.hyperbolic.xyz/v1",
            "HYPERBOLIC_API_KEY",
        ),
        (
            "nebius",
            "https://api.tokenfactory.nebius.com/v1",
            "NEBIUS_API_KEY",
        ),
        (
            "ovhcloud",
            "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
            "OVHCLOUD_API_KEY",
        ),
        ("portkey", "https://api.portkey.ai/v1", "PORTKEY_API_KEY"),
        (
            "requesty",
            "https://router.requesty.ai/v1",
            "REQUESTY_API_KEY",
        ),
        ("scaleway", "https://api.scaleway.ai/v1", "SCALEWAY_API_KEY"),
        ("upstage", "https://api.upstage.ai/v1", "UPSTAGE_API_KEY"),
    ] {
        assert_eq!(
            provider_endpoint(id),
            Some((url, Some(env), ApiProtocol::ChatCompletions)),
            "{id}"
        );
        assert!(!builtin_provider(id).unwrap().listed, "{id}");
    }
    // The generic GitHub token is read for GitHub Models but stays a shared, not origin-bound, key.
    let github = builtin_provider("github-models").unwrap();
    assert_eq!(github.shared_env, ["GITHUB_TOKEN"]);
    assert!(!github.env.iter().any(|key| key == "GITHUB_TOKEN"));
    assert!(
        builtin_provider("scaleway")
            .unwrap()
            .env
            .iter()
            .any(|key| key == "SCW_SECRET_KEY")
    );
}

#[test]
fn alternate_credential_names_are_read_when_the_primary_is_unset() {
    let set = |names: &'static [&'static str]| move |key: &str| names.contains(&key);
    let command = builtin_provider("commandcode").unwrap();
    assert_eq!(command.key_env(set(&[])), Some("COMMAND_CODE_API_KEY"));
    assert_eq!(command.key_env(set(&["CMD_API_KEY"])), Some("CMD_API_KEY"));
    let github = builtin_provider("github-models").unwrap();
    assert_eq!(github.key_env(set(&["GITHUB_TOKEN"])), Some("GITHUB_TOKEN"));
    assert_eq!(
        github.key_env(set(&["GITHUB_TOKEN", "GITHUB_MODELS_TOKEN"])),
        Some("GITHUB_MODELS_TOKEN")
    );
    let ollama = builtin_provider("ollama-cloud").unwrap();
    assert_eq!(ollama.key_env(set(&[])), Some("OLLAMA_API_KEY"));
    assert_eq!(
        ollama.key_env(set(&["OLLAMA_CLOUD_API_KEY"])),
        Some("OLLAMA_CLOUD_API_KEY")
    );
}

#[test]
fn perplexity_agent_and_stepfun_regions_use_their_documented_endpoints() {
    let endpoint = |id: &str| {
        let p = builtin_provider(id).unwrap();
        (p.base_url.as_str(), p.env[0].as_str(), p.protocol)
    };
    assert_eq!(
        endpoint("perplexity-agent"),
        (
            "https://api.perplexity.ai/v1",
            "PERPLEXITY_API_KEY",
            ApiProtocol::Responses
        )
    );
    assert_eq!(
        endpoint("stepfun"),
        (
            "https://api.stepfun.ai/v1",
            "STEPFUN_API_KEY",
            ApiProtocol::ChatCompletions
        )
    );
    assert_eq!(
        endpoint("stepfun-cn"),
        (
            "https://api.stepfun.com/v1",
            "STEPFUN_API_KEY",
            ApiProtocol::ChatCompletions
        )
    );
    // The bare `perplexity` id belongs to the Perplexity sign-in flow, not an API-key row.
    assert!(builtin_provider("perplexity").is_none());
}

#[test]
fn bundled_models_take_their_declared_wire_only_on_the_bundled_endpoint() {
    let zen = "https://opencode.ai/zen/v1";
    let wire = |model: &str| model_protocol("opencode-zen", zen, None, model);
    assert_eq!(wire("claude-opus-4-8"), Some(ApiProtocol::Anthropic));
    assert_eq!(wire("gpt-5.5"), Some(ApiProtocol::Responses));
    assert_eq!(wire("gemini-3.5-flash"), Some(ApiProtocol::Gemini));
    // Rows without a declared wire, and unknown models, keep the provider's protocol.
    assert_eq!(wire("glm-5.2"), None);
    assert_eq!(wire("not-a-bundled-model"), None);
    // The bundled default protocol and a trailing slash still count as the bundled route.
    assert_eq!(
        model_protocol(
            "opencode-zen",
            "https://opencode.ai/zen/v1/",
            Some(ApiProtocol::ChatCompletions),
            "claude-opus-4-8"
        ),
        Some(ApiProtocol::Anthropic)
    );
    // An operator-chosen protocol or endpoint is never rewritten.
    for (base, protocol) in [
        (zen, Some(ApiProtocol::Responses)),
        ("https://proxy.example.com/v1", None),
    ] {
        assert_eq!(
            model_protocol("opencode-zen", base, protocol, "claude-opus-4-8"),
            None
        );
    }
    assert_eq!(model_protocol("custom", zen, None, "claude-opus-4-8"), None);
    assert_eq!(
        model_protocol(
            "opencode-go",
            "https://opencode.ai/zen/go/v1",
            None,
            "gpt-5.6-luna"
        ),
        Some(ApiProtocol::Responses)
    );
    // Model rules that hold on the Messages API come with the routed Claude rows.
    assert!(
        bundled_model("opencode-zen", "claude-opus-5")
            .unwrap()
            .has(ModelFact::NoSampling)
    );
}

#[test]
fn declared_wires_stay_on_routable_protocols_of_bundled_providers() {
    for (provider, models) in catalog() {
        for entry in &models.entries {
            if let Some(protocol) = entry.protocol {
                assert!(routable(protocol), "{provider}/{}", entry.id);
                let builtin = builtin_provider(provider)
                    .unwrap_or_else(|| panic!("{provider}/{} has no bundled endpoint", entry.id));
                assert!(routable(builtin.protocol), "{provider}");
            }
        }
    }
}

#[test]
fn first_party_rows_price_direct_sessions() {
    for (provider, model) in [
        ("anthropic", "claude-opus-4-6"),
        ("openai", "gpt-5.5"),
        ("gemini", "gemini-3-pro-preview"),
        ("gemini", "models/gemini-3-pro-preview"),
        ("chatgpt", "gpt-5.6-sol"),
    ] {
        let found = bundled_model(provider, model).unwrap_or_else(|| panic!("{provider}/{model}"));
        assert!(
            found.list_price.is_some() && found.context_window.is_some(),
            "{provider}/{model}"
        );
    }
    let opus = bundled_model("anthropic", "Claude-Opus-4-6").unwrap();
    assert_eq!(opus.thinking, Some(ThinkingMode::AnthropicAdaptive));
    assert!(opus.thinking_efforts.contains(&ReasoningEffort::Max));
    // $5/M input, $25/M output, $0.50/M cache reads, $6.25/M cache writes.
    let cost = opus
        .list_price
        .unwrap()
        .reply_cost(&reply(1_000_000, 100_000, Some(400_000), Some(100_000)), 0)
        .unwrap();
    assert!(
        (cost - (500_000.0 * 5.0 + 100_000.0 * 25.0 + 400_000.0 * 0.5 + 100_000.0 * 6.25) / 1e6)
            .abs()
            < 1e-9
    );
}

#[test]
fn missing_usage_and_inconsistent_cache_counts_stay_unpriced() {
    let price = bundled_model("anthropic", "claude-opus-4-6")
        .unwrap()
        .list_price
        .unwrap();
    let mut partial = reply(10, 1, None, None);
    partial.completion_tokens = None;
    assert_eq!(price.reply_cost(&partial, 0), None);
    assert_eq!(price.reply_cost(&reply(10, 1, Some(11), None), 0), None);
}

#[test]
fn cache_writes_price_per_ttl_bucket() {
    let price = ListPrice {
        base: Rates::new([1.0, 2.0, 0.1, 3.75]).unwrap(),
        long_context: None,
        schedule: None,
    };
    let mut split = reply(1_000_000, 0, None, Some(1_000_000));
    split.cache_creation_5m_tokens = Some(400_000);
    split.cache_creation_1h_tokens = Some(600_000);
    // 400k at the 5m rate (3.75/M) plus 600k at the 1h rate (2x input = 2.0/M).
    assert!((price.reply_cost(&split, 0).unwrap() - 2.7).abs() < 1e-9);
    // Without a split, the whole write prices at the default write rate.
    let flat = reply(1_000_000, 0, None, Some(1_000_000));
    assert!((price.reply_cost(&flat, 0).unwrap() - 3.75).abs() < 1e-9);
}

#[test]
fn long_context_and_off_peak_schedules_follow_upstream_rules() {
    let price = ListPrice {
        base: Rates::new([1.0, 2.0, 0.1, 0.0]).unwrap(),
        long_context: Some(LongContext {
            threshold: 100,
            inclusive: false,
            rates: Rates::new([10.0, 20.0, 1.0, 0.0]).unwrap(),
        }),
        schedule: None,
    };
    assert!(
        (price
            .reply_cost(&reply(100, 1_000_000, None, None), 0)
            .unwrap()
            - (100.0 + 2_000_000.0) / 1e6)
            .abs()
            < 1e-12
    );
    assert!(
        (price
            .reply_cost(&reply(101, 1_000_000, None, None), 0)
            .unwrap()
            - (1010.0 + 20_000_000.0) / 1e6)
            .abs()
            < 1e-12
    );
    let scheduled = ListPrice {
        base: Rates::new([1.0, 2.0, 0.0, 0.0]).unwrap(),
        long_context: None,
        schedule: Some(Schedule {
            off_peak: 0.5,
            // Monday 01:00-04:00 UTC.
            peak: vec![(vec![1], 60, 240)],
            effective: vec![(10 * 86_400_000, Rates::new([4.0, 4.0, 0.0, 0.0]).unwrap())],
        }),
    };
    // 1970-01-05 was a Monday.
    let monday_2am = 4 * 86_400_000 + 2 * 3_600_000;
    let monday_5am = 4 * 86_400_000 + 5 * 3_600_000;
    let one_million = reply(1_000_000, 0, None, None);
    assert_eq!(scheduled.reply_cost(&one_million, monday_2am), Some(1.0));
    assert_eq!(scheduled.reply_cost(&one_million, monday_5am), Some(0.5));
    // From day 10 the replacement card applies (day 11 is a Monday, 02:00 is peak).
    assert_eq!(
        scheduled.reply_cost(&one_million, 11 * 86_400_000 + 2 * 3_600_000),
        Some(4.0)
    );
}

#[test]
fn selectors_resolve_qualified_bare_vendor_and_fireworks_wire_ids() {
    assert_eq!(
        known_context_window("anthropic/claude-opus-4-6"),
        Some(1_000_000)
    );
    assert_eq!(
        known_context_window("anthropic/claude-sonnet-4-6-20260514"),
        Some(1_000_000)
    );
    assert_eq!(
        model_capabilities("claude-opus-4-6").unwrap().provider,
        "anthropic"
    );
    assert!(model_capabilities("together/moonshotai/Kimi-K2.7-Code").is_some());
    assert_eq!(
        fireworks_public_id("accounts/fireworks/models/kimi-k2p6").as_deref(),
        Some("kimi-k2.6")
    );
    assert!(bundled_model("fireworks", "accounts/fireworks/models/kimi-k3").is_some());
    assert!(model_capabilities("definitely-not-a-model").is_none());
    for model in ["ééééé", "claude-sonnet-4-6-2026051x"] {
        assert!(bundled_model("anthropic", model).is_none(), "{model}");
    }
}

#[test]
fn selection_pricing_follows_the_endpoint_origin() {
    let selection = |provider: &str, base: Option<&str>| ModelSelection {
        provider: provider.into(),
        model: "claude-opus-4-6".into(),
        base_url: base.map(str::to_owned),
        ..Default::default()
    };
    assert_eq!(
        selection_catalog_provider(&selection(
            "anthropic",
            Some("https://api.anthropic.com/v1")
        )),
        Some("anthropic")
    );
    assert_eq!(
        selection_catalog_provider(&selection("custom", Some("https://api.together.xyz/v1/"))),
        Some("together")
    );
    assert_eq!(
        selection_catalog_provider(&selection(
            "anthropic",
            Some("https://proxy.example.com/v1")
        )),
        None
    );
    assert_eq!(
        selection_catalog_provider(&selection("custom", Some("http://127.0.0.1:1234/v1"))),
        None
    );
    assert_eq!(selection_catalog_provider(&selection("mlx", None)), None);
    assert_eq!(
        selection_catalog_provider(&selection(
            "fireworks",
            Some("https://api.fireworks.ai/inference/v1")
        )),
        Some("fireworks")
    );
    assert_eq!(
        selection_catalog_provider(&selection(
            "firepass",
            Some("https://api.fireworks.ai/inference/v1")
        )),
        Some("firepass")
    );
}

#[test]
fn inception_minimal_is_sent_as_instant_and_wire_maps_are_validated_at_load() {
    let model = bundled_model("inception", "mercury-2").expect("inception mercury-2 is bundled");
    assert!(model.thinking_efforts.contains(&ReasoningEffort::Minimal));
    assert_eq!(
        model
            .effort_wire
            .get(&ReasoningEffort::Minimal)
            .map(String::as_str),
        Some("instant")
    );

    let undeclared: Row =
        serde_json::from_str(r#"{"id":"x","e":["low"],"m":{"high":"h"}}"#).expect("row parses");
    assert!(validate_effort_wire(&undeclared).is_err());
    let blank: Row =
        serde_json::from_str(r#"{"id":"x","e":["low"],"m":{"low":" "}}"#).expect("row parses");
    assert!(validate_effort_wire(&blank).is_err());
}
