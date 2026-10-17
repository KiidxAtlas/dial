//! Model, role and reasoning selection: builds the config changes a choice implies.

use crate::config::selection::protocol as selection_protocol;
use crate::config::settings_store::{Change, Scope};
use crate::config::types::Config;
use crate::llm::model_config::{ModelSelection, ProviderSpec, ROLES};
use anyhow::{Result, ensure};

// ---------------------------------------------------------------------------
// Configuration changes as data (pure, so they can be tested without a terminal)
// ---------------------------------------------------------------------------

fn selection_headers<'a>(
    config: &'a Config,
    selection: &'a ModelSelection,
) -> &'a [crate::llm::model_config::RequestHeader] {
    selection
        .headers
        .as_deref()
        .or_else(|| {
            config
                .providers
                .get(&selection.provider)
                .filter(|provider| {
                    selection.base_url.as_ref().is_none_or(|url| {
                        url.trim_end_matches('/') == provider.base_url.trim_end_matches('/')
                    })
                })
                .and_then(|provider| provider.headers.as_deref())
        })
        .unwrap_or_default()
}

/// The `/model next` cycle set: the `--models` scope when installed, else saved favorites.
pub(super) fn cycle_set(config: &Config) -> Vec<ModelSelection> {
    crate::cli::model_scope::cycle_set().unwrap_or_else(|| config.model_favorites.clone())
}

/// Next model in the `/model next` cycle: the active `--models` scope when one is installed,
/// otherwise the saved favorites (`cycle_set`), restricted to `available` models.
pub(super) fn next_favorite(
    config: &Config,
    favorites: &[ModelSelection],
    available: &[ModelSelection],
) -> Option<ModelSelection> {
    let same_model = |a: &ModelSelection, b: &ModelSelection| {
        a.provider == b.provider
            && a.model == b.model
            && config
                .selection_base_url(a)
                .map(|url| url.trim_end_matches('/').to_owned())
                == config
                    .selection_base_url(b)
                    .map(|url| url.trim_end_matches('/').to_owned())
            && selection_protocol(config, a) == selection_protocol(config, b)
    };
    let same = |a: &ModelSelection, b: &ModelSelection| {
        same_model(a, b)
            && a.account_id == b.account_id
            && a.api_key_env == b.api_key_env
            && a.allow_insecure == b.allow_insecure
            && selection_headers(config, a) == selection_headers(config, b)
    };
    let mut candidates = Vec::new();
    for favorite in favorites {
        if let Some(selection) = available
            .iter()
            .find(|selection| same_model(favorite, selection))
        {
            let mut candidate = selection.clone();
            candidate.account_id = favorite.account_id.clone();
            candidate.headers = favorite.headers.clone();
            candidate.allow_insecure = favorite.allow_insecure
                || (favorite.base_url.is_none() && selection.allow_insecure);
            if let Some(variable) = &favorite.api_key_env {
                candidate.api_key_env = Some(variable.clone());
            }
            if !candidates.iter().any(|existing| same(existing, &candidate)) {
                candidates.push(candidate);
            }
        }
    }
    let current = config.selection();
    let index = candidates
        .iter()
        .position(|selection| same(selection, &current))
        .map_or(0, |index| (index + 1) % candidates.len());
    candidates.get(index).cloned()
}

/// The settings path a change writes.
pub(super) fn change_path(change: &Change) -> &str {
    match change {
        Change::Set { path, .. } | Change::Remove { path } => path,
    }
}

/// Make `role` follow the main model again. In the project layer an explicit marker is needed so a
/// global assignment does not show through.
pub(super) fn clear_role_changes(role: &str, scope: Scope) -> Result<Vec<Change>> {
    ensure!(
        ROLES.iter().any(|(name, _)| *name == role) && role != "main",
        "the main model cannot be cleared"
    );
    let changes = match scope {
        Scope::Global => vec![Change::Remove {
            path: format!("model_roles.{role}"),
        }],
        Scope::Project => vec![
            Change::Set {
                path: format!("model_roles.{role}.provider"),
                value: "inherit".into(),
            },
            Change::Set {
                path: format!("model_roles.{role}.model"),
                value: "".into(),
            },
        ],
    };
    Ok(changes)
}

pub(super) fn reasoning_profile(
    config: &Config,
) -> Result<Option<crate::llm::profiles::ModelProfile>> {
    if let Some(profile) = config.active_model_profile() {
        return Ok(Some(profile.clone()));
    }
    if config.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER)
        && let Some(profile) = crate::llm::chatgpt::model_profile(&config.user_dir, &config.model)?
    {
        return Ok(Some(profile));
    }
    let selection = config.selection();
    let identity = format!("{}/{}", selection.provider, selection.model);
    let Some(model) = crate::llm::catalog::model_capabilities(&identity) else {
        return Ok(None);
    };
    let Some(mode) = model.thinking else {
        return Ok(None);
    };
    use crate::llm::profiles::{ModelCapabilities, ModelProfile, ReasoningApi};
    let reasoning = match mode {
        crate::llm::catalog::ThinkingMode::Effort => {
            if selection.protocol == Some(crate::llm::model_config::ApiProtocol::Responses) {
                ReasoningApi::OpenaiResponses
            } else {
                ReasoningApi::OpenaiEffort
            }
        }
        crate::llm::catalog::ThinkingMode::Budget => match selection.provider.as_str() {
            "anthropic" => ReasoningApi::AnthropicBudget,
            "gemini" => ReasoningApi::GeminiBudget,
            _ => ReasoningApi::OpenaiEffort,
        },
        crate::llm::catalog::ThinkingMode::AnthropicAdaptive => ReasoningApi::AnthropicAdaptive,
        crate::llm::catalog::ThinkingMode::AnthropicBudgetEffort => ReasoningApi::AnthropicBudget,
        crate::llm::catalog::ThinkingMode::GoogleLevel => ReasoningApi::GeminiLevel,
    };
    Ok(Some(ModelProfile {
        provider: selection.provider,
        model: selection.model,
        capabilities: ModelCapabilities {
            reasoning,
            reasoning_efforts: model.thinking_efforts,
            ..Default::default()
        },
        reasoning_effort: model.default_effort,
        thinking: Some(true),
        ..Default::default()
    }))
}

pub(super) fn thinking_label(profile: &crate::llm::profiles::ModelProfile) -> String {
    use crate::llm::profiles::{ReasoningApi, ReasoningEffort};
    if profile.capabilities.reasoning == ReasoningApi::None {
        return "unsupported".into();
    }
    if profile.thinking == Some(false) || profile.reasoning_effort == Some(ReasoningEffort::None) {
        return "off".into();
    }
    if let Some(effort) = profile.reasoning_effort {
        return effort.as_str().into();
    }
    if profile.thinking == Some(true) {
        return "on".into();
    }
    "default".into()
}

pub(super) fn reasoning_changes(config: &Config, args: &str) -> Result<(Vec<Change>, String)> {
    if args == "auto" {
        // `auto` stores a flag instead of a profile: each user turn's effort is judged from its prompt.
        return Ok((
            vec![Change::Set {
                path: "thinking_auto".into(),
                value: toml::Value::Boolean(true),
            }],
            "auto".into(),
        ));
    }
    use crate::llm::profiles::{ReasoningApi, ReasoningEffort};
    let mut profile=reasoning_profile(config)?.ok_or_else(||anyhow::anyhow!("reasoning metadata is unavailable; open /models and rescan, or select an explicit /profile"))?;
    let supported = profile
        .capabilities
        .reasoning_efforts
        .iter()
        .map(|effort| effort.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    let label = if matches!(
        profile.capabilities.reasoning,
        ReasoningApi::AnthropicBudget
            | ReasoningApi::AnthropicAdaptive
            | ReasoningApi::GeminiBudget
            | ReasoningApi::OllamaThink
    ) && matches!(args, "on" | "off")
    {
        profile.thinking = Some(args == "on");
        if profile.capabilities.reasoning == ReasoningApi::GeminiBudget {
            if args == "off" {
                profile.thinking_budget = Some(0);
            } else if profile.thinking_budget == Some(0) {
                profile.thinking_budget = None;
            }
        } else if profile.capabilities.reasoning == ReasoningApi::OllamaThink {
            profile.reasoning_effort = None;
        }
        args.into()
    } else {
        let effort = match args {
            "on" => profile
                .reasoning_effort
                .filter(|effort| *effort != ReasoningEffort::None)
                .or_else(|| {
                    profile
                        .capabilities
                        .reasoning_efforts
                        .iter()
                        .copied()
                        .find(|effort| *effort != ReasoningEffort::None)
                }),
            "off" => Some(ReasoningEffort::None),
            level => serde_json::from_value::<ReasoningEffort>(serde_json::json!(level)).ok(),
        }
        .filter(|effort| profile.capabilities.reasoning_efforts.contains(effort))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "supported reasoning levels for {}: {supported}",
                config.model
            )
        })?;
        profile.reasoning_effort = Some(effort);
        profile.thinking = Some(effort != ReasoningEffort::None);
        effort.as_str().to_owned()
    };
    profile.validate()?;
    let name = config
        .model_profile
        .clone()
        .or_else(|| {
            config.active_model_profile().and_then(|_| {
                config
                    .model_profiles
                    .iter()
                    .find(|(_, saved)| {
                        saved.provider == profile.provider && saved.model == profile.model
                    })
                    .map(|(name, _)| name.clone())
            })
        })
        .unwrap_or_else(|| {
            use sha2::{Digest, Sha256};
            format!(
                "reasoning_{}",
                &format!(
                    "{:x}",
                    Sha256::digest(format!("{}/{}", profile.provider, profile.model))
                )[..16]
            )
        });
    Ok((
        vec![
            Change::Set {
                path: format!("model_profiles.{name}"),
                value: toml::Value::try_from(profile)?,
            },
            Change::Set {
                path: "model_profile".into(),
                value: name.into(),
            },
            Change::Set {
                path: "thinking_auto".into(),
                value: toml::Value::Boolean(false),
            },
        ],
        label,
    ))
}

pub(super) fn provider_changes(id: &str, spec: &ProviderSpec) -> Vec<Change> {
    let set = |field: &str, value: toml::Value| Change::Set {
        path: format!("providers.{id}.{field}"),
        value,
    };
    let mut changes = vec![
        set("label", spec.label.clone().into()),
        set("base_url", spec.base_url.clone().into()),
        set("enabled", spec.enabled.into()),
        set("allow_insecure", spec.allow_insecure.into()),
    ];
    changes.push(set(
        "protocol",
        spec.protocol
            .unwrap_or_else(|| crate::llm::model_config::ApiProtocol::builtin(id))
            .as_str()
            .into(),
    ));
    changes.push(set(
        "headers",
        crate::llm::model_config::headers_setting(&spec.headers),
    ));
    changes.push(set(
        "api_key_env",
        spec.api_key_env.clone().unwrap_or_default().into(),
    ));
    changes
}

pub(super) struct PendingFavorite {
    pub(super) main: ModelSelection,
    pub(super) favorites: Vec<ModelSelection>,
    pub(super) scope: Scope,
    pub(super) catalog_request: u64,
}

impl PendingFavorite {
    pub(super) fn current(&self, config: &Config, scope: Scope, request: u64) -> bool {
        let selection = config.selection();
        self.scope == scope
            && self.catalog_request == request
            && self.main.provider == selection.provider
            && self.main.model == selection.model
            && self.main.base_url == selection.base_url
            && selection_protocol(config, &self.main) == selection_protocol(config, &selection)
            && self.main.account_id == selection.account_id
            && self.main.headers == selection.headers
            && self.main.api_key_env == selection.api_key_env
            && self.main.allow_insecure == selection.allow_insecure
            && self.favorites == cycle_set(config)
    }
}

/// The configuration side of a local model's suggestion check; the overlay adds the profile's own
/// values (window, penalties, keep-alive, edit pin, draft model).
pub(super) fn local_suggestion_base(config: &Config) -> crate::llm::local_evidence::Current {
    let name = |value: serde_json::Result<serde_json::Value>| {
        value
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    crate::llm::local_evidence::Current {
        edit_mode: name(serde_json::to_value(
            config.resolved_edit_mode(&config.harness),
        )),
        edit_mode_pinned: config.pinned_edit_mode(&config.harness).is_some(),
        context_window: config.context_window,
        draft_model: config.local_runtime.draft_model.clone(),
        side_models: name(serde_json::to_value(config.assistance.local.side_models)),
        ..Default::default()
    }
}

pub(super) fn model_settings_profile(
    config: &Config,
) -> (String, crate::llm::profiles::ModelProfile) {
    use crate::llm::profiles::{ModelCapabilities, ModelProfile};
    let selection = config.selection();
    let profile = reasoning_profile(config).ok().flatten().unwrap_or_else(|| {
        config
            .model_profiles
            .values()
            .find(|p| p.provider == selection.provider && p.model == selection.model)
            .cloned()
            .unwrap_or_else(|| ModelProfile {
                provider: selection.provider.clone(),
                model: selection.model.clone(),
                capabilities: ModelCapabilities {
                    temperature: true,
                    top_p: true,
                    seed: matches!(selection.provider.as_str(), "mlx" | "lm-studio"),
                    ..Default::default()
                },
                ..Default::default()
            })
    });
    let existing = config
        .model_profile
        .clone()
        .filter(|_| config.active_model_profile().is_some())
        .or_else(|| {
            config
                .model_profiles
                .iter()
                .find(|(_, p)| p.provider == selection.provider && p.model == selection.model)
                .map(|(name, _)| name.clone())
        });
    let key = existing.unwrap_or_else(|| {
        use sha2::{Digest, Sha256};
        format!(
            "model_{:.16x}",
            Sha256::digest(format!("{}/{}", selection.provider, selection.model))
        )
    });
    (key, profile)
}
