//! Subscription-backed catalogs (Anthropic seeds, OAuth-sourced listings).

use super::bundled;
use super::metadata::{DeclaredReasoning, ModelEntry, ModelMetadata};
use super::probe::Probe;
use super::{MAX_MODELS, ProviderStatus, builtin_provider, bundled_models};
use crate::llm::model_config::{ApiProtocol, ModelSelection, ProviderSpec};
use crate::llm::profiles::ReasoningEffort;
use std::collections::BTreeMap;
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
/// Effort suffixes an Antigravity wire ID may end with, longest first so `-extra-low` is not read
/// as `-low`. `extra-low` is the wire spelling of the minimal level.
const ANTIGRAVITY_EFFORT_SUFFIXES: [(&str, ReasoningEffort); 6] = [
    ("-extra-low", ReasoningEffort::Minimal),
    ("-xhigh", ReasoningEffort::Xhigh),
    ("-medium", ReasoningEffort::Medium),
    ("-high", ReasoningEffort::High),
    ("-low", ReasoningEffort::Low),
    // The wire ID of the off level, not a disabled flag on the row.
    ("-disabled", ReasoningEffort::None),
];
/// Representative preference when a family lists no unsuffixed row.
const ANTIGRAVITY_REPRESENTATIVE_ORDER: [ReasoningEffort; 6] = [
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::Low,
    ReasoningEffort::Xhigh,
    ReasoningEffort::Minimal,
    ReasoningEffort::None,
];

fn split_effort(model: &str) -> Option<(&str, ReasoningEffort)> {
    ANTIGRAVITY_EFFORT_SUFFIXES
        .iter()
        .find_map(|(suffix, effort)| {
            let family = model.strip_suffix(suffix)?;
            (!family.is_empty()).then_some((family, *effort))
        })
}

/// The model family of an Antigravity wire ID: the ID without one exact effort suffix. Other
/// segments are part of the family, so `gemini-4-hydrogen` and `gemini-4-hydrogen-tiered` differ.
pub fn antigravity_family(model: &str) -> &str {
    split_effort(model).map_or(model, |(family, _)| family)
}

/// The wire ID that selects `effort` within `model`'s family, or `None` when the effort has no
/// Antigravity suffix. Use it only for a level the account's listing declares.
pub fn antigravity_effort_model(model: &str, effort: ReasoningEffort) -> Option<String> {
    let (suffix, _) = ANTIGRAVITY_EFFORT_SUFFIXES
        .iter()
        .find(|(_, listed)| *listed == effort)?;
    Some(format!("{}{suffix}", antigravity_family(model)))
}

/// Build selectable rows from the account's wire IDs, never from a marketing name or quota label.
/// Rows sharing a family with listed effort suffixes become one `ModelVariant` row labelled with
/// the family; its representative is a listed wire ID. Internal and explicitly disabled rows
/// (`"isInternal": true`, `"disabled": true`) are skipped; an ID that merely contains a word is not.
pub(super) fn antigravity_models(
    base: &str,
    listing: &serde_json::Value,
) -> anyhow::Result<Vec<ModelEntry>> {
    use anyhow::Context;
    let rows = listing["models"]
        .as_object()
        .context("missing Antigravity models map")?;
    anyhow::ensure!(
        rows.len() <= MAX_MODELS,
        "model listing exceeded model limit"
    );
    let mut families: BTreeMap<&str, Vec<(&str, Option<ReasoningEffort>)>> = BTreeMap::new();
    for (id, row) in rows {
        let row = row.as_object().context("invalid Antigravity model entry")?;
        let flagged = |key: &str| row.get(key).and_then(|v| v.as_bool()) == Some(true);
        if !super::valid_model_id(id) || flagged("isInternal") || flagged("disabled") {
            continue;
        }
        let (family, effort) = match split_effort(id) {
            Some((family, effort)) => (family, Some(effort)),
            None => (id.as_str(), None),
        };
        families
            .entry(family)
            .or_default()
            .push((id.as_str(), effort));
    }
    let mut entries = Vec::new();
    for (family, members) in families {
        let mut entry = antigravity_group(base, family, &members);
        // The displayed limit must be safe when /effort switches between members. Prefer each
        // member's reported input capacity; do not mistake an output-token limit for context.
        entry.selection.context_window = members
            .iter()
            .filter_map(|(id, _)| {
                super::probe::context(&rows[*id])
                    .or_else(|| bundled::antigravity_context_window(id))
            })
            .min();
        entries.push(entry);
    }
    Ok(entries)
}

/// One grouped row. The representative is the family's unsuffixed listed ID when listed, else the
/// first listed effort in [`ANTIGRAVITY_REPRESENTATIVE_ORDER`]. Declared levels are the listed
/// suffixed efforts only.
fn antigravity_group(
    base: &str,
    family: &str,
    members: &[(&str, Option<ReasoningEffort>)],
) -> ModelEntry {
    let representative = members
        .iter()
        .find(|(_, effort)| effort.is_none())
        .or_else(|| {
            ANTIGRAVITY_REPRESENTATIVE_ORDER
                .iter()
                .find_map(|pref| members.iter().find(|(_, effort)| *effort == Some(*pref)))
        })
        .map_or(family, |(id, _)| *id);
    let mut efforts = members.iter().filter_map(|(_, effort)| *effort).peekable();
    let declared = efforts
        .peek()
        .is_some()
        .then(|| DeclaredReasoning::model_variant(efforts));
    antigravity_entry(base, representative, family, declared)
}

fn antigravity_entry(
    base: &str,
    model: &str,
    label: &str,
    declared: Option<DeclaredReasoning>,
) -> ModelEntry {
    let card = bundled::bundled_model("google-antigravity", model);
    let mut metadata = card.as_ref().map(|m| m.metadata()).unwrap_or_default();
    metadata.declared_reasoning = declared.or(metadata.declared_reasoning);
    ModelEntry {
        selection: ModelSelection {
            provider: "google-antigravity".into(),
            model: model.into(),
            base_url: Some(base.into()),
            protocol: Some(ApiProtocol::Antigravity),
            // Filled from the group's listed members after choosing its representative.
            context_window: None,
            ..Default::default()
        },
        label: label.into(),
        source: "Google Antigravity account model list".into(),
        available: true,
        metadata,
    }
}

/// Persist the listing under the account and project it was fetched with, then return its rows.
/// Without a known account and project nothing is saved: a later active account must not see it.
pub(super) fn save_antigravity_listing(
    user_dir: &Path,
    base: &str,
    identity: Option<(String, String)>,
    listing: &serde_json::Value,
) -> anyhow::Result<Vec<ModelEntry>> {
    let models = antigravity_models(base, listing)?;
    if let Some((account, project)) = identity {
        let saved = SavedAntigravityListing {
            scope: listing_scope(&account, &project),
            models: models.clone(),
        };
        let path = crate::config::paths::antigravity_listing_file(user_dir);
        crate::config::settings_store::write_private(&path, &serde_json::to_vec(&saved)?)?;
    }
    Ok(models)
}

/// The rows of the last saved Antigravity listing, only when it was fetched under the active
/// grant's account and project. Reads the grant file; no network request.
pub(super) fn saved_antigravity_models(user_dir: &Path) -> Vec<ModelEntry> {
    let Some((account, project)) =
        crate::llm::oauth::active_listing_identity(user_dir, "google-antigravity")
    else {
        return Vec::new();
    };
    let path = crate::config::paths::antigravity_listing_file(user_dir);
    let Some(bytes) = crate::platform::atomic::read_private(&path).ok().flatten() else {
        return Vec::new();
    };
    match serde_json::from_slice::<SavedAntigravityListing>(&bytes) {
        Ok(saved) if saved.scope == listing_scope(&account, &project) => saved.models,
        _ => Vec::new(),
    }
}

/// One saved listing and the scope it belongs to: a hash of the account and project, never the raw ids.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedAntigravityListing {
    scope: String,
    models: Vec<ModelEntry>,
}

fn listing_scope(account: &str, project: &str) -> String {
    use sha2::{Digest, Sha256};
    let scope = serde_json::json!(["dial-antigravity-listing-v1", account, project]);
    format!("{:x}", Sha256::digest(scope.to_string().as_bytes()))
}

/// Antigravity lists account models; other signed-in OAuth grants use bundled rows.
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
            if signed && flow.protocol == ApiProtocol::Antigravity {
                let result = crate::llm::usage_report::antigravity_available_models(user_dir)
                    .and_then(|(identity, listing)| {
                        save_antigravity_listing(user_dir, &base, identity, &listing)
                    });
                let (state, detail, models) = match result {
                    Ok(models) => (
                        "reachable",
                        "Models reported for the signed-in Antigravity project".to_owned(),
                        models,
                    ),
                    // Display only the operation-level message, not the underlying error chain.
                    Err(error) => ("unavailable", error.to_string(), Vec::new()),
                };
                return Some((
                    ProviderStatus {
                        id: flow.id.into(),
                        label: provider.label.clone(),
                        base_url: Some(base),
                        state: state.into(),
                        detail,
                        models: models.len(),
                    },
                    models,
                ));
            }
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
