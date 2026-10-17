use super::*;
use crate::llm::errors::ErrorClass;

#[test]
fn only_recoverable_provider_classes_trigger_model_fallback() {
    let error = anyhow::anyhow!(errors::Classified::new(
        ErrorClass::QuotaExhausted { reset_at: None },
        "quota"
    ));
    assert!(should_fallback(&error));
    let error = anyhow::anyhow!(errors::Classified::new(ErrorClass::Payload, "payload"));
    assert!(!should_fallback(&error));
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
