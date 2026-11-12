//! Offline model listing for RPC `list_models`, the `state` model fields and the ACP model and
//! thought-level config options. The listing is exactly the set `set_model` accepts without a
//! network round trip (see `agent::modes::switch::is_known_model`): configured selections (main
//! model, roles, favorites, profiles), bundled catalog rows of the providers those selections and
//! `[providers]` name, and downloaded local models. Remote provider listings are never queried.
use crate::config::types::Config;
use crate::llm::catalog;
use crate::llm::model_config::ModelSelection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// One listed model, keyed by `provider/model`.
struct Row {
    provider: String,
    model: String,
    label: String,
    sources: Vec<String>,
    local: Option<catalog::ModelMetadata>,
}

fn key(selection: &ModelSelection) -> String {
    format!("{}/{}", selection.provider, selection.model)
}

/// Every model `set_model` accepts offline, sorted by `provider/model`. `current` is the model in
/// force (the session's `set_model` choice when there is one; it is listed even when the launched
/// configuration does not name it). `provider` narrows the listing to one provider and also lists
/// that provider's whole bundled catalog.
pub(super) fn list(
    config: &Config,
    current: &ModelSelection,
    user_dir: &Path,
    provider: Option<&str>,
) -> Vec<Value> {
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    fn add(
        rows: &mut BTreeMap<String, Row>,
        selection: &ModelSelection,
        label: Option<&str>,
        source: String,
    ) {
        let row = rows.entry(key(selection)).or_insert_with(|| Row {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            label: label.unwrap_or(&selection.model).to_owned(),
            sources: Vec::new(),
            local: None,
        });
        if !row.sources.contains(&source) {
            row.sources.push(source);
        }
    }
    add(&mut rows, &config.selection(), None, "main".into());
    add(&mut rows, current, None, "current".into());
    for (role, selection) in &config.model_roles {
        add(&mut rows, selection, None, format!("role:{role}"));
    }
    for selection in &config.model_favorites {
        add(&mut rows, selection, None, "favorite".into());
    }
    for (name, profile) in &config.model_profiles {
        let selection = ModelSelection {
            provider: profile.provider.clone(),
            model: profile.model.clone(),
            ..Default::default()
        };
        add(&mut rows, &selection, None, format!("profile:{name}"));
    }
    let mut providers: BTreeSet<String> = rows.values().map(|row| row.provider.clone()).collect();
    providers.extend(config.providers.keys().cloned());
    if let Some(provider) = provider {
        providers.insert(provider.to_owned());
    }
    for provider in &providers {
        for model in catalog::bundled_models(provider) {
            let selection = ModelSelection {
                provider: provider.clone(),
                model: model.id.clone(),
                ..Default::default()
            };
            add(&mut rows, &selection, None, "catalog".into());
        }
    }
    // Downloaded local models and the saved listings of the providers named above. Models whose
    // metadata declares a non-chat kind (embedding, rerank, image, audio) cannot be the main model.
    let listed = catalog::saved_listing_models(user_dir)
        .into_iter()
        .filter(|entry| providers.contains(&entry.selection.provider))
        .map(|entry| (entry, "listing"));
    let local = catalog::downloaded_local_models(user_dir)
        .into_iter()
        .map(|entry| (entry, "local"));
    for (entry, source) in local.chain(listed) {
        if !entry.supports_chat() {
            continue;
        }
        add(
            &mut rows,
            &entry.selection,
            Some(&entry.label),
            source.into(),
        );
        if let Some(row) = rows.get_mut(&key(&entry.selection))
            && row.local.is_none()
        {
            row.local = Some(entry.metadata);
        }
    }
    rows.into_values()
        .filter(|row| provider.is_none_or(|wanted| row.provider == wanted))
        .map(|row| {
            let is_current = row.provider == current.provider && row.model == current.model;
            describe(config, row, is_current)
        })
        .collect()
}

fn describe(config: &Config, row: Row, current: bool) -> Value {
    let bundled = catalog::bundled_model(&row.provider, &row.model);
    let levels = thinking_levels(config, &row.provider, &row.model);
    let mut value = json!({
        "provider": row.provider,
        "model_id": row.model,
        "label": row.label,
        "sources": row.sources,
        "current": current,
        "thinking_levels": levels,
    });
    if let Some(bundled) = &bundled {
        value["context_window"] = json!(bundled.context_window);
        value["max_output_tokens"] = json!(bundled.max_output_tokens);
        value["vision"] = json!(bundled.vision);
        value["reasoning"] = json!(bundled.reasoning);
        value["default_thinking"] = json!(bundled.default_effort.map(|effort| effort.as_str()));
        value["pricing"] = json!(bundled.pricing);
    } else if let Some(metadata) = &row.local {
        value["max_output_tokens"] = json!(metadata.max_output_tokens);
        value["vision"] = json!(metadata.vision);
        value["reasoning"] = json!(metadata.reasoning);
        value["kind"] = json!(metadata.kind);
    }
    value
}

/// The profile `set_thinking` would edit for `provider/model`: the configured named profile when
/// it matches, else the only matching one (the same choice `switch::switched_config` makes).
fn thinking_profile<'a>(
    config: &'a Config,
    provider: &str,
    model: &str,
) -> Option<&'a crate::llm::profiles::ModelProfile> {
    let matching: Vec<(&String, &crate::llm::profiles::ModelProfile)> = config
        .model_profiles
        .iter()
        .filter(|(_, profile)| profile.provider == provider && profile.model == model)
        .collect();
    match config.model_profile.as_ref() {
        Some(name) if matching.iter().any(|(own, _)| *own == name) => {
            config.model_profiles.get(name)
        }
        _ => match matching.as_slice() {
            [(_, only)] => Some(only),
            _ => None,
        },
    }
}

/// Reasoning levels declared for `provider/model`: the profile's documented efforts, else the
/// bundled catalog row's.
fn thinking_levels(config: &Config, provider: &str, model: &str) -> Vec<&'static str> {
    let declared = thinking_profile(config, provider, model)
        .map(|profile| profile.capabilities.reasoning_efforts.clone())
        .filter(|levels| !levels.is_empty())
        .or_else(|| catalog::bundled_model(provider, model).map(|bundled| bundled.thinking_efforts))
        .unwrap_or_default();
    declared.into_iter().map(|effort| effort.as_str()).collect()
}

/// The reasoning profile `set_thinking` edits for the current model: the configured one, else the
/// session-only one built from catalog or saved-listing metadata (as `switch::switched_config`
/// builds it). `explicit` is true for a configured profile, whose level is an explicit choice.
fn current_reasoning(config: &Config) -> Option<(crate::llm::profiles::ModelProfile, bool)> {
    let selection = config.selection();
    if let Some(profile) = thinking_profile(config, &selection.provider, &selection.model) {
        return Some((profile.clone(), true));
    }
    let ambiguous = config
        .model_profiles
        .values()
        .filter(|profile| {
            profile.provider == selection.provider && profile.model == selection.model
        })
        .count()
        > 1;
    if ambiguous {
        return None;
    }
    let mut cleared = config.clone();
    cleared.model_profile = None;
    crate::config::thinking::reasoning_profile(&cleared)
        .ok()
        .flatten()
        .map(|profile| (profile, false))
}

/// The model in force and its reasoning level, for `state` and ACP options. `level` is the explicit
/// level (null: the model's default, `default_level` when the catalog knows it); `settable`
/// reports whether `set_thinking` can choose one of `levels` for this model.
pub(super) fn current(config: &Config) -> Value {
    let selection = config.selection();
    let reasoning = current_reasoning(config);
    let levels: Vec<&str> = match &reasoning {
        Some((profile, _)) if !profile.capabilities.reasoning_efforts.is_empty() => profile
            .capabilities
            .reasoning_efforts
            .iter()
            .map(|effort| effort.as_str())
            .collect(),
        _ => thinking_levels(config, &selection.provider, &selection.model),
    };
    let (level, default_level) = match &reasoning {
        Some((profile, true)) => (profile.reasoning_effort, None),
        Some((profile, false)) => (None, profile.reasoning_effort),
        None => (None, None),
    };
    json!({
        "provider": selection.provider,
        "model_id": selection.model,
        "thinking": {
            "level": level.map(|effort| effort.as_str()),
            "default_level": default_level.map(|effort| effort.as_str()),
            "settable": reasoning.is_some() && !levels.is_empty(),
            "levels": levels,
        },
    })
}

/// ACP model options for the session. Select values are `provider/model`; thought levels are the
/// current model's declared levels. Sent with actor results so the adapter can render
/// `configOptions` without querying the actor. Without `with_list` the model list is omitted and
/// the adapter keeps the list it already has (prompts do not rescan local models).
pub(super) fn acp_options(config: &Config, user_dir: &Path, with_list: bool) -> Value {
    let current = current(config);
    let mut options = json!({
        "model": format!("{}/{}", current["provider"].as_str().unwrap_or_default(), current["model_id"].as_str().unwrap_or_default()),
        "thought_level": current["thinking"]["level"],
        "default_level": current["thinking"]["default_level"],
        "levels": current["thinking"]["levels"],
        "settable": current["thinking"]["settable"],
    });
    if with_list {
        let models: Vec<Value> = list(config, &config.selection(), user_dir, None)
            .into_iter()
            .map(|row| {
                json!({
                    "value": format!("{}/{}", row["provider"].as_str().unwrap_or_default(), row["model_id"].as_str().unwrap_or_default()),
                    "name": row["label"],
                })
            })
            .collect();
        options["models"] = json!(models);
    }
    options
}

#[cfg(test)]
mod tests;
