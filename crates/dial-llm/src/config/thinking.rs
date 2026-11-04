//! `--thinking` levels and their application to a loaded configuration.

use crate::config::types::Config;
use crate::llm::profiles::{ModelProfile, ReasoningApi, ReasoningEffort};
use anyhow::{Result, anyhow};

/// Thinking selectors accepted by `--thinking`, in display order.
const THINKING_SELECTORS: [(&str, CliThinking); 8] = [
    ("off", CliThinking::Off),
    ("minimal", CliThinking::Minimal),
    ("low", CliThinking::Low),
    ("medium", CliThinking::Medium),
    ("high", CliThinking::High),
    ("xhigh", CliThinking::XHigh),
    ("max", CliThinking::Max),
    ("auto", CliThinking::Auto),
];

/// `--thinking` selector: the thinking levels accepted on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliThinking {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Auto,
}

impl CliThinking {
    /// The reasoning effort a fixed level selects; `None` for `auto`, which is judged per user turn.
    pub fn reasoning_effort(self) -> Option<ReasoningEffort> {
        Some(match self {
            Self::Off => ReasoningEffort::None,
            Self::Minimal => ReasoningEffort::Minimal,
            Self::Low => ReasoningEffort::Low,
            Self::Medium => ReasoningEffort::Medium,
            Self::High => ReasoningEffort::High,
            Self::XHigh => ReasoningEffort::Xhigh,
            Self::Max => ReasoningEffort::Max,
            Self::Auto => return None,
        })
    }

    fn name(self) -> &'static str {
        THINKING_SELECTORS
            .iter()
            .find(|(_, level)| *level == self)
            .map_or("auto", |(name, _)| name)
    }
}

/// Parses `--thinking`: an exact selector, or an unambiguous prefix of at least two characters.
pub fn parse_thinking(value: &str) -> std::result::Result<CliThinking, String> {
    if let Some((_, level)) = THINKING_SELECTORS.iter().find(|(name, _)| *name == value) {
        return Ok(*level);
    }
    let invalid = || {
        format!(
            "Invalid --thinking value: {value:?}. Expected one of: {}.",
            THINKING_SELECTORS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    if value.len() < 2 {
        return Err(invalid());
    }
    let matches: Vec<CliThinking> = THINKING_SELECTORS
        .iter()
        .filter(|(name, _)| name.starts_with(value))
        .map(|(_, level)| *level)
        .collect();
    match matches.as_slice() {
        [level] => Ok(*level),
        _ => Err(invalid()),
    }
}

pub fn install_thinking_profile(mut config: Config, profile: ModelProfile) -> Result<Config> {
    profile.validate()?;
    let name = (0..)
        .map(|i| format!("__cli_thinking_{i}"))
        .find(|name| !config.model_profiles.contains_key(name))
        .expect("an unused request profile name");
    config.model_profiles.insert(name.clone(), profile);
    config.model_profile = Some(name);
    Ok(config)
}

pub fn apply_thinking(mut config: Config, level: CliThinking) -> Result<Config> {
    let Some(effort) = level.reasoning_effort() else {
        // `auto` is stored, not applied: each user turn's effort is judged from its prompt.
        config.thinking_auto = true;
        return Ok(config);
    };
    config.thinking_auto = false;
    let mut profile = {
        let selection = config.selection();
        if level == CliThinking::Off
            && config.active_model_profile().is_none()
            && matches!(selection.provider.as_str(), "anthropic" | "openrouter")
        {
            // These protocols have a documented disabling control. This is a request-only
            // profile, not an inferred capability for enabling thinking on an unknown model.
            let mut profile = ModelProfile::minimal(&selection.provider, &selection.model);
            profile.thinking = Some(false);
            profile.capabilities.reasoning = if selection.provider == "anthropic" {
                ReasoningApi::AnthropicBudget
            } else {
                profile.capabilities.reasoning_efforts = vec![ReasoningEffort::None];
                ReasoningApi::OpenaiEffort
            };
            return install_thinking_profile(config, profile);
        }
        let active = config.active_model_profile().ok_or_else(|| {
            anyhow!(
                "--thinking {} needs a [model_profiles] entry for {}/{} that declares reasoning_efforts; Dial applies reasoning effort only through model profiles",
                level.name(),
                selection.provider,
                selection.model
            )
        })?;
        active.clone()
    };
    if level == CliThinking::Off
        && matches!(
            profile.capabilities.reasoning,
            ReasoningApi::AnthropicBudget | ReasoningApi::AnthropicAdaptive
        )
    {
        // Anthropic disables thinking with `thinking.type`, never an unsupported `none` effort.
        profile.thinking = Some(false);
        profile.reasoning_effort = None;
    } else {
        profile.reasoning_effort = Some(effort);
    }
    // A fresh process-only entry also lets deep-merged reloads clear optional controls
    // (such as a saved Anthropic effort) without mutating the operator's named profile.
    install_thinking_profile(config, profile)
}

/// The reasoning profile for the selected model: the active model profile, a ChatGPT account
/// profile, or one built from the bundled catalog's thinking metadata. `None` when nothing
/// declares the model's reasoning controls.
pub fn reasoning_profile(config: &Config) -> Result<Option<ModelProfile>> {
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
    use crate::llm::catalog::ThinkingMode;
    let reasoning = match mode {
        ThinkingMode::Effort => {
            if selection.protocol == Some(crate::llm::model_config::ApiProtocol::Responses) {
                ReasoningApi::OpenaiResponses
            } else {
                ReasoningApi::OpenaiEffort
            }
        }
        ThinkingMode::Budget => match selection.provider.as_str() {
            "anthropic" => ReasoningApi::AnthropicBudget,
            "gemini" => ReasoningApi::GeminiBudget,
            _ => ReasoningApi::OpenaiEffort,
        },
        ThinkingMode::AnthropicAdaptive => ReasoningApi::AnthropicAdaptive,
        ThinkingMode::AnthropicBudgetEffort => ReasoningApi::AnthropicBudget,
        ThinkingMode::GoogleLevel => ReasoningApi::GeminiLevel,
    };
    Ok(Some(ModelProfile {
        provider: selection.provider,
        model: selection.model,
        capabilities: crate::llm::profiles::ModelCapabilities {
            reasoning,
            reasoning_efforts: model.thinking_efforts,
            reasoning_effort_wire: model.effort_wire,
            ..Default::default()
        },
        reasoning_effort: model.default_effort,
        thinking: Some(true),
        ..Default::default()
    }))
}

/// Applies a model role's saved `thinking` level (`model_roles.<role>.thinking`) to the role's
/// resolved configuration. Unlike `--thinking`, the catalog's reasoning metadata is enough: a role
/// model needs no hand-written profile. `auto` is judged per user turn and is not a role level.
pub fn apply_role_thinking(mut config: Config, level: &str) -> Result<Config> {
    let level = parse_thinking(level)
        .map_err(|error| anyhow!(error.replace("--thinking", "role thinking")))?;
    anyhow::ensure!(
        level != CliThinking::Auto,
        "role thinking must be a fixed level; auto applies only to the main model"
    );
    if config.active_model_profile().is_none()
        && let Some(profile) = reasoning_profile(&config)?
    {
        config = install_thinking_profile(config, profile)?;
    }
    let mut config = apply_thinking(config, level)?;
    config.thinking_auto = false;
    Ok(config)
}
