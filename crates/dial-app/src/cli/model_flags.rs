//! Model/provider CLI flags: `--models`, `--thinking`, `--provider`, `--api-key`, `--smol`,
//! `--slow`, `--service-tier`, `--max-time`.
//!
//! Parsing is pure and happens at argument time (clap `value_parser`). [`apply`] runs after
//! `Config::load` and mutates the loaded [`Config`] in memory only, then validates it with
//! [`Config::validate`]. Nothing is persisted.
//!
//! Per-flag status against Dial:
//! - `--provider <name>`: implemented. Switches the main selection's provider via `with_selection`,
//!   keeping the current model and any explicit context cap.
//! - `--smol <model>`, `--slow <model>`: implemented. Sets `model_roles.small` / `model_roles.coach`
//!   to the main selection (after `--provider`) with the given model. Replaces a configured role
//!   of the same name for this run; no context cap is inherited.
//! - `--thinking <level>`: implemented for models with a matching `[model_profiles.*]` entry that
//!   declares the effort in `capabilities.reasoning_efforts`. `off` maps to Dial's `none` effort.
//!   `off` needs no manual profile on Anthropic or OpenRouter. Other efforts need declared support.
//!   `auto` is stored as `thinking_auto`: each user turn's effort is judged from its prompt, clamped to
//!   the profile's declared efforts, and falls back to the model default when no judge is usable.
//! - `--models <csv>`: implemented by `model_scope` (model-pattern scope). Resolved against
//!   available models before `apply`; sets the starting model (unless `--provider`) and the
//!   `/model next` cycle for this run only. Unmatched patterns error.
//! - `--api-key <key>`: process-only credential for the active provider's origin
//!   (`catalog::set_cli_api_key`). Never persisted; ranks above env and saved keys. Errors when the
//!   key is malformed or the provider has no base URL.
//! - `--service-tier <tier>`: sets `service_tier` (`none` ... `priority`, `flex`, `ultrafast`).
//!   Validated against `config::SERVICE_TIERS` and sent on provider requests that accept it.
//! - `--max-time <duration>`: implemented as a launch-scoped deadline (`deadline`). Once it
//!   passes, the agent stops the turn at the next step (`agent::modes::Launch`).
use crate::config::thinking::CliThinking;
use crate::config::thinking::apply_thinking;
use crate::config::thinking::parse_thinking;

use crate::config::types::Config;
use crate::llm::model_config::ModelSelection;
use anyhow::{Result, anyhow};
use std::fmt;

/// Service-tier values accepted by `--service-tier` (the OpenAI set).
const SERVICE_TIER_VALUES: [(&str, ServiceTier); 7] = [
    ("none", ServiceTier::None),
    ("auto", ServiceTier::Auto),
    ("default", ServiceTier::Default),
    ("flex", ServiceTier::Flex),
    ("scale", ServiceTier::Scale),
    ("priority", ServiceTier::Priority),
    ("ultrafast", ServiceTier::Ultrafast),
];

/// Longest model reference accepted for `--smol` / `--slow`.
const MAX_MODEL_BYTES: usize = 512;
/// Largest `--max-time` accepted, in seconds (about 31 years). Larger values would overflow `Instant`
/// arithmetic in [`ModelFlags::deadline`].
const MAX_MAX_TIME_SECS: f64 = 1_000_000_000.0;

/// `--service-tier` value (the OpenAI set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceTier {
    None,
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
    Ultrafast,
}

/// `--models` patterns in the order given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPatterns(pub Vec<String>);

/// `--api-key` literal secret. Debug output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Model/provider flags, flattened into the top-level CLI by `main.rs`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ModelFlags {
    /// Comma-separated model patterns: scope the `/model next` cycle and pick the starting model
    /// for this run only (not saved). Resolved by `model_scope::apply`.
    #[arg(long, global = true, value_parser = parse_models)]
    pub models: Option<ModelPatterns>,
    /// Thinking level: off, minimal, low, medium, high, xhigh, max or auto.
    #[arg(long, global = true, value_parser = parse_thinking)]
    pub thinking: Option<CliThinking>,
    /// Default provider for the main model.
    #[arg(long, global = true, value_parser = parse_provider)]
    pub provider: Option<String>,
    /// Process-only API key for the active provider's origin; never saved. Prefer --api-key-env.
    #[arg(long, global = true, value_parser = parse_api_key)]
    pub api_key: Option<ApiKey>,
    /// Model for the `small` role: compaction, commit messages, memory, and scout subagents.
    #[arg(long, global = true, value_parser = parse_role_model)]
    pub smol: Option<String>,
    /// Model for the `coach` role: escalation, self-improvement feedback, and advisor review.
    #[arg(long, global = true, value_parser = parse_role_model)]
    pub slow: Option<String>,
    /// OpenAI service tier (sent only to providers that accept it).
    #[arg(long, global = true, value_parser = parse_service_tier)]
    pub service_tier: Option<ServiceTier>,
    /// Whole-run deadline: seconds, or a duration like 5s, 10m, 1h.
    #[arg(long, global = true, value_parser = parse_max_time)]
    pub max_time: Option<f64>,
}

impl ModelFlags {
    /// `--max-time` as a wall-clock deadline measured from now, or `None` when unset.
    pub fn deadline(&self) -> Option<std::time::Instant> {
        self.max_time
            .map(|secs| std::time::Instant::now() + std::time::Duration::from_secs_f64(secs))
    }
}

/// Parses `--models`: comma-separated, each entry trimmed and non-empty.
pub fn parse_models(value: &str) -> std::result::Result<ModelPatterns, String> {
    let mut patterns = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(format!(
                "Invalid --models value: {value:?}. Every comma-separated model pattern must be non-empty."
            ));
        }
        if entry.chars().any(char::is_control) {
            return Err("Invalid --models value: control characters are not allowed.".into());
        }
        patterns.push(entry.to_owned());
    }
    Ok(ModelPatterns(patterns))
}

/// Parses `--provider`: a non-empty provider id of ASCII letters, digits, `.`, `_` or `-`.
pub fn parse_provider(value: &str) -> std::result::Result<String, String> {
    let valid = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(value.to_owned())
    } else {
        Err(format!(
            "Invalid --provider value: {value:?}. Expected a provider id of letters, digits, '.', '_' or '-'."
        ))
    }
}

/// Parses `--api-key`: a non-empty secret without whitespace or control characters.
pub fn parse_api_key(value: &str) -> std::result::Result<ApiKey, String> {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(
            "Invalid --api-key value: expected a non-empty key without whitespace or control characters."
                .into(),
        );
    }
    Ok(ApiKey(value.to_owned()))
}

/// Parses `--smol` / `--slow`: a non-empty model reference of at most 512 bytes.
pub fn parse_role_model(value: &str) -> std::result::Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_MODEL_BYTES {
        return Err(format!(
            "Invalid model value: {value:?}. Expected a non-empty model of at most {MAX_MODEL_BYTES} bytes."
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err("Invalid model value: control characters are not allowed.".into());
    }
    Ok(trimmed.to_owned())
}

/// Parses `--service-tier` against the OpenAI set.
pub fn parse_service_tier(value: &str) -> std::result::Result<ServiceTier, String> {
    SERVICE_TIER_VALUES
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, tier)| *tier)
        .ok_or_else(|| {
            format!(
                "Invalid --service-tier value: {value:?}. Expected one of: {}.",
                SERVICE_TIER_VALUES
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Parses `--max-time` into seconds: a positive number, optionally suffixed `s`, `m` or `h`
/// (suffix form requires digits with an optional fraction, as in `^(\d+(\.\d+)?)([smh])$`).
pub fn parse_max_time(value: &str) -> std::result::Result<f64, String> {
    let trimmed = value.trim();
    let invalid = || {
        format!(
            "Invalid --max-time value: {value:?}. Expected a positive number of seconds or duration like \"5s\", \"10m\", \"1h\"."
        )
    };
    let seconds = match trimmed.chars().last() {
        Some(unit @ ('s' | 'm' | 'h')) => {
            let number = &trimmed[..trimmed.len() - 1];
            if !is_decimal_digits(number) {
                return Err(invalid());
            }
            let multiplier = match unit {
                'h' => 3600.0,
                'm' => 60.0,
                _ => 1.0,
            };
            number.parse::<f64>().map_err(|_| invalid())? * multiplier
        }
        _ => trimmed.parse::<f64>().map_err(|_| invalid())?,
    };
    if seconds.is_finite() && seconds > 0.0 && seconds <= MAX_MAX_TIME_SECS {
        Ok(seconds)
    } else {
        Err(invalid())
    }
}

/// `\d+(\.\d+)?`: digits, optionally one dot followed by digits.
fn is_decimal_digits(text: &str) -> bool {
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(whole) && fraction.is_none_or(digits)
}

/// Applies the flags to a loaded configuration in memory and validates the result.
///
/// `--models` is not applied here: `model_scope::apply` runs first and installs the scope and the
/// starting model. Call after `Config::load`.
pub fn apply(mut config: Config, flags: &ModelFlags) -> Result<Config> {
    if let Some(tier) = flags.service_tier {
        config.service_tier = match tier {
            ServiceTier::None => "none",
            ServiceTier::Auto => "auto",
            ServiceTier::Default => "default",
            ServiceTier::Flex => "flex",
            ServiceTier::Scale => "scale",
            ServiceTier::Priority => "priority",
            ServiceTier::Ultrafast => "ultrafast",
        }
        .to_owned();
    }
    if let Some(provider) = &flags.provider {
        let mut selection = config.selection();
        selection.provider = provider.clone();
        selection.context_window = config.context_limit.tokens();
        config = config.with_selection(&selection);
    }
    if let Some(key) = &flags.api_key {
        let selection = config.selection();
        let base_url = config
            .selection_base_url(&selection)
            .map(std::borrow::Cow::into_owned)
            .ok_or_else(|| {
                anyhow!(
                    "--api-key needs a base URL for provider {}",
                    selection.provider
                )
            })?;
        crate::llm::catalog::set_cli_api_key(&base_url, key.0.clone())?;
    }
    for (role, model) in [("small", &flags.smol), ("coach", &flags.slow)] {
        if let Some(model) = model {
            let mut selection: ModelSelection = config.selection();
            selection.model = model.clone();
            selection.context_window = None;
            config.model_roles.insert(role.to_owned(), selection);
        }
    }
    if let Some(level) = flags.thinking {
        config = apply_thinking(config, level)?;
    }
    config.validate()?;
    Ok(config)
}

/// Retain only launch-time changes in the process overlay used by settings reloads.
/// Serializing the full config into that overlay would freeze unrelated editable settings.
pub fn capture_overlay(overlay: &mut toml::Table, before: &Config, after: &Config) -> Result<()> {
    fn changed(before: &toml::Table, after: toml::Table) -> toml::Table {
        after
            .into_iter()
            .filter_map(|(key, value)| {
                if before.get(&key) == Some(&value) {
                    return None;
                }
                let value = match (before.get(&key), value) {
                    (Some(toml::Value::Table(old)), toml::Value::Table(new)) => {
                        toml::Value::Table(changed(old, new))
                    }
                    (_, value) => value,
                };
                Some((key, value))
            })
            .collect()
    }
    let before = toml::Table::try_from(before)?;
    let after = toml::Table::try_from(after)?;
    crate::config::merge(overlay, changed(&before, after));
    Ok(())
}

#[cfg(test)]
mod tests;
