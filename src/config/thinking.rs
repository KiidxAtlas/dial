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

pub(crate) fn install_thinking_profile(
    mut config: Config,
    profile: ModelProfile,
) -> Result<Config> {
    profile.validate()?;
    let name = (0..)
        .map(|i| format!("__cli_thinking_{i}"))
        .find(|name| !config.model_profiles.contains_key(name))
        .expect("an unused request profile name");
    config.model_profiles.insert(name.clone(), profile);
    config.model_profile = Some(name);
    Ok(config)
}

pub(crate) fn apply_thinking(mut config: Config, level: CliThinking) -> Result<Config> {
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
