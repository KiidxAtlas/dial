use super::*;

#[test]
fn provider_backoff_is_not_a_silent_stall() {
    let idle = Duration::from_secs(150);
    // Genuine remote silence past the limit is still bounded.
    assert!(silence_exceeded(120, false, false, false, idle));
    // A retry delay longer than the limit is an explicit wait, not silence.
    assert!(!silence_exceeded(120, false, false, true, idle));
    // Local prefill stays unarmed until generation is observed.
    assert!(!silence_exceeded(120, true, false, false, idle));
    assert!(silence_exceeded(120, true, true, false, idle));
    // Disabled limit, and idle time below the limit.
    assert!(!silence_exceeded(0, false, true, false, idle));
    assert!(!silence_exceeded(
        120,
        false,
        true,
        false,
        Duration::from_secs(119)
    ));
}

#[test]
fn telemetry_requires_advancing_phase_or_token_counts() {
    let mut watch = TelemetryProgress::default();
    let mut value = llm::ModelProgress::new(llm::InferencePhase::Generating);
    value.output_tokens = Some(3);
    assert!(watch.observe(&value));
    assert!(!watch.observe(&value));
    value.tokens_per_second = Some(0.5);
    value.eta_secs = Some(12.0);
    assert!(!watch.observe(&value));
    value.output_tokens = None;
    assert!(!watch.observe(&value));
    value.output_tokens = Some(2);
    assert!(!watch.observe(&value));
    value.output_tokens = Some(4);
    assert!(watch.observe(&value));
    value.phase = llm::InferencePhase::Processing;
    assert!(!watch.observe(&value));
    value.phase = llm::InferencePhase::Generating;
    assert!(!watch.observe(&value));
    // A new attempt owns fresh marks, so its first report is progress again.
    assert!(TelemetryProgress::default().observe(&value));
}
