//! `/switch [model]`: a session-only model override. The selection drives
//! later turns, fallbacks and role display; the config on disk is never written.

use crate::agent::{Agent, make_session_backend, session_auth_config};
use crate::config::types::Config;
use crate::llm::model_config::ModelSelection;
use crate::llm::profiles::ReasoningEffort;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The session's `/switch` selection, stored in mode state so it survives agent rebuilds and session swaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub selection: ModelSelection,
    pub effort: Option<ReasoningEffort>,
}

const EFFORTS: [ReasoningEffort; 8] = [
    ReasoningEffort::None,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::Xhigh,
    ReasoningEffort::Max,
    ReasoningEffort::Ultra,
];

/// The reasoning level named by `name` (`none`, `minimal`, … `ultra`), if it is one of them.
pub fn parse_effort(name: &str) -> Option<ReasoningEffort> {
    EFFORTS.into_iter().find(|effort| effort.as_str() == name)
}

/// A `/switch` argument split into the model selector and an optional `:level` reasoning suffix.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub selector: String,
    pub effort: Option<ReasoningEffort>,
}

/// Splits a trailing `:level` only when `level` names a reasoning effort, so ids containing `:` still work.
pub fn parse_target(input: &str) -> Target {
    let input = input.trim();
    if let Some((head, level)) = input.rsplit_once(':')
        && let Some(effort) = EFFORTS.into_iter().find(|e| e.as_str() == level)
    {
        return Target {
            selector: head.trim().to_owned(),
            effort: Some(effort),
        };
    }
    Target {
        selector: input.to_owned(),
        effort: None,
    }
}

/// Resolves a selector: `@role`, an exact `provider/id` among
/// the known models, `provider/id` for a known provider, then a unique case-insensitive id match.
pub fn resolve(config: &Config, selector: &str) -> Result<ModelSelection> {
    let selector = selector.trim();
    if let Some(role) = selector.strip_prefix('@') {
        if role == "main" {
            return Ok(config.selection());
        }
        return config
            .model_roles
            .get(role)
            .cloned()
            .with_context(|| format!("Unknown model role: @{role}"));
    }
    let candidates = std::iter::once(config.selection())
        .chain(config.model_roles.values().cloned())
        .chain(config.model_favorites.iter().cloned())
        .chain(
            config
                .model_profiles
                .values()
                .map(|profile| ModelSelection {
                    provider: profile.provider.clone(),
                    model: profile.model.clone(),
                    ..Default::default()
                }),
        );
    let mut known: BTreeMap<String, ModelSelection> = BTreeMap::new();
    for candidate in candidates {
        known
            .entry(format!("{}/{}", candidate.provider, candidate.model))
            .or_insert(candidate);
    }
    if let Some(hit) = known.get(selector) {
        return Ok(hit.clone());
    }
    if let Some((provider, model)) = selector.split_once('/')
        && !model.is_empty()
        && (config.providers.contains_key(provider)
            || crate::llm::catalog::provider_endpoint(provider).is_some()
            || crate::llm::catalog::is_local_provider(provider))
    {
        return Ok(ModelSelection {
            provider: provider.to_owned(),
            model: model.to_owned(),
            ..Default::default()
        });
    }
    let needle = selector.to_lowercase();
    let mut hits: Vec<&ModelSelection> = known
        .values()
        .filter(|s| s.model.to_lowercase() == needle)
        .collect();
    if hits.is_empty() {
        hits = known
            .values()
            .filter(|s| s.model.to_lowercase().contains(&needle))
            .collect();
    }
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("Unknown model: {selector}"),
        many => bail!(
            "Ambiguous model {selector}: {}",
            many.iter()
                .map(|s| format!("{}/{}", s.provider, s.model))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The config a session override runs under: `config` with `selection` active and, when given, the
/// `:level` effort set on that model's profile (in memory only; the profile must validate). A model
/// without a configured profile gets a session-only one from the reasoning controls the bundled
/// catalog or the saved provider listing declares, as `--thinking` and role thinking do.
pub(crate) fn switched_config(
    config: &Config,
    selection: &ModelSelection,
    effort: Option<ReasoningEffort>,
) -> Result<Config> {
    let next = config.with_selection(selection);
    let Some(effort) = effort else {
        return Ok(next);
    };
    // As `/thinking` does: an explicit thinking switch follows the chosen level.
    let apply = |profile: &mut crate::llm::profiles::ModelProfile| {
        profile.reasoning_effort = Some(effort);
        if profile.thinking.is_some() {
            profile.thinking = Some(effort != ReasoningEffort::None);
        }
    };
    let matching: Vec<String> = next
        .model_profiles
        .iter()
        .filter(|(_, p)| p.provider == selection.provider && p.model == selection.model)
        .map(|(name, _)| name.clone())
        .collect();
    let key = match next.model_profile.as_ref() {
        Some(name) if matching.contains(name) => name.clone(),
        _ => match matching.as_slice() {
            [only] => only.clone(),
            [] => {
                let mut cleared = next;
                cleared.model_profile = None;
                let mut profile = crate::config::thinking::reasoning_profile(&cleared)?
                    .with_context(|| {
                        format!(
                            "reasoning metadata is unavailable for {}/{}; open /models and rescan, or add a model profile",
                            selection.provider, selection.model
                        )
                    })?;
                apply(&mut profile);
                // Validates the profile and installs it as a session-only entry.
                return crate::config::thinking::install_thinking_profile(cleared, profile);
            }
            _ => bail!(
                "several model profiles match {}/{}; select one with model_profile",
                selection.provider,
                selection.model
            ),
        },
    };
    let mut next = next;
    let profile = next
        .model_profiles
        .get_mut(&key)
        .context("model profile disappeared while switching")?;
    apply(profile);
    profile.validate()?;
    Ok(next)
}

impl Agent<'_> {
    /// The config in force for this session: the `/switch` override when set, else the configured one.
    pub fn effective_config(&self) -> &Config {
        self.model_override.as_ref().unwrap_or(self.config)
    }

    /// Makes `selection` the session-only model for later turns. The config on disk is untouched.
    /// Refused while a turn holds the backend.
    pub fn switch_model(
        &mut self,
        selection: &ModelSelection,
        effort: Option<ReasoningEffort>,
    ) -> Result<()> {
        ensure!(
            self.backend.is_some(),
            "a turn is in progress; switch models between turns"
        );
        let config = switched_config(self.effective_config(), selection, effort)?;
        let backend = make_session_backend(&config, &self.session)?;
        let mut backend = if self.hermetic {
            backend.without_steering()
        } else {
            backend
        };
        backend.bind_session(&config, &mut self.session)?;
        let auth = session_auth_config(&config, &backend, &mut self.session)?;
        self.backend = Some(backend);
        self.auth_config = auth;
        self.cache_warm = None;
        self.model_override = Some(config);
        self.route.reset();
        self.record_main_model_route("Session main model switch installed the selected model (may reapply a recorded session switch); not worker assignment routing");
        // The loaded-size cap belonged to the previous server/model; the next probe re-reads it.
        self.context_cap = None;
        self.modes.switch = Some(Saved {
            selection: selection.clone(),
            effort,
        });
        self.save_modes()?;
        Ok(())
    }

    /// Drops the session's reasoning-level choice: the current model keeps running, at the level
    /// its configuration gives it. Refused while a turn holds the backend.
    pub fn reset_thinking(&mut self) -> Result<()> {
        let selection = self.effective_config().selection();
        let prior = self.model_override.take();
        let result = self.switch_model(&selection, None);
        if result.is_err() {
            self.model_override = prior;
        }
        result
    }

    /// Reapplies this session's saved `/switch` selection when no override is in memory (a rebuilt agent).
    /// A saved selection that fails to resolve is reported and the configured model is used instead.
    pub(crate) fn switch_resume(&mut self) {
        if self.model_override.is_some()
            || self.plan_model_prior.is_some()
            || self.modes.prewalk.handoff.is_some()
        {
            return;
        }
        let Some(saved) = self.modes.switch.clone() else {
            return;
        };
        if let Err(error) = self.switch_model(&saved.selection, saved.effort)
            && let Some(backend) = &mut self.backend
        {
            backend.remember_session_error(error);
        }
    }

    /// After a session swap: drops the previous session's override and rebuilds the model from this
    /// session's saved selection, or from the configured model when it has none.
    pub(crate) fn reinstall_session_model(&mut self) -> Result<()> {
        self.plan_model_prior = None;
        self.model_override = None;
        self.install_model_override(None)?;
        if let Some(saved) = self.modes.switch.clone() {
            self.switch_model(&saved.selection, saved.effort)?;
        }
        Ok(())
    }

    /// Runs later turns on `driver` while the `model_weights` tool keeps acting on `target`, the
    /// checkpoint of the model in force before. Returns the override to hand back to
    /// [`Self::restore_model_override`] when the request ends. Session-only: neither the config on
    /// disk nor the saved `/switch` selection changes. Refused while a turn holds the backend.
    pub fn drive_weights_with(
        &mut self,
        driver: &ModelSelection,
        target: &std::path::Path,
    ) -> Result<Option<Config>> {
        ensure!(
            self.backend.is_some(),
            "a turn is in progress; choose the driver model between turns"
        );
        let mut config = switched_config(self.effective_config(), driver, None)?;
        config.weights.model = target.to_string_lossy().into_owned();
        let prior = self.model_override.clone();
        self.install_model_override(Some(config))?;
        Ok(prior)
    }

    /// Reinstalls the override [`Self::drive_weights_with`] returned (`None`: the configured model).
    pub fn restore_model_override(&mut self, prior: Option<Config>) -> Result<()> {
        ensure!(
            self.backend.is_some(),
            "a turn is in progress; the session model is restored between turns"
        );
        self.install_model_override(prior)
    }
}

/// Whether `selection` names a chat model Dial can offer without a network round trip: a bundled
/// catalog entry, a configured selection (current model, role, favorite or profile), a downloaded
/// local model, or a model in a provider listing saved by an earlier `/models` refresh. Local and
/// listed models that declare a non-chat kind (embedding, rerank, image, audio) are excluded.
pub fn is_known_model(
    config: &Config,
    user_dir: &std::path::Path,
    selection: &ModelSelection,
) -> bool {
    let same = |a: &ModelSelection| a.provider == selection.provider && a.model == selection.model;
    let mut configured = std::iter::once(config.selection())
        .chain(config.model_roles.values().cloned())
        .chain(config.model_favorites.iter().cloned())
        .chain(
            config
                .model_profiles
                .values()
                .map(|profile| ModelSelection {
                    provider: profile.provider.clone(),
                    model: profile.model.clone(),
                    ..Default::default()
                }),
        );
    configured.any(|candidate| same(&candidate))
        || crate::llm::catalog::bundled_model(&selection.provider, &selection.model).is_some()
        || crate::llm::catalog::downloaded_local_models(user_dir)
            .iter()
            .chain(crate::llm::catalog::saved_listing_models(user_dir).iter())
            .any(|entry| entry.supports_chat() && same(&entry.selection))
}

#[cfg(test)]
mod tests;
