//! Configured model fallback selection after a request cannot be recovered in-place.
use crate::{
    config::types::Config,
    llm::{
        errors::{self, ErrorClass},
        model_config::ModelSelection,
    },
};

/// Whether a request failure is one for which an alternate model may help.
pub fn should_fallback(error: &anyhow::Error) -> bool {
    matches!(
        errors::classify(error),
        ErrorClass::RateLimited { .. }
            | ErrorClass::QuotaExhausted { .. }
            | ErrorClass::Transient
            | ErrorClass::Auth
    )
}

/// Resolve configured selectors in order. The role-specific chain takes precedence over an
/// exact `provider/model` chain; credentials and origin binding are resolved by Config normally.
pub fn fallback_models(
    config: &Config,
    role: &str,
    selected: &ModelSelection,
) -> Vec<ModelSelection> {
    let identity = format!("{}/{}", selected.provider, selected.model);
    let chain = config
        .retry
        .fallback_chains
        .get(role)
        .or_else(|| config.retry.fallback_chains.get(&identity));
    let Some(chain) = chain else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for item in chain {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let candidate = if let Some(role) = item.strip_prefix('@') {
            config.for_role(role).selection()
        } else {
            let mut candidate = selected.clone();
            if let Some((provider, model)) = item.split_once('/')
                && (config.providers.contains_key(provider)
                    || crate::llm::catalog::builtin_provider(provider).is_some()
                    || matches!(
                        provider,
                        "mlx" | "chatgpt" | "gemini" | "ollama" | "anthropic"
                    ))
            {
                if provider != candidate.provider {
                    candidate = ModelSelection {
                        provider: provider.to_owned(),
                        model: model.to_owned(),
                        ..Default::default()
                    };
                    if let Some((url, key, protocol)) =
                        crate::llm::catalog::provider_endpoint(provider)
                    {
                        candidate.base_url = Some(url.to_owned());
                        candidate.api_key_env = key.map(str::to_owned);
                        candidate.protocol = Some(protocol);
                    }
                } else {
                    candidate.model = model.to_owned();
                }
            } else {
                candidate.model = item.to_owned();
            }
            candidate
        };
        if (candidate.provider != selected.provider || candidate.model != selected.model)
            && !result.iter().any(|seen: &ModelSelection| {
                seen.provider == candidate.provider && seen.model == candidate.model
            })
        {
            result.push(candidate);
        }
    }
    result
}

#[cfg(test)]
mod tests;
