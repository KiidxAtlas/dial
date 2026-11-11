use crate::config::adaptive::Phase;

use super::*;

#[test]
fn experimental_output_effort_raises_on_failure_but_never_overrides_an_explicit_pin() {
    let mut config = config();
    config.provider = Some("anthropic".into());
    config.experimental.adaptive_output_effort = true;
    let mut profile = ModelProfile::minimal("anthropic", "test");
    profile.capabilities.reasoning = ReasoningApi::AnthropicAdaptive;
    profile.capabilities.reasoning_efforts = vec![ReasoningEffort::Low, ReasoningEffort::High];
    assert_eq!(
        experimental_output_effort(&config, &profile, OutputEffortSignal::Routine),
        Some(ReasoningEffort::Low)
    );
    assert_eq!(
        experimental_output_effort(&config, &profile, OutputEffortSignal::CheckFailure),
        Some(ReasoningEffort::High)
    );
    profile.reasoning_effort = Some(ReasoningEffort::Low);
    config
        .model_profiles
        .insert("fixed".into(), profile.clone());
    config.model_profile = Some("fixed".into());
    assert!(
        config.active_model_profile().is_some(),
        "the pinned fixture must match the selected route"
    );
    assert_eq!(
        experimental_output_effort(&config, &profile, OutputEffortSignal::RewritePressure),
        None
    );
}
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
fn local_request_settings_reports_overrides_and_delegation_without_template_data() {
    let request = crate::llm::Chat {
        messages: vec![],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 4096,
        seed: None,
    };
    let mut profile = ModelProfile::minimal("mlx", "test");
    profile.temperature = Some(0.0);
    profile.top_p = Some(0.95);
    profile.local_sampling = Some(crate::llm::local_sampling::LocalSampling {
        top_k: Some(20),
        min_p: Some(0.05),
        chat_template_kwargs: Some(
            serde_json::from_value(json!({"enable_thinking":true,"private_key":"not-for-ui"}))
                .unwrap(),
        ),
        ..Default::default()
    });
    let settings = local_request_settings(
        &request,
        Some(&profile),
        &json!({"thinking":null,"loop_penalty":false}),
        "model metadata",
    );
    assert_eq!(settings["temperature"], 0.0);
    assert_eq!(settings["top_p"], 0.95);
    assert_eq!(settings["top_k"], 20);
    assert_eq!(settings["min_p"], 0.05);
    assert_eq!(settings["thinking"], true);
    assert_eq!(settings["max_tokens"], 4096);
    assert!(!settings.to_string().contains("not-for-ui"));
    let phase = local_request_settings(
        &request,
        None,
        &json!({"thinking":false,"loop_penalty":true}),
        "harness",
    );
    assert_eq!(phase["temperature"], 0.2);
    assert_eq!(phase["thinking"], false);
    assert_eq!(phase["loop_penalty"], true);
    profile.temperature = None;
    profile.capabilities.temperature = false;
    profile.inherit_output_limit = true;
    let delegated = local_request_settings(
        &request,
        Some(&profile),
        &json!({}),
        "local server defaults",
    );
    assert!(delegated["temperature"].is_null());
    assert!(delegated["max_tokens"].is_null());
    assert_eq!(
        render_request_settings(&delegated, true).unwrap(),
        "think on",
        "pure delegation stays off the live label"
    );
    // The server's reported values are shown for the fields Dial omits; sent values stay Dial's.
    let reported = local_request_settings(
        &request,
        Some(&profile),
        &json!({"server_defaults": {"temperature": 0.6, "top_p": 0.8, "top_k": 20,
            "min_p": null, "max_tokens": "unlimited"}}),
        "local server defaults",
    );
    assert_eq!(
        render_request_settings(&reported, true).unwrap(),
        "temp 0.6 (provider) · think on · out unlimited (provider)"
    );
    let detail = render_request_settings(&reported, false).unwrap();
    assert!(
        detail.contains("Top-p 0.95 · top-k 20 · min-p 0.05"),
        "profile values are sent, so they win: {detail}"
    );
    let mut bare = profile.clone();
    bare.top_p = None;
    bare.local_sampling = None;
    let reported = local_request_settings(
        &request,
        Some(&bare),
        &json!({"server_defaults": {"top_p": 0.8, "top_k": 20, "max_tokens": 2048}}),
        "local server defaults",
    );
    let detail = render_request_settings(&reported, false).unwrap();
    assert!(
        detail.starts_with("temp provider · think model default · out 2048 (provider)\nTop-p 0.8 (provider) · top-k 20 (provider) · min-p unset"),
        "{detail}"
    );
    // An explicit profile allowance turns inheritance off on the wire, so the sent value is shown.
    profile.max_tokens = Some(256);
    let capped = local_request_settings(&request, Some(&profile), &json!({}), "local");
    assert_eq!(capped["max_tokens"], 4096);
    assert!(render_request_settings(&Value::Null, true).is_none());
}

#[test]
fn request_settings_are_labelled_as_dial_requests_with_unconfirmed_enforcement() {
    let request = crate::llm::Chat {
        messages: vec![],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 4096,
        seed: None,
    };
    let settings = local_request_settings(
        &request,
        None,
        &json!({}),
        "unchanged; sampling capability unverified or unsupported",
    );
    let detail = render_request_settings(&settings, false).unwrap();
    assert!(detail.contains("Dial-side requested controls, not confirmed backend behavior"));
    assert!(detail.contains("Unset fields may be provider defaults, not confirmed unsent"));
    assert!(detail.contains(
        "Sampling selection: unchanged; sampling capability unverified or unsupported; server enforcement unconfirmed"
    ));
    assert!(
        render_request_settings(&settings, true)
            .unwrap()
            .starts_with("temp 0.2 ·")
    );

    let mut unknown = settings.clone();
    unknown.as_object_mut().unwrap().remove("sampling_source");
    assert!(
        render_request_settings(&unknown, false)
            .unwrap()
            .contains("Sampling selection: unconfirmed; server enforcement unconfirmed")
    );

    let value = json!({
        "config": {"enabled": true, "sampling": false, "pin_sampling": false},
        "last_request": {
            "result": "ok",
            "selection": {
                "task": "editing",
                "phase": "editing",
                "reason": "r",
                "temperature": null,
                "top_p": null,
                "max_tokens": 4096,
                "pre_action_secs": 40,
                "sampling_source": "unchanged; sampling capability unverified or unsupported",
                "reasoning_effort": null,
                "reasoning_source": "provider default or explicit profile",
                "effective": settings,
            }
        }
    });
    let text = render(&value);
    assert!(text.contains("Local request settings (Dial-side requested; backend not confirmed):"));
    assert!(text.contains("Temperature unverified/provider-controlled"));
    assert!(text.contains(
        "Sampling selection: unchanged; sampling capability unverified or unsupported (server enforcement unconfirmed)"
    ));
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
fn successful_shell_work_resumes_the_phase_before_repeated_failures() {
    for (kind, objective, work, expected) in [
        (Kind::General, "Answer a question", None, Phase::General),
        (
            Kind::Editing,
            "Compare approaches",
            None,
            Phase::Exploration,
        ),
        (
            Kind::Editing,
            "Implement change",
            None,
            Phase::Investigation,
        ),
        (
            Kind::Editing,
            "Implement change",
            Some(("edit", false)),
            Phase::Editing,
        ),
        (
            Kind::Editing,
            "Implement change",
            Some(("bash", true)),
            Phase::Verification,
        ),
    ] {
        let mut state = State::begin(kind, objective);
        if let Some((name, check)) = work {
            state.observe(name, true, check);
        }
        for _ in 0..2 {
            state.observe("bash", false, false);
            assert_eq!(state.phase, Phase::Recovery, "{expected:?}");
        }
        for name in ["todo", "working_state", "tool_load"] {
            state.observe(name, true, false);
            assert_eq!(state.phase, Phase::Recovery, "{name} is bookkeeping");
        }
        state.observe("bash", true, false);
        assert_eq!(state.phase, expected, "{kind:?}: {objective}");
    }
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

#[test]
fn live_label_is_empty_when_everything_is_delegated_and_diffs_name_only_controls() {
    let delegated = json!({"temperature": null, "max_tokens": null, "thinking": null});
    assert!(render_request_settings(&delegated, true).is_none());
    let before = json!({"temperature": 0.6, "thinking": true, "max_tokens": 4096,
        "max_reasoning_bytes": 13107, "provider_defaults": {"top_k": 20}});
    let mut after = before.clone();
    after["max_reasoning_bytes"] = json!(26214);
    after["provider_defaults"] = json!({"top_k": 40});
    assert_eq!(
        request_settings_diff(&before, &after),
        None,
        "derived values are not controls"
    );
    after["thinking"] = json!(false);
    after["temperature"] = json!(0.2);
    assert_eq!(
        request_settings_diff(&before, &after).as_deref(),
        Some("temp 0.6 → 0.2 · thinking on → off")
    );
    after["max_tokens"] = Value::Null;
    assert!(
        request_settings_diff(&before, &after)
            .unwrap()
            .ends_with("output limit 4096 → default")
    );
}
