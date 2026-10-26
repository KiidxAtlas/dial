//! Configured model fallback selection after a request cannot be recovered in-place.
use std::collections::BTreeMap;

use crate::{
    config::types::{Config, FallbackOn},
    llm::{
        errors::{self, ErrorClass},
        model_config::ModelSelection,
    },
};

/// Whether a request failure is one for which an alternate model may help under the
/// `retry.fallback_on` policy. Usage limits are throttling and quota exhaustion.
pub fn should_fallback(error: &anyhow::Error, on: FallbackOn) -> bool {
    fallback_applies(&errors::classify(error), on)
}

/// [`should_fallback`] for an already classified failure.
pub fn fallback_applies(class: &ErrorClass, on: FallbackOn) -> bool {
    let usage_limit = matches!(
        class,
        ErrorClass::RateLimited { .. } | ErrorClass::QuotaExhausted { .. }
    );
    let other = matches!(class, ErrorClass::Transient | ErrorClass::Auth);
    match on {
        FallbackOn::Any => usage_limit || other,
        FallbackOn::UsageLimit => usage_limit,
        FallbackOn::ExceptUsageLimit => other,
    }
}

/// The chain that applies to `role` and `selected` in `chains`. Precedence: the role key, then the
/// exact `provider/model` key, then the `provider/*` wildcard.
fn chain_for<'a>(
    chains: &'a BTreeMap<String, Vec<String>>,
    role: &str,
    selected: &ModelSelection,
) -> Option<&'a Vec<String>> {
    chains
        .get(role)
        .or_else(|| chains.get(&format!("{}/{}", selected.provider, selected.model)))
        .or_else(|| chains.get(&format!("{}/*", selected.provider)))
}

/// Resolves one configured selector against `selected`: `@role` names a model role, `provider/model`
/// names a known provider's model, and a bare name is a model on `selected`'s provider. Credentials
/// and origin binding are resolved by Config normally. `None` for a blank selector.
pub fn resolve_selector(
    config: &Config,
    selected: &ModelSelection,
    item: &str,
) -> Option<ModelSelection> {
    let item = item.trim();
    if item.is_empty() {
        return None;
    }
    if let Some(role) = item.strip_prefix('@') {
        return Some(config.for_role(role).selection());
    }
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
            if let Some((url, key, protocol)) = crate::llm::catalog::provider_endpoint(provider) {
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
    Some(candidate)
}

/// Resolves `chain` in order, dropping `selected` itself and duplicates. `fresh_window` clears the
/// window a non-role selector would inherit from `selected`, so the candidate resolves its own.
fn resolve_chain(
    config: &Config,
    chain: &[String],
    selected: &ModelSelection,
    fresh_window: bool,
) -> Vec<ModelSelection> {
    let mut result = Vec::new();
    for item in chain {
        let Some(mut candidate) = resolve_selector(config, selected, item) else {
            continue;
        };
        if fresh_window && !item.trim().starts_with('@') {
            candidate.context_window = None;
        }
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

/// Resolve configured selectors in order. The role-specific chain takes precedence over an
/// exact `provider/model` chain, which takes precedence over a `provider/*` wildcard chain;
/// credentials and origin binding are resolved by Config normally.
pub fn fallback_models(
    config: &Config,
    role: &str,
    selected: &ModelSelection,
) -> Vec<ModelSelection> {
    chain_for(&config.retry.fallback_chains, role, selected)
        .map(|chain| resolve_chain(config, chain, selected, false))
        .unwrap_or_default()
}

/// Larger-window models `[compaction.promotion.targets]` offers in place of `selected`, in order.
/// Keys follow the same precedence as fallback chains (role key, exact `provider/model`, then
/// `provider/*`). A bare or `provider/model` target resolves its own window rather than inheriting
/// `selected`'s; an `@role` target keeps the window its role resolves.
pub fn promotion_targets(
    config: &Config,
    role: &str,
    selected: &ModelSelection,
) -> Vec<ModelSelection> {
    chain_for(&config.compaction.promotion.targets, role, selected)
        .map(|chain| resolve_chain(config, chain, selected, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
