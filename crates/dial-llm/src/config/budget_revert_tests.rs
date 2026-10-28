use super::*;
use crate::config::types::FallbackRevert;

fn resolve(text: &str) -> Result<Config> {
    let project = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned())?;
    Config::from_table(&layout, toml::from_str(text)?)
}

#[test]
fn warn_thresholds_parse_and_stay_separate_from_the_hard_caps() {
    let config = resolve(
        "provider='chatgpt'\nmodel='m'\n[budget]\nmax_session_cost_usd=5.0\nwarn_session_cost_usd=2.0\nwarn_turn_cost_usd=0.25\n",
    )
    .unwrap();
    assert_eq!(config.budget.max_session_cost_usd, Some(5.0));
    assert_eq!(config.budget.max_turn_cost_usd, Some(0.0));
    assert_eq!(config.budget.warn_session_cost_usd, Some(2.0));
    assert_eq!(config.budget.warn_turn_cost_usd, Some(0.25));
    config.validate().unwrap();
}

#[test]
fn warn_thresholds_reject_negative_or_non_finite_values() {
    let mut config = resolve("provider='chatgpt'\nmodel='m'\n").unwrap();
    config.budget.warn_turn_cost_usd = Some(-1.0);
    assert!(config.validate().is_err());
    config.budget.warn_turn_cost_usd = Some(f64::INFINITY);
    assert!(config.validate().is_err());
    config.budget.warn_turn_cost_usd = Some(0.0);
    assert!(config.validate().is_ok(), "zero disables the warning");
}

#[test]
fn fallback_revert_defaults_to_cooldown_expiry_and_parses_never() {
    let default = resolve("provider='chatgpt'\nmodel='m'\n").unwrap();
    assert_eq!(
        default.retry.fallback_revert,
        FallbackRevert::CooldownExpiry
    );
    let sticky =
        resolve("provider='chatgpt'\nmodel='m'\n[retry]\nfallback_revert='never'\n").unwrap();
    assert_eq!(sticky.retry.fallback_revert, FallbackRevert::Never);
    assert!(
        resolve("provider='chatgpt'\nmodel='m'\n[retry]\nfallback_revert='sometimes'\n").is_err()
    );
}
