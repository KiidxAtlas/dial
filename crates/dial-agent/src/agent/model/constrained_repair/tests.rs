use super::*;
use crate::config::types::Config;

fn dropped() -> Reply {
    Reply {
        finish_reason: "tool_calls".into(),
        ..Default::default()
    }
}

#[test]
fn enabled_follows_provider_default_and_explicit_override() {
    let config = Config::default().assistance;
    let lm = ModelProfile::minimal("lm-studio", "m");
    let mlx = ModelProfile::minimal("mlx", "m");
    assert!(enabled(&config, false, "lm-studio", "m", Some(&lm), None));
    assert!(enabled(&config, false, "ollama", "m", None, None));
    assert!(!enabled(&config, false, "mlx", "m", Some(&mlx), None));
    assert!(!enabled(&config, true, "lm-studio", "m", Some(&lm), None));
    let mut off = ModelProfile::minimal("lm-studio", "m");
    off.capabilities.structured_output = Some(StructuredOutput::Disabled);
    assert!(!enabled(&config, false, "lm-studio", "m", Some(&off), None));
    let mut disabled = config.clone();
    disabled.constrained_repair = false;
    assert!(!enabled(
        &disabled,
        false,
        "lm-studio",
        "m",
        Some(&lm),
        None
    ));
}

#[test]
fn detected_llama_server_enables_repair_with_or_without_a_profile() {
    let config = Config::default().assistance;
    let json = Some(StructuredOutput::JsonSchema);
    // A generic OpenAI-compatible endpoint declares no mechanism; the detected one decides.
    assert!(!enabled(
        &config,
        false,
        "openai-compatible",
        "m",
        None,
        None
    ));
    assert!(enabled(
        &config,
        false,
        "openai-compatible",
        "m",
        None,
        json
    ));
    let plain = ModelProfile::minimal("openai-compatible", "m");
    assert!(enabled(
        &config,
        false,
        "openai-compatible",
        "m",
        Some(&plain),
        json
    ));
    // A detection already recorded on the profile (set at the request boundary) counts the same.
    let mut recorded = plain.clone();
    recorded.endpoint_structured_output = json;
    assert!(enabled(
        &config,
        false,
        "openai-compatible",
        "m",
        Some(&recorded),
        None
    ));
    // An explicit declaration still wins over detection.
    let mut declared_off = recorded.clone();
    declared_off.capabilities.structured_output = Some(StructuredOutput::Disabled);
    assert!(!enabled(
        &config,
        false,
        "openai-compatible",
        "m",
        Some(&declared_off),
        json
    ));
}

#[test]
fn at_most_one_retry_per_malformed_sequence() {
    let mut repair = Repair::default();
    assert_eq!(
        repair.trigger(true, &dropped(), &[], &[]),
        Some("dropped_call")
    );
    assert_eq!(repair.trigger(false, &dropped(), &[], &[]), None);
    repair.request();
    assert!(repair.take_request());
    assert!(!repair.take_request());
    assert_eq!(repair.trigger(true, &dropped(), &[], &[]), None);
    repair.settle();
    assert_eq!(
        repair.trigger(true, &dropped(), &[], &[]),
        Some("dropped_call")
    );
}

#[test]
fn constrained_reply_converts_and_plain_text_is_rejected() {
    let offered = vec!["read".to_owned()];
    let mut ok = Reply {
        text: r#"{"name":"read","arguments":{"path":"a"}}"#.into(),
        finish_reason: "stop".into(),
        ..Default::default()
    };
    assert_eq!(finish(&mut ok, &offered, "c1".into()), Ok("converted"));
    assert_eq!(ok.tool_calls.len(), 1);
    assert_eq!(ok.tool_calls[0].name, "read");
    assert!(ok.text.is_empty());

    let mut prose = Reply {
        text: "I will read the file now.".into(),
        finish_reason: "stop".into(),
        ..Default::default()
    };
    assert!(finish(&mut prose, &offered, "c2".into()).is_err());
    assert!(prose.tool_calls.is_empty());
}
