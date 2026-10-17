use crate::config::adaptive::Phase;

use super::*;
fn config() -> AppConfig {
    AppConfig {
        model: "test".into(),
        ..Default::default()
    }
}
fn choose(
    c: &AppConfig,
    s: &State,
    p: Option<&ModelProfile>,
    recover: bool,
    hermetic: bool,
) -> Selection {
    select(
        c,
        s,
        Request {
            kind: Kind::Editing,
            temperature: 0.4,
            tokens: 8192,
            pre_action_secs: 60,
            base_profile: p,
            recovering_output: recover,
            hermetic,
        },
    )
}
#[test]
fn task_phase_follows_observed_work_and_recovery_is_not_a_quality_label() {
    let mut s = State::begin(Kind::Editing, "Implement change");
    assert_eq!(s.phase, Phase::Investigation);
    s.observe("write", true, false);
    assert_eq!(s.phase, Phase::Editing);
    s.observe("bash", true, true);
    assert_eq!(s.phase, Phase::Verification);
    s.observe("read", false, false);
    assert_eq!(s.phase, Phase::Recovery);
    s.observe("todo", true, false);
    assert_eq!(s.phase, Phase::Recovery);
    s.observe("read", true, false);
    assert_eq!(s.phase, Phase::Editing);
}
#[test]
fn default_output_budget_is_not_reduced_by_phase() {
    let c = config();
    assert!(!c.adaptive.output_limits);
    for phase in [
        Phase::Investigation,
        Phase::Editing,
        Phase::Verification,
        Phase::Recovery,
        Phase::Exploration,
    ] {
        let mut s = State::begin(Kind::Editing, "Implement change");
        s.phase = phase;
        assert_eq!(choose(&c, &s, None, false, false).max_tokens, 8192);
    }
}
#[test]
fn budgets_respect_operator_limits_and_length_recovery_escapes_phase_cap() {
    let mut c = config();
    c.adaptive.output_limits = true;
    let mut s = State::begin(Kind::Investigation, "Locate symbol");
    let first = choose(&c, &s, None, false, false);
    assert_eq!(first.max_tokens, 2048);
    assert_eq!(first.temperature, Some(0.4));
    let smaller = select(
        &c,
        &s,
        Request {
            kind: Kind::Editing,
            temperature: 0.4,
            tokens: 512,
            pre_action_secs: 3,
            base_profile: None,
            recovering_output: false,
            hermetic: false,
        },
    );
    assert_eq!(smaller.max_tokens, 512);
    assert_eq!(smaller.pre_action_secs, 3);
    s.recover("truncated");
    assert_eq!(choose(&c, &s, None, true, false).max_tokens, 8192);
    assert_eq!(choose(&c, &s, None, false, true).max_tokens, 8192);
}
#[test]
fn phase_sampling_is_opt_in_and_pins_and_unknown_capabilities_win() {
    let mut c = config();
    let mut s = State::begin(Kind::Editing, "Change");
    s.observe("write", true, false);
    assert_eq!(choose(&c, &s, None, false, false).temperature, Some(0.4));
    c.adaptive.sampling = true;
    assert_eq!(choose(&c, &s, None, false, false).temperature, Some(0.15));
    c.adaptive.pin_sampling = true;
    assert_eq!(choose(&c, &s, None, false, false).temperature, Some(0.4));
    c.adaptive.pin_sampling = false;
    let profile = ModelProfile {
        provider: "mlx".into(),
        model: "test".into(),
        temperature: Some(0.7),
        top_p: Some(0.99),
        max_tokens: Some(8192),
        capabilities: crate::llm::profiles::ModelCapabilities {
            temperature: true,
            top_p: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let pinned = choose(&c, &s, Some(&profile), false, false);
    assert_eq!(pinned.temperature, Some(0.7));
    assert_eq!(pinned.top_p, Some(0.99));
    assert_eq!(pinned.max_tokens, 8192);
    let unsupported = ModelProfile {
        temperature: None,
        capabilities: Default::default(),
        ..profile
    };
    assert!(
        choose(&c, &s, Some(&unsupported), false, false)
            .temperature
            .is_none()
    );
}
#[test]
fn reasoning_only_uses_declared_values_and_never_clips_thinking_budget() {
    let c = config();
    let mut s = State::begin(Kind::Editing, "Implement");
    s.observe("write", true, false);
    let mut p = ModelProfile {
        provider: "test".into(),
        model: "test".into(),
        capabilities: crate::llm::profiles::ModelCapabilities {
            reasoning: ReasoningApi::OpenaiEffort,
            reasoning_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
            ..Default::default()
        },
        ..Default::default()
    };
    let chosen = choose(&c, &s, Some(&p), false, false);
    assert_eq!(chosen.reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(chosen.max_tokens, 8192);
    p.reasoning_effort = Some(ReasoningEffort::High);
    assert_eq!(
        choose(&c, &s, Some(&p), false, false).reasoning_effort,
        Some(ReasoningEffort::High)
    );
    p.capabilities.reasoning = ReasoningApi::AnthropicBudget;
    p.capabilities.reasoning_efforts.clear();
    p.reasoning_effort = None;
    p.thinking = Some(true);
    p.thinking_budget = Some(4096);
    let chosen = choose(&c, &s, Some(&p), false, false);
    assert_eq!(chosen.max_tokens, 8192);
    assert!(chosen.temperature.is_none());
    assert!(chosen.profile.unwrap().thinking_budget == Some(4096));
}
#[test]
fn phase_pin_and_invalid_configuration_are_bounded() {
    let mut c = config();
    c.adaptive.phase = Phase::Verification;
    let s = State::begin(Kind::General, "hello");
    assert_eq!(
        choose(&c, &s, None, false, false).phase,
        Phase::Verification
    );
    c.adaptive.verification_tokens = 0;
    assert!(c.adaptive.validate().is_err());
    c.adaptive.verification_tokens = 1536;
    c.adaptive.editing_temperature = f64::INFINITY;
    assert!(c.adaptive.validate().is_err());
}
