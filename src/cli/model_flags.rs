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

use crate::config::types::Config;
use crate::llm::model_config::ModelSelection;
use crate::llm::profiles::{ModelProfile, ReasoningApi, ReasoningEffort};
use anyhow::{Result, anyhow};
use std::fmt;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_accepts_selectors_and_unambiguous_prefixes() {
        assert_eq!(parse_thinking("off"), Ok(CliThinking::Off));
        assert_eq!(parse_thinking("minimal"), Ok(CliThinking::Minimal));
        assert_eq!(parse_thinking("xhigh"), Ok(CliThinking::XHigh));
        assert_eq!(parse_thinking("auto"), Ok(CliThinking::Auto));
        assert_eq!(parse_thinking("med"), Ok(CliThinking::Medium));
        assert_eq!(parse_thinking("ma"), Ok(CliThinking::Max));
        assert_eq!(parse_thinking("xh"), Ok(CliThinking::XHigh));
    }

    #[test]
    fn thinking_rejects_unknown_short_and_case_variants() {
        assert!(parse_thinking("").is_err());
        assert!(parse_thinking("m").is_err());
        assert!(parse_thinking("galaxy").is_err());
        assert!(parse_thinking("HIGH").is_err());
        assert!(parse_thinking("inherit").is_err());
    }

    #[test]
    fn thinking_maps_to_reasoning_effort() {
        assert_eq!(
            CliThinking::Off.reasoning_effort(),
            Some(ReasoningEffort::None)
        );
        assert_eq!(
            CliThinking::XHigh.reasoning_effort(),
            Some(ReasoningEffort::Xhigh)
        );
        assert_eq!(CliThinking::Auto.reasoning_effort(), None);
    }

    #[test]
    fn models_split_and_trim() {
        assert_eq!(
            parse_models(" a/b , c ,d").unwrap(),
            ModelPatterns(vec!["a/b".into(), "c".into(), "d".into()])
        );
        assert!(parse_models("").is_err());
        assert!(parse_models("a,,b").is_err());
        assert!(parse_models("a, ").is_err());
    }

    #[test]
    fn provider_charset() {
        assert_eq!(parse_provider("mlx").unwrap(), "mlx");
        assert_eq!(parse_provider("open-ai.v1_x").unwrap(), "open-ai.v1_x");
        assert!(parse_provider("").is_err());
        assert!(parse_provider("bad name").is_err());
        assert!(parse_provider("a/b").is_err());
    }

    #[test]
    fn api_key_rejects_empty_whitespace_and_control() {
        assert!(parse_api_key("sk-abc").is_ok());
        assert!(parse_api_key("").is_err());
        assert!(parse_api_key("sk abc").is_err());
        assert!(parse_api_key("sk\u{7}").is_err());
        assert_eq!(
            format!("{:?}", parse_api_key("sk-secret").unwrap()),
            "ApiKey(<redacted>)"
        );
    }

    #[test]
    fn role_model_trims_and_bounds() {
        assert_eq!(parse_role_model("  qwen-small ").unwrap(), "qwen-small");
        assert!(parse_role_model("   ").is_err());
        assert!(parse_role_model(&"m".repeat(513)).is_err());
        assert!(parse_role_model("a\nb").is_err());
    }

    #[test]
    fn service_tier_matches_openai_set() {
        assert_eq!(parse_service_tier("priority"), Ok(ServiceTier::Priority));
        assert_eq!(parse_service_tier("ultrafast"), Ok(ServiceTier::Ultrafast));
        assert_eq!(parse_service_tier("none"), Ok(ServiceTier::None));
        assert!(parse_service_tier("turbo").is_err());
        assert!(parse_service_tier("Priority").is_err());
    }

    #[test]
    fn max_time_accepts_seconds_and_units() {
        assert_eq!(parse_max_time("30"), Ok(30.0));
        assert_eq!(parse_max_time("1.5"), Ok(1.5));
        assert_eq!(parse_max_time("5s"), Ok(5.0));
        assert_eq!(parse_max_time("10m"), Ok(600.0));
        assert_eq!(parse_max_time("1h"), Ok(3600.0));
        assert_eq!(parse_max_time("2.5m"), Ok(150.0));
        assert_eq!(parse_max_time(" 5s "), Ok(5.0));
    }

    #[test]
    fn max_time_rejects_nonpositive_and_malformed() {
        for bad in [
            "",
            "0",
            "0s",
            "-5",
            "5x",
            "5S",
            ".5s",
            "1e3s",
            "abc",
            "inf",
            "NaN",
            "1h2m",
            "100000000000000000000",
            "2000000000h",
        ] {
            assert!(parse_max_time(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn api_key_with_invalid_token_fails_before_any_override() {
        let flags = ModelFlags {
            api_key: Some(ApiKey("bad key with spaces".into())),
            ..Default::default()
        };
        assert!(apply(Config::default(), &flags).is_err());
    }

    #[test]
    fn max_time_becomes_a_future_deadline() {
        let flags = ModelFlags {
            max_time: Some(60.0),
            ..Default::default()
        };
        let deadline = flags.deadline().expect("deadline set");
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            remaining > std::time::Duration::from_secs(50),
            "{remaining:?}"
        );
        assert!(ModelFlags::default().deadline().is_none());
        assert!(apply(Config::default(), &flags).is_ok());
    }

    #[test]
    fn service_tier_flag_sets_config_tier() {
        let flags = ModelFlags {
            service_tier: Some(ServiceTier::Priority),
            ..Default::default()
        };
        let config = apply(Config::default(), &flags).expect("service tier applies");
        assert_eq!(config.service_tier, "priority");
    }

    #[test]
    fn thinking_without_declared_profile_is_an_error() {
        let flags = ModelFlags {
            thinking: Some(CliThinking::High),
            ..Default::default()
        };
        let err = apply(Config::default(), &flags).unwrap_err().to_string();
        assert!(err.contains("model_profiles"), "{err}");
    }

    #[test]
    fn openrouter_thinking_off_reaches_request_without_manual_profile() {
        let config = Config {
            provider: Some("openrouter".into()),
            model: "anthropic/claude-haiku-5.5".into(),
            ..Default::default()
        };
        let original = config.clone();
        let configured = apply_thinking(config, CliThinking::Off).unwrap();
        let mut wire = serde_json::json!({"model":configured.model,"temperature":0.2});
        configured
            .active_model_profile()
            .unwrap()
            .apply_openai(&mut wire);
        assert_eq!(wire["reasoning_effort"], "none");
        assert!(original.model_profiles.is_empty());
        assert!(apply_thinking(original, CliThinking::High).is_err());
    }

    #[test]
    fn anthropic_thinking_off_uses_disabled_thinking_not_none_effort() {
        let config = Config {
            provider: Some("anthropic".into()),
            model: "claude-sonnet-4-6".into(),
            ..Default::default()
        };
        let configured = apply_thinking(config, CliThinking::Off).unwrap();
        let profile = configured.active_model_profile().unwrap();
        assert_eq!(profile.thinking, Some(false));
        assert_eq!(profile.reasoning_effort, None);
        // The second application exercises the existing-profile branch used by model switching.
        let configured = apply_thinking(configured, CliThinking::Off).unwrap();
        assert_eq!(
            configured.active_model_profile().unwrap().thinking,
            Some(false)
        );
    }

    #[test]
    fn disk_reload_keeps_launch_selection_and_thinking_without_freezing_other_settings() {
        use crate::storage::{paths::Layout, settings_store::SettingsReload};
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
        let path = home.path().join("config.toml");
        std::fs::write(&path, "model = 'anthropic/claude-haiku-5.5'\nprovider = 'openrouter'\n[harness]\nmax_tokens = 8192\n").unwrap();
        let before = Config::load(&layout, None).unwrap();
        let selected = apply(
            before.clone(),
            &ModelFlags {
                thinking: Some(CliThinking::Off),
                service_tier: Some(ServiceTier::Priority),
                ..Default::default()
            },
        )
        .unwrap();
        let mut overlay = toml::Table::new();
        capture_overlay(&mut overlay, &before, &selected).unwrap();
        let mut reload = SettingsReload::new(&layout, &overlay, &selected);
        let (same, errors) = reload.reload(&layout, &overlay, None);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(same.service_tier, "priority");
        let mut wire = serde_json::json!({});
        same.active_model_profile().unwrap().apply_openai(&mut wire);
        assert_eq!(wire["reasoning_effort"], "none");
        std::fs::write(&path, "model = 'anthropic/claude-haiku-5.5'\nprovider = 'openrouter'\n[harness]\nmax_tokens = 8192\ntemperature = 0.4\n").unwrap();
        let (changed, errors) = reload.reload(&layout, &overlay, None);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(changed.harness.temperature, 0.4);
        assert_eq!(changed.service_tier, "priority");
        assert_eq!(
            changed.active_model_profile().unwrap().thinking,
            Some(false)
        );
        assert!(
            !std::fs::read_to_string(path)
                .unwrap()
                .contains("__cli_thinking")
        );
    }

    #[test]
    fn thinking_auto_is_stored_without_a_profile() {
        let flags = ModelFlags {
            thinking: Some(CliThinking::Auto),
            ..Default::default()
        };
        let auto = apply(Config::default(), &flags).unwrap();
        assert!(auto.thinking_auto);
        assert!(auto.model_profile.is_none());
    }
}
