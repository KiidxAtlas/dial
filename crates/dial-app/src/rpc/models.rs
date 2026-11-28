//! Chat and role listings for RPC, plus the `state` and ACP model controls. Ordinary listings
//! use configured models, saved provider catalogs and downloads without network access. A chat
//! refresh uses available entries from the same provider scan as the terminal picker.
use crate::config::types::Config;
use crate::llm::catalog;
use crate::llm::model_config::ModelSelection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// One full discovery per RPC connection. A project/config rebuild or newer scan invalidates
/// in-flight writers; provider filters and picker purpose never truncate the shared snapshot.
#[derive(Default)]
pub(super) struct ModelScan(Mutex<ScanState>);

#[derive(Default)]
struct ScanState {
    generation: u64,
    rows: Option<Arc<[catalog::ModelEntry]>>,
}

impl ModelScan {
    pub(super) fn begin(&self) -> u64 {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.generation += 1;
        state.generation
    }

    pub(super) fn reset(&self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.generation += 1;
        state.rows = None;
    }

    pub(super) fn publish(
        &self,
        ticket: u64,
        rows: Vec<catalog::ModelEntry>,
    ) -> anyhow::Result<Arc<[catalog::ModelEntry]>> {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        anyhow::ensure!(
            ticket == state.generation,
            "model scan superseded; refresh the picker"
        );
        let rows: Arc<[_]> = rows.into();
        state.rows = Some(rows.clone());
        Ok(rows)
    }

    pub(super) fn snapshot(&self) -> Option<Arc<[catalog::ModelEntry]>> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .rows
            .clone()
    }
}

/// One listed model, keyed by `provider/model`.
struct Row {
    selection: ModelSelection,
    label: String,
    sources: Vec<String>,
    local: Option<catalog::ModelMetadata>,
    /// Whether the model generates images, when its listing or local metadata says.
    image_output: Option<bool>,
}

fn key(selection: &ModelSelection) -> String {
    format!("{}/{}", selection.provider, selection.model)
}

fn provider_disabled(config: &Config, provider: &str) -> bool {
    config
        .providers
        .get(provider)
        .is_some_and(|spec| !spec.enabled || spec.is_reset())
}

fn is_account_scoped_provider(provider: &str) -> bool {
    matches!(provider, "chatgpt" | "google-antigravity")
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
    chat_list(config, current, user_dir, provider, None)
}

/// Use the scan itself when available. Some signed-in subscriptions offer selectable
/// cached or bundled rows without writing a provider listing to disk.
pub(super) fn chat_list(
    config: &Config,
    current: &ModelSelection,
    user_dir: &Path,
    provider: Option<&str>,
    discovered: Option<&[catalog::ModelEntry]>,
) -> Vec<Value> {
    chat_rows(config, current, user_dir, provider, discovered)
        .into_values()
        .map(|row| {
            let is_current =
                row.selection.provider == current.provider && row.selection.model == current.model;
            describe(config, row, is_current)
        })
        .collect()
}

pub(super) fn select_chat(
    config: &Config,
    current: &ModelSelection,
    user_dir: &Path,
    provider: &str,
    model: &str,
    discovered: Option<&[catalog::ModelEntry]>,
) -> anyhow::Result<ModelSelection> {
    chat_rows(config, current, user_dir, Some(provider), discovered)
        .remove(&format!("{provider}/{model}"))
        .map(|row| row.selection)
        .ok_or_else(|| super::coded(dial_protocol::ErrorCode::NotFound, format!("unknown model {provider}/{model}: not a selectable chat model; refresh the picker")))
}

fn chat_rows(
    config: &Config,
    current: &ModelSelection,
    user_dir: &Path,
    provider: Option<&str>,
    discovered: Option<&[catalog::ModelEntry]>,
) -> BTreeMap<String, Row> {
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    fn add(
        rows: &mut BTreeMap<String, Row>,
        selection: &ModelSelection,
        label: Option<&str>,
        source: String,
    ) {
        let row = rows.entry(key(selection)).or_insert_with(|| Row {
            selection: selection.clone(),
            label: label.unwrap_or(&selection.model).to_owned(),
            sources: Vec::new(),
            local: None,
            image_output: None,
        });
        if !row.sources.contains(&source) {
            row.sources.push(source);
        }
    }
    let main = config.selection();
    if !provider_disabled(config, &main.provider) {
        add(&mut rows, &main, None, "main".into());
    }
    if !provider_disabled(config, &current.provider) {
        add(&mut rows, current, None, "current".into());
    }
    for (role, selection) in &config.model_roles {
        if !provider_disabled(config, &selection.provider) {
            add(&mut rows, selection, None, format!("role:{role}"));
        }
    }
    for selection in &config.model_favorites {
        if !provider_disabled(config, &selection.provider) {
            add(&mut rows, selection, None, "favorite".into());
        }
    }
    for (name, profile) in &config.model_profiles {
        if provider_disabled(config, &profile.provider) {
            continue;
        }
        let selection = ModelSelection {
            provider: profile.provider.clone(),
            model: profile.model.clone(),
            ..Default::default()
        };
        add(&mut rows, &selection, None, format!("profile:{name}"));
    }
    let mut providers: BTreeSet<String> = rows
        .values()
        .map(|row| row.selection.provider.clone())
        .collect();
    providers.extend(
        config
            .providers
            .iter()
            .filter(|(_, spec)| spec.enabled && !spec.is_reset())
            .map(|(name, _)| name.clone()),
    );
    if let Some(provider) = provider.filter(|p| !provider_disabled(config, p)) {
        providers.insert(provider.to_owned());
    }
    // Downloaded local models and the saved listings of enabled providers. Models whose
    // metadata declares a non-chat kind (embedding, rerank, image, audio) cannot be the main model.
    let mut listed_providers: BTreeSet<String> = BTreeSet::new();
    let mut non_chat = BTreeSet::new();
    for entry in picker_entries(config, user_dir, provider, discovered) {
        let source = entry_source(&entry);
        if source == "listing" {
            listed_providers.insert(entry.selection.provider.clone());
        }
        if !entry.supports_chat() {
            non_chat.insert(key(&entry.selection));
            continue;
        }
        if !entry.available && (discovered.is_some() || source == "local") {
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
            // An explicit configured route wins intact; never transplant credentials or
            // transport policy from one endpoint onto another. A bare ID takes the scan route.
            let mut bare = ModelSelection::new(&row.selection.provider, &row.selection.model);
            bare.context_window = row.selection.context_window;
            bare.thinking.clone_from(&row.selection.thinking);
            if row.selection == bare {
                row.selection = entry.selection.clone();
                row.selection.context_window = bare.context_window.or(row.selection.context_window);
                if bare.thinking.is_some() {
                    row.selection.thinking = bare.thinking;
                }
            }
            row.label.clone_from(&entry.label);
            row.image_output = Some(entry.supports_images());
            row.local = Some(entry.metadata);
        }
    }
    // Fall back to static bundled catalog entries only when an enabled provider has no saved live
    // listing yet and is not strictly account-scoped (ChatGPT/Antigravity listings come from the
    // signed-in account, never static bundled IDs).
    for provider in &providers {
        if discovered.is_some()
            || listed_providers.contains(provider)
            || is_account_scoped_provider(provider)
        {
            continue;
        }
        for model in catalog::bundled_models(provider) {
            let selection = ModelSelection {
                provider: provider.clone(),
                model: model.id.clone(),
                ..Default::default()
            };
            add(&mut rows, &selection, None, "catalog".into());
        }
    }
    rows.retain(|key, row| {
        !non_chat.contains(key)
            && !row.selection.model.trim().is_empty()
            && !row.selection.provider.trim().is_empty()
            && provider.is_none_or(|p| p == row.selection.provider)
    });
    rows
}

/// Shared source for all picker purposes. Account-scoped rows from a completed scan are usable
/// only while the active account's offline catalog still lists them. Never trust client routes.
fn picker_entries(
    config: &Config,
    user_dir: &Path,
    provider: Option<&str>,
    discovered: Option<&[catalog::ModelEntry]>,
) -> Vec<catalog::ModelEntry> {
    let wanted = |e: &catalog::ModelEntry| {
        !provider_disabled(config, &e.selection.provider)
            && provider.is_none_or(|p| p == e.selection.provider)
    };
    if let Some(discovered) = discovered {
        let mut scoped = None;
        let mut entries: Vec<_> = discovered
            .iter()
            .filter(|e| wanted(e))
            .filter_map(|entry| {
                let entry =
                    if is_account_scoped_provider(&entry.selection.provider) && entry.available {
                        let active = scoped.get_or_insert_with(|| {
                            catalog::saved_subscription_models(user_dir)
                                .into_iter()
                                .map(|e| (key(&e.selection), e))
                                .collect::<BTreeMap<_, _>>()
                        });
                        let saved = active.get(&key(&entry.selection))?;
                        if entry.selection.account_id.is_some()
                            && entry.selection.account_id != saved.selection.account_id
                        {
                            return None;
                        }
                        saved
                    } else {
                        entry
                    };
                Some(entry.clone())
            })
            .collect();
        // A generation since the scan may have verified or refused this image route.
        catalog::image_capabilities::apply_cached(user_dir, &mut entries);
        return entries;
    }
    catalog::downloaded_local_models(user_dir)
        .into_iter()
        .filter(&wanted)
        .chain(
            catalog::saved_listing_models(user_dir)
                .into_iter()
                .filter(wanted)
                .filter(|e| saved_route_allowed(config, &e.selection)),
        )
        .collect()
}

fn entry_source(entry: &catalog::ModelEntry) -> &'static str {
    if entry.selection.provider == "mlx" && entry.selection.base_url.is_none() {
        "local"
    } else {
        "listing"
    }
}

// The on-disk endpoint catalog is shared between workspaces. Only restore a route still
// configured here, a builtin route, or the active subscription's account-scoped listing.
fn saved_route_allowed(config: &Config, saved: &ModelSelection) -> bool {
    let matches =
        |route: &ModelSelection| {
            if route.provider != saved.provider {
                return false;
            }
            let Some(endpoint) = config.selection_endpoint(route) else {
                return saved.base_url.is_none();
            };
            saved.base_url.as_deref().is_some_and(|url| {
                url.trim_end_matches('/') == endpoint.base_url.trim_end_matches('/')
            }) && saved.headers.as_deref().unwrap_or_default()
                == endpoint.headers.as_deref().unwrap_or_default()
                && (!saved.allow_insecure || endpoint.allow_insecure)
                && saved
                    .api_key_env
                    .as_ref()
                    .is_none_or(|env| Some(env) == endpoint.api_key_env.as_ref())
                && saved
                    .protocol
                    .is_none_or(|protocol| endpoint.protocol == Some(protocol))
        };
    saved.base_url.is_none()
        || matches(&ModelSelection::new(&saved.provider, &saved.model))
        || matches(&config.selection())
        || config
            .model_roles
            .values()
            .chain(&config.model_favorites)
            .any(matches)
}

/// What a non-chat `list_models` listing is for (`purpose` param).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Purpose {
    /// Models that generate images: the `image.model` setting and the `image` role.
    Image,
    /// Embedding models: the `embedding` role (semantic memory recall).
    Embedding,
}

impl Purpose {
    fn accepts(self, entry: &catalog::ModelEntry) -> bool {
        match self {
            Self::Image => entry.accepts_image_role(),
            Self::Embedding => entry.is_embedding(),
        }
    }
}

/// RPC `list_models` with `purpose: "image"` or `"embedding"`: the models a non-chat role or
/// setting can take. A model qualifies when its saved provider listing, persisted capability cache,
/// or local metadata admits it (`ModelEntry::accepts_image_role` for images, matching the terminal's
/// image role picker; `ModelEntry::is_embedding` for embeddings); names are never guessed. Offline
/// like `list`: downloaded local models and every saved `/models` listing, no network request.
/// `provider` is the id a role assignment saves and `provider_label` the name to show. `current`
/// marks the assigned `image` role (or `image.model` when unset) or the assigned `embedding` role;
/// `base_url` is the endpoint the selection needs and `image_api` (image rows) what `image.api`
/// needs.
pub(super) fn role_list(
    config: &Config,
    user_dir: &Path,
    provider: Option<&str>,
    purpose: Purpose,
    discovered: Option<&[catalog::ModelEntry]>,
) -> Vec<Value> {
    let image_role = config.model_roles.get("image");
    let embedding = config.model_roles.get("embedding");
    let mut rows: BTreeMap<String, Value> = BTreeMap::new();
    for entry in picker_entries(config, user_dir, provider, discovered) {
        let source = entry_source(&entry);
        if (discovered.is_some() && !entry.available) || !purpose.accepts(&entry) {
            continue;
        }
        rows.entry(key(&entry.selection)).or_insert_with(|| {
            let current = match purpose {
                Purpose::Image => image_role.map_or_else(
                    || entry.selection.model == config.image.model,
                    |role| {
                        role.provider == entry.selection.provider
                            && role.model == entry.selection.model
                    },
                ),
                Purpose::Embedding => embedding.is_some_and(|role| {
                    role.provider == entry.selection.provider && role.model == entry.selection.model
                }),
            };
            let mut row = json!({
                "provider": entry.selection.provider,
                "provider_label": entry.display_provider(),
                "model_id": entry.selection.model,
                "label": entry.label,
                "sources": [source],
                "current": current,
                "base_url": entry.selection.base_url,
            });
            if purpose == Purpose::Image {
                row["image_api"] = json!(entry.metadata.image_generation);
                let cached = catalog::image_capabilities::lookup(user_dir, &entry.selection);
                let (status, cap_source, checked_at) = match cached {
                    Some(rec) => (
                        rec.status.as_str(),
                        rec.source.as_str(),
                        (rec.checked_at > 0).then_some(rec.checked_at),
                    ),
                    None if entry.supports_images() => ("capable", "metadata", None),
                    None => ("candidate", "candidate", None),
                };
                row["image_capability"] = json!(status);
                row["image_capability_source"] = json!(cap_source);
                if let Some(ts) = checked_at {
                    row["image_checked_at"] = json!(ts);
                }
            }
            row
        });
    }
    rows.into_values().collect()
}

/// Probe configured/signed-in providers via `catalog::discover` and refresh the persistent
/// provider listing caches and image-capability cache (`image-capabilities.json`).
/// An explicit image refresh also retries previously refused hosted image routes. `provider`
/// narrows the reported `provider_status` (never the shared scan).
pub(super) fn refresh_and_check(
    layout: &crate::config::paths::Layout,
    config: &Config,
    purpose: Option<Purpose>,
    provider: Option<&str>,
) -> (Value, Vec<catalog::ModelEntry>) {
    if matches!(purpose, Some(Purpose::Image)) {
        crate::tools::net::image_gen::clear_refusals(&layout.user_dir);
    }
    let selections: Vec<ModelSelection> = std::iter::once(config.selection())
        .chain(config.model_favorites.iter().cloned())
        .chain(config.model_roles.values().cloned())
        .collect();
    let discovery = catalog::discover(layout, &config.providers, &selections);
    let checked_providers = discovery
        .providers
        .iter()
        .filter(|p| !matches!(p.state.as_str(), "not-set-up" | "auth-required"))
        .count();
    let mut capable = 0usize;
    let mut candidates = 0usize;
    for entry in &discovery.models {
        if !entry.available {
            continue;
        }
        if entry.supports_images() {
            capable += 1;
        } else if entry.accepts_image_role() {
            candidates += 1;
        }
    }
    let checked = json!({
        "providers": checked_providers,
        "capable": capable,
        "candidates": candidates,
        "checked_at": catalog::image_capabilities::last_updated_at(&layout.user_dir),
        "provider_status": provider_status(&discovery.providers, provider),
    });
    (checked, discovery.models)
}

/// Each probed provider's outcome as the terminal provider picker shows it. Probe details are
/// fixed descriptions (or HTTP status codes); an endpoint URL that carries credentials or a
/// query (possible through an environment override) is reported as null, so nothing here is
/// secret. `provider` keeps only that provider.
fn provider_status(providers: &[catalog::ProviderStatus], provider: Option<&str>) -> Value {
    providers
        .iter()
        .filter(|status| provider.is_none_or(|id| status.id == id))
        .map(|status| {
            let base_url = status
                .base_url
                .as_deref()
                .filter(|url| crate::llm::provider::endpoint_url(url, true).is_ok());
            json!({
                "id": status.id,
                "label": status.label,
                "base_url": base_url,
                "state": status.state,
                "detail": status.detail,
                "models": status.models,
            })
        })
        .collect()
}

fn describe(config: &Config, row: Row, current: bool) -> Value {
    let provider = &row.selection.provider;
    let model = &row.selection.model;
    let bundled = catalog::bundled_model(provider, model);
    let levels = thinking_levels(
        config,
        provider,
        model,
        row.local
            .as_ref()
            .and_then(|metadata| metadata.declared_reasoning),
    );
    let mut value = json!({
        "provider": provider,
        "model_id": model,
        "label": row.label,
        "sources": row.sources,
        "current": current,
        "thinking_levels": levels,
    });
    let window = row
        .local
        .as_ref()
        .and(row.selection.context_window)
        .or_else(|| bundled.as_ref()?.context_window);
    if let Some(window) = window {
        value["context_window"] = json!(window);
    }
    if let Some(bundled) = &bundled {
        value["max_output_tokens"] = json!(bundled.max_output_tokens);
        value["vision"] = json!(bundled.vision);
        value["reasoning"] = json!(bundled.reasoning);
        value["default_thinking"] = json!(bundled.default_effort.map(|effort| effort.as_str()));
        value["pricing"] = json!(bundled.pricing);
    }
    if let Some(metadata) = &row.local {
        if provider == "mlx" && row.sources.iter().any(|source| source == "local") {
            if let Some(format) = metadata.format {
                use crate::llm::local_runtime::LocalEngine;
                let key = LocalEngine::model_key(model);
                let choices = LocalEngine::choices(format, metadata.gguf_engines);
                if !choices.is_empty() {
                    let default_engine = LocalEngine::default_for(format, metadata.gguf_engines);
                    let mut installed = serde_json::Map::new();
                    for &choice in choices {
                        if choice != LocalEngine::Auto {
                            installed.insert(
                                choice.id().to_owned(),
                                json!(choice.is_installed(&config.user_dir)),
                            );
                        }
                    }
                    value["local_engine"] = json!({
                        "choices": choices,
                        "selected": config.local_model_engines.get(&key).copied().unwrap_or_default(),
                        "setting": format!("local_model_engines.{key}"),
                        "default": default_engine,
                        "installed": installed,
                    });
                }
            }
            let requested = config
                .local_context_window
                .tokens()
                .map(u64::from)
                .or_else(|| window.map(u64::from))
                .unwrap_or(32_768);
            if let Some((est, max_ctx, memory)) = crate::llm::local_fit::inspect_local_path(
                Path::new(model),
                requested,
                config.local_runtime.kv_bits,
            ) {
                value["memory_fit"] = json!({
                    "fit": est.fit,
                    "weights_bytes": est.weights_bytes,
                    "kv_bytes": est.kv_bytes,
                    "resident_bytes": est.resident_bytes,
                    "memory_bytes": memory,
                    "context_tokens": est.context_tokens,
                    "max_context_tokens": max_ctx,
                });
            }
        }
        if let Some(limit) = metadata.max_output_tokens {
            value["max_output_tokens"] = json!(limit);
        }
        if let Some(vision) = metadata.vision {
            value["vision"] = json!(vision);
        }
        if let Some(reasoning) = metadata.reasoning {
            value["reasoning"] = json!(reasoning);
        }
        value["kind"] = json!(metadata.kind);
    }
    if let Some(images) = row.image_output {
        value["image_output"] = json!(images);
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
/// efforts a model group's listing declares (`listed`; its wire IDs accept only those), else the
/// bundled catalog row's, else the efforts any other saved listing declares.
fn thinking_levels(
    config: &Config,
    provider: &str,
    model: &str,
    listed: Option<catalog::DeclaredReasoning>,
) -> Vec<&'static str> {
    let listed_efforts = |listed: catalog::DeclaredReasoning| {
        Some(listed.efforts()).filter(|levels| !levels.is_empty())
    };
    let grouped = listed
        .filter(|listed| listed.mode == catalog::ThinkingMode::ModelVariant)
        .and_then(listed_efforts);
    let declared = grouped
        .or_else(|| {
            thinking_profile(config, provider, model)
                .map(|profile| profile.capabilities.reasoning_efforts.clone())
                .filter(|levels| !levels.is_empty())
        })
        .or_else(|| catalog::bundled_model(provider, model).map(|bundled| bundled.thinking_efforts))
        .or_else(|| listed.and_then(listed_efforts))
        .or_else(|| local_thinking_levels(config, provider, model))
        .unwrap_or_default();
    declared.into_iter().map(|effort| effort.as_str()).collect()
}

fn local_thinking_levels(
    config: &Config,
    provider: &str,
    model: &str,
) -> Option<Vec<crate::llm::profiles::ReasoningEffort>> {
    let selection = config.selection();
    let is_active_local =
        config.local_main_model() && selection.provider == provider && selection.model == model;
    if provider != "local" && !is_active_local {
        return None;
    }
    let (base_url, token, headers) = if is_active_local {
        let base_url = config.endpoint.as_ref().map(|e| e.base_url.as_str());
        let token = config.endpoint.as_ref().and_then(|endpoint| {
            crate::llm::catalog::resolve_credential(
                &config.user_dir,
                &endpoint.base_url,
                endpoint.api_key_env.as_deref(),
            )
            .ok()
            .flatten()
        });
        (base_url, token, config.request_headers())
    } else {
        (None, None, &[][..])
    };
    let facts =
        crate::llm::local_server::facts(provider, model, base_url, token.as_deref(), headers);
    let levels = facts.effort_caps?.to_reasoning_efforts();
    (!levels.is_empty()).then_some(levels)
}

/// The reasoning profile `set_thinking` edits for the current model: the configured one, else the
/// session-only one built from catalog or saved-listing metadata (as `switch::switched_config`
/// builds it). `explicit` is true for a configured profile, whose level is an explicit choice.
fn current_reasoning(config: &Config) -> Option<(crate::llm::profiles::ModelProfile, bool)> {
    let selection = config.selection();
    if let Some(profile) = thinking_profile(config, &selection.provider, &selection.model) {
        // A saved model-group profile takes its levels from the current listing. When that listing
        // cannot be read the saved levels may be stale, so the model offers none rather than them.
        if profile.capabilities.reasoning
            == crate::llm::profiles::ReasoningApi::AntigravityModelVariant
        {
            return crate::config::thinking::reasoning_profile(config)
                .ok()
                .flatten()
                .map(|fresh| (fresh, true));
        }
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
        _ => thinking_levels(config, &selection.provider, &selection.model, None),
    };
    let (level, default_level) = match &reasoning {
        Some((profile, true)) => (profile.reasoning_effort, None),
        Some((profile, false)) => (None, profile.reasoning_effort),
        None => (None, None),
    };
    json!({
        "provider": selection.provider,
        "model_id": selection.model,
        "local": config.local_main_model(),
        "managed_local": config.runs_dial_mlx(),
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

/// RPC `delete_local_model`: deletes a downloaded local model's folder (or, with `dry_run`, only
/// reports what would be removed). `in_use` is every model the configuration and the session
/// select; those are refused. Refusals are coded: an unknown model is `NotFound`, a selected one
/// `Refused`, a malformed request `InvalidParams`, and a failed delete `Failed`.
pub(super) fn delete_local(params: &Value, in_use: &[String]) -> anyhow::Result<Value> {
    use super::coded;
    use dial_protocol::ErrorCode;
    if !params.is_object() {
        return Err(coded(ErrorCode::InvalidParams, "params must be an object"));
    }
    let model = params["model"]
        .as_str()
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            coded(
                ErrorCode::InvalidParams,
                "delete_local_model requires params.model (a local model path or label)",
            )
        })?;
    let dry_run = match params.get("dry_run") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return Err(coded(ErrorCode::InvalidParams, "dry_run must be a boolean"));
        }
    };
    let removal = catalog::plan_local_model_removal(model)
        .map_err(|error| coded(ErrorCode::NotFound, error))?;
    if !dry_run {
        catalog::remove_local_model(&removal.model, in_use).map_err(|error| {
            let code = if error.downcast_ref::<catalog::ModelInUse>().is_some() {
                ErrorCode::Refused
            } else {
                ErrorCode::Failed
            };
            coded(code, error)
        })?;
    }
    Ok(json!({
        "label": removal.label,
        "model": removal.model,
        "folder": removal.folder,
        "bytes": removal.bytes,
        "source": removal.source,
        "deleted": !dry_run,
    }))
}

#[cfg(test)]
mod tests;
