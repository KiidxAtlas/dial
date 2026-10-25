use super::*;
use crate::llm::errors::ErrorClass;

#[test]
fn only_recoverable_provider_classes_trigger_model_fallback() {
    let error = anyhow::anyhow!(errors::Classified::new(
        ErrorClass::QuotaExhausted { reset_at: None },
        "quota"
    ));
    assert!(should_fallback(&error, FallbackOn::Any));
    let error = anyhow::anyhow!(errors::Classified::new(ErrorClass::Payload, "payload"));
    assert!(!should_fallback(&error, FallbackOn::Any));
}

#[test]
fn role_chain_preserves_provider_for_bare_models_and_resolves_builtin_provider() {
    let mut config = Config::default();
    config.retry.fallback_chains.insert(
        "small".into(),
        vec!["next-model".into(), "together/meta-llama/Llama-3".into()],
    );
    let selected = ModelSelection {
        provider: "openai".into(),
        model: "primary".into(),
        ..Default::default()
    };
    let models = fallback_models(&config, "small", &selected);
    assert_eq!(models[0].provider, "openai");
    assert_eq!(models[0].model, "next-model");
    assert_eq!(models[1].provider, "together");
    assert_eq!(
        models[1].base_url.as_deref(),
        Some("https://api.together.xyz/v1")
    );
}

fn classified(class: ErrorClass) -> anyhow::Error {
    anyhow::anyhow!(errors::Classified::new(class, "failure"))
}

#[test]
fn fallback_on_gates_usage_limits_apart_from_other_recoverable_failures() {
    let usage = [
        classified(ErrorClass::RateLimited { retry_after: None }),
        classified(ErrorClass::QuotaExhausted { reset_at: None }),
    ];
    let other = [
        classified(ErrorClass::Transient),
        classified(ErrorClass::Auth),
    ];
    let never = [
        classified(ErrorClass::Payload),
        classified(ErrorClass::Fatal),
        classified(ErrorClass::ContextOverflow),
    ];
    for (on, usage_expected, other_expected) in [
        (FallbackOn::Any, true, true),
        (FallbackOn::UsageLimit, true, false),
        (FallbackOn::ExceptUsageLimit, false, true),
    ] {
        for error in &usage {
            assert_eq!(should_fallback(error, on), usage_expected, "{on:?} {error}");
        }
        for error in &other {
            assert_eq!(should_fallback(error, on), other_expected, "{on:?} {error}");
        }
        for error in &never {
            assert!(!should_fallback(error, on), "{on:?} {error}");
        }
    }
}

#[test]
fn fallback_on_parses_kebab_case_and_rejects_unknown_values() {
    let parse = |text: &str| toml::from_str::<crate::config::types::RetryConfig>(text);
    assert_eq!(parse("").unwrap().fallback_on, FallbackOn::Any);
    assert_eq!(
        parse("fallback_on = \"usage-limit\"").unwrap().fallback_on,
        FallbackOn::UsageLimit
    );
    assert_eq!(
        parse("fallback_on = \"except-usage-limit\"")
            .unwrap()
            .fallback_on,
        FallbackOn::ExceptUsageLimit
    );
    assert!(parse("fallback_on = \"sometimes\"").is_err());
}

#[test]
fn chain_keys_resolve_role_then_exact_identity_then_provider_wildcard() {
    let mut config = Config::default();
    let chains = &mut config.retry.fallback_chains;
    chains.insert("openai/*".into(), vec!["wild".into()]);
    let selected = ModelSelection {
        provider: "openai".into(),
        model: "primary".into(),
        ..Default::default()
    };
    let models = |config: &Config, role: &str| {
        fallback_models(config, role, &selected)
            .into_iter()
            .map(|m| m.model)
            .collect::<Vec<_>>()
    };
    assert_eq!(models(&config, "task"), ["wild"]);
    config
        .retry
        .fallback_chains
        .insert("openai/primary".into(), vec!["exact".into()]);
    assert_eq!(models(&config, "task"), ["exact"]);
    config
        .retry
        .fallback_chains
        .insert("task".into(), vec!["role".into()]);
    assert_eq!(models(&config, "task"), ["role"]);
    assert_eq!(models(&config, "small"), ["exact"]);
    // A wildcard for another provider never applies.
    let other = ModelSelection {
        provider: "anthropic".into(),
        model: "claude".into(),
        ..Default::default()
    };
    assert!(fallback_models(&config, "small", &other).is_empty());
}

fn sel(provider: &str, model: &str) -> ModelSelection {
    ModelSelection {
        provider: provider.into(),
        model: model.into(),
        ..Default::default()
    }
}

#[test]
fn promotion_targets_follow_the_fallback_key_precedence_without_touching_fallback_chains() {
    let mut config = Config::default();
    let selected = sel("openai", "primary");
    let promotion = &mut config.compaction.promotion.targets;
    promotion.insert("openai/*".into(), vec!["wild-long".into()]);
    let models = |config: &Config, role: &str| {
        promotion_targets(config, role, &selected)
            .into_iter()
            .map(|m| m.model)
            .collect::<Vec<_>>()
    };
    assert_eq!(models(&config, "default"), ["wild-long"]);
    config
        .compaction
        .promotion
        .targets
        .insert("openai/primary".into(), vec!["exact-long".into()]);
    assert_eq!(models(&config, "default"), ["exact-long"]);
    config
        .compaction
        .promotion
        .targets
        .insert("default".into(), vec!["role-long".into(), "primary".into()]);
    // The selected model itself is never a target.
    assert_eq!(models(&config, "default"), ["role-long"]);
    // Promotion tables and fallback chains are independent.
    assert!(fallback_models(&config, "default", &selected).is_empty());
    config
        .retry
        .fallback_chains
        .insert("default".into(), vec!["fb".into()]);
    assert_eq!(models(&config, "default"), ["role-long"]);
    assert_eq!(
        fallback_models(&config, "default", &selected)[0].model,
        "fb"
    );
}

#[test]
fn promotion_targets_resolve_their_own_window_but_roles_keep_theirs() {
    let mut config = Config::default();
    config.model_roles.insert(
        "big".into(),
        ModelSelection {
            context_window: Some(500_000),
            ..sel("openai", "big-model")
        },
    );
    config.compaction.promotion.targets.insert(
        "default".into(),
        vec!["bare".into(), "together/other".into(), "@big".into()],
    );
    let selected = ModelSelection {
        context_window: Some(8_192),
        ..sel("openai", "primary")
    };
    let targets = promotion_targets(&config, "default", &selected);
    assert_eq!(targets.len(), 3);
    assert_eq!(
        (targets[0].provider.as_str(), targets[0].model.as_str()),
        ("openai", "bare")
    );
    assert_eq!(
        targets[0].context_window, None,
        "the current window is not inherited"
    );
    assert_eq!(targets[1].provider, "together");
    assert_eq!(targets[2].context_window, Some(500_000));
}

#[test]
fn resolve_selector_matches_the_chain_semantics_and_skips_blank_entries() {
    let config = Config::default();
    let selected = sel("openai", "primary");
    assert!(resolve_selector(&config, &selected, "  ").is_none());
    let bare = resolve_selector(&config, &selected, " next ").unwrap();
    assert_eq!(
        (bare.provider.as_str(), bare.model.as_str()),
        ("openai", "next")
    );
    let cross = resolve_selector(&config, &selected, "together/meta-llama/Llama-3").unwrap();
    assert_eq!(cross.provider, "together");
    assert_eq!(
        cross.base_url.as_deref(),
        Some("https://api.together.xyz/v1")
    );
}
