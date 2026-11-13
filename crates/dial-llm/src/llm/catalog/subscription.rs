//! Subscription-backed catalogs (Anthropic seeds, OAuth-sourced listings).

use super::bundled;
use super::metadata::{ModelEntry, ModelMetadata};
use super::probe::Probe;
use super::{MAX_MODELS, ProviderStatus, builtin_provider, bundled_models};
use crate::llm::model_config::{ApiProtocol, ModelSelection, ProviderSpec};
use std::path::Path;

const ANTHROPIC_SUBSCRIPTION_SEEDS: &[(&str, &str)] = &[
    ("claude-opus-5-5", "Claude Opus 5.5"),
    ("claude-sonnet-5", "Claude Sonnet 5"),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
    ("claude-fable-5", "Claude Fable 5"),
    ("claude-mythos-5", "Claude Mythos 5"),
    ("claude-fable-5-1", "Claude Fable 5.1"),
    ("claude-mythos-5-1", "Claude Mythos 5.1"),
];
pub(super) fn anthropic_subscription_catalog(
    probe: &Probe,
    status: &mut ProviderStatus,
    stale: Option<&[ModelEntry]>,
) -> Option<Vec<ModelEntry>> {
    if probe.id != "anthropic"
        || !probe
            .token
            .as_deref()
            .is_some_and(crate::llm::anthropic::subscription_token)
    {
        return None;
    }
    let mut models = stale
        .unwrap_or_default()
        .iter()
        .filter(|entry| {
            entry
                .selection
                .protocol
                .is_none_or(|p| p == ApiProtocol::Anthropic)
        })
        .take(MAX_MODELS)
        .cloned()
        .collect::<Vec<_>>();
    if models.is_empty() {
        models = ANTHROPIC_SUBSCRIPTION_SEEDS
            .iter()
            .map(|(id, label)| {
                let selection = ModelSelection {
                    provider: "anthropic".into(),
                    model: (*id).into(),
                    base_url: Some(crate::llm::anthropic::BASE_URL.into()),
                    context_window: Some(1_000_000),
                    ..Default::default()
                };
                ModelEntry {
                    // The seeds carry no metadata of their own; their bundled records do.
                    metadata: bundled::complete_capabilities(&selection, ModelMetadata::default()),
                    selection,
                    label: (*label).into(),
                    source: "Claude subscription catalog (inference verifies model access)".into(),
                    available: true,
                }
            })
            .collect();
    }
    for model in &mut models {
        model.selection.provider = "anthropic".into();
        model.selection.base_url = Some(crate::llm::anthropic::BASE_URL.into());
        model.available = true;
        model.source =
            "Claude subscription catalog (cached; inference verifies model access)".into();
    }
    status.state = "cached".into();
    status.detail = format!(
        "Claude OAuth is signed in; live model discovery failed ({}). Cached or bundled models remain selectable; inference verifies model access.",
        status.detail
    );
    status.models = models.len();
    Some(models)
}
/// Subscription OAuth providers with a bundled catalog row. A signed-in grant lists its bundled
/// models without a network probe (the grant only reaches the endpoint at inference); a signed-out
/// grant stays visible as `auth-required` so /providers can start sign-in.
pub(super) fn oauth_catalog(
    user_dir: &Path,
    probed: &[(String, ProviderSpec)],
) -> Vec<(ProviderStatus, Vec<ModelEntry>)> {
    crate::llm::oauth::flows()
        .filter(|flow| !probed.iter().any(|(id, _)| id == flow.id))
        .filter_map(|flow| {
            let provider = builtin_provider(flow.id)?;
            let signed = crate::llm::oauth::signed_in(user_dir, flow.id);
            // A grant that routes by its own identifiers names that route; the template stays otherwise.
            let base = crate::llm::oauth::grant_route(user_dir, flow.id)
                .unwrap_or_else(|| provider.base_url.clone());
            let models: Vec<ModelEntry> = if signed {
                bundled_models(flow.id)
                    .into_iter()
                    .map(|model| ModelEntry {
                        selection: ModelSelection {
                            provider: flow.id.into(),
                            model: model.id.clone(),
                            base_url: Some(base.clone()),
                            protocol: Some(provider.protocol),
                            context_window: model.context_window,
                            ..Default::default()
                        },
                        label: model.id.clone(),
                        source: format!("{} bundled catalog", flow.name),
                        available: true,
                        metadata: model.metadata(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let status = ProviderStatus {
                id: flow.id.into(),
                label: provider.label.clone(),
                base_url: Some(base),
                state: if signed { "cached" } else { "auth-required" }.into(),
                detail: if signed {
                    "Signed in; models from the bundled catalog, verified at first request".into()
                } else {
                    "Sign in to use this subscription".into()
                },
                models: models.len(),
            };
            Some((status, models))
        })
        .collect()
}
