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

/// A `/switch` argument split into the model selector and an optional `:level` reasoning suffix.
#[derive(Debug, PartialEq)]
pub(crate) struct Target {
    pub(crate) selector: String,
    pub(crate) effort: Option<ReasoningEffort>,
}

/// Splits a trailing `:level` only when `level` names a reasoning effort, so ids containing `:` still work.
pub(crate) fn parse_target(input: &str) -> Target {
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
pub(crate) fn resolve(config: &Config, selector: &str) -> Result<ModelSelection> {
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
            || crate::llm::catalog::provider_endpoint(provider).is_some())
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
/// `:level` effort set on that model's profile (in memory only; the profile must validate).
pub(crate) fn switched_config(
    config: &Config,
    selection: &ModelSelection,
    effort: Option<ReasoningEffort>,
) -> Result<Config> {
    let mut next = config.with_selection(selection);
    if let Some(effort) = effort {
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
                _ => bail!(
                    "reasoning metadata is unavailable for {}/{}; open /models and rescan, or select an explicit profile",
                    selection.provider,
                    selection.model
                ),
            },
        };
        let profile = next
            .model_profiles
            .get_mut(&key)
            .context("model profile disappeared while switching")?;
        profile.reasoning_effort = Some(effort);
        profile.validate()?;
    }
    Ok(next)
}

impl Agent<'_> {
    /// The config in force for this session: the `/switch` override when set, else the configured one.
    pub(crate) fn effective_config(&self) -> &Config {
        self.model_override.as_ref().unwrap_or(self.config)
    }

    /// Makes `selection` the session-only model for later turns. The config on disk is untouched.
    /// Refused while a turn holds the backend.
    pub(crate) fn switch_model(
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
        self.model_override = Some(config);
        self.route.reset();
        // The loaded-size cap belonged to the previous server/model; the next probe re-reads it.
        self.context_cap = None;
        self.modes.switch = Some(Saved {
            selection: selection.clone(),
            effort,
        });
        self.save_modes()?;
        Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::NullUi;
    use crate::config::types::Version;
    use crate::llm::model_config::ApiProtocol;
    use crate::llm::provider::{Backend, ProviderKind};
    use serde_json::json;
    use std::path::PathBuf;

    fn reply(text: &str) -> crate::llm::fake::Script {
        let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
        crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
    }

    #[test]
    fn switch_applies_to_later_turns_and_leaves_the_config_untouched() {
        let config = Config::default();
        let server = crate::llm::fake::serve(vec![reply("one"), reply("two")]);
        let base_url = format!("{}/v1", server.url);
        let backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: base_url.clone(),
                model: "alpha".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        );
        let workspace = tempfile::tempdir().unwrap();
        let version = Version {
            id: 0,
            parent: None,
            kind: "config".into(),
            harness: config.harness.clone(),
            adapter: None,
            status: "active".into(),
        };
        let mut agent =
            Agent::hermetic(&config, backend, &version, workspace.path(), None, None).unwrap();
        agent.turn("first", &mut NullUi).unwrap();

        let selection = ModelSelection {
            provider: "switch-test".into(),
            model: "beta".into(),
            base_url: Some(base_url),
            protocol: Some(ApiProtocol::ChatCompletions),
            ..Default::default()
        };
        agent.switch_model(&selection, None).unwrap();
        agent.turn("second", &mut NullUi).unwrap();

        assert_eq!(agent.effective_config().selection().model, "beta");
        assert_eq!(config.selection(), Config::default().selection());
        drop(agent);
        let requests = server.requests();
        assert_eq!(crate::llm::fake::body(&requests[0])["model"], "alpha");
        assert_eq!(crate::llm::fake::body(&requests[1])["model"], "beta");
    }

    #[test]
    fn parse_target_splits_only_known_effort_levels() {
        assert_eq!(
            parse_target("anthropic/claude:high"),
            Target {
                selector: "anthropic/claude".into(),
                effort: Some(ReasoningEffort::High)
            }
        );
        assert_eq!(
            parse_target("model:v2"),
            Target {
                selector: "model:v2".into(),
                effort: None
            }
        );
    }
}
