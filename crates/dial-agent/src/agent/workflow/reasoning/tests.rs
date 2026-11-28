use super::*;

#[test]
fn loading_and_prefill_do_not_consume_the_reasoning_deadline() {
    let config = ReasoningGuardConfig {
        max_pre_action_secs: 60,
        ..Default::default()
    };
    let mut gate = Gate::new();
    gate.progress(llm::InferencePhase::Loading);
    gate.started -= Duration::from_secs(120);
    assert!(
        gate.check(&config),
        "loading uses the server startup timeout"
    );
    gate.progress(llm::InferencePhase::Processing);
    assert!(gate.check(&config), "prefill is not reasoning");
    gate.progress(llm::InferencePhase::Generating);
    assert!(gate.check(&config));
    gate.started -= Duration::from_secs(61);
    // Repeated progress cannot restart the reasoning budget or pause it again.
    gate.progress(llm::InferencePhase::Processing);
    gate.progress(llm::InferencePhase::Generating);
    assert!(!gate.check(&config));
}

#[test]
fn reasoning_never_resets_the_deadline_and_empty_deltas_do_not_disarm_it() {
    let config = ReasoningGuardConfig {
        max_pre_action_secs: 60,
        ..Default::default()
    };
    let mut gate = Gate::new();
    assert!(gate.output(&config, false));
    gate.started -= Duration::from_secs(61);
    assert!(!gate.check(&config));
    assert!(
        !gate.output(&config, true),
        "late arguments cannot bypass the gate"
    );
}

#[test]
fn answer_text_disarms_only_deadline_not_reasoning_byte_budget() {
    let config = ReasoningGuardConfig {
        max_pre_action_secs: 60,
        max_reasoning_bytes: 16384,
        ..Default::default()
    };
    let mut gate = Gate::new();
    assert!(gate.output(&config, true));
    gate.started -= Duration::from_secs(61);
    assert!(gate.check(&config), "long real answers remain allowed");
    gate.reasoning_bytes = config.max_reasoning_bytes;
    assert!(
        !gate.check(&config),
        "a preamble cannot disable the reasoning budget"
    );
    let mut gate = Gate::new();
    gate.reasoning_bytes = config.max_reasoning_bytes;
    assert!(!gate.check(&config));
    // Disabling the gate drops the time deadline only; the byte cap stays a hard ceiling.
    let off = ReasoningGuardConfig {
        enabled: false,
        ..config
    };
    let mut gate = Gate::new();
    gate.started -= Duration::from_secs(3600);
    assert!(gate.check(&off), "no time deadline when disabled");
    gate.reasoning_bytes = off.max_reasoning_bytes - 1;
    assert!(gate.check(&off));
    gate.reasoning_bytes = off.max_reasoning_bytes;
    assert!(!gate.check(&off), "unbounded thinking is never allowed");
    assert!(!gate.check(&off), "a hit is sticky");
}

#[test]
fn zero_reasoning_limits_do_not_stop_generation() {
    let config = ReasoningGuardConfig {
        max_pre_action_secs: 0,
        max_reasoning_bytes: 0,
        ..ReasoningGuardConfig::default()
    };
    let mut gate = Gate::new();
    gate.started -= Duration::from_secs(3600);
    gate.reasoning_bytes = usize::MAX;
    assert!(gate.check(&config));
    let limited = ReasoningGuardConfig {
        max_reasoning_bytes: 1,
        ..config
    };
    assert!(!gate.check(&limited));
}

#[test]
fn reasoning_recovery_is_bounded_across_causes() {
    let mut recovery = Recovery::default();
    assert!(
        recovery
            .intercept("time limit")
            .unwrap()
            .contains("Take the next small")
    );
    assert!(
        recovery
            .intercept("token limit")
            .unwrap_err()
            .contains("one recovery attempt")
    );
}

#[test]
fn reasoning_recovery_resets_after_progress_preserves_digest_and_stays_bounded() {
    let mut recovery = Recovery::default();
    let first = recovery
        .intercept_with_digest(
            "stream repeated reasoning",
            Some("The regex alternates unparenthesized branches in Cargo.lock."),
        )
        .unwrap();
    assert!(first.contains("Earlier reasoning before cutoff"));
    assert!(first.contains("unparenthesized branches"));

    // Without forward progress, a consecutive hit still stops cleanly.
    assert!(recovery.intercept("second consecutive hit").is_err());

    // Forward progress re-arms recovery for later steps, up to MAX_RECOVERIES_PER_TURN.
    for step in 2..=MAX_RECOVERIES_PER_TURN {
        recovery.note_progress();
        assert!(
            recovery.intercept(&format!("later hit {step}")).is_ok(),
            "hit {step} after progress should recover"
        );
    }
    // Once the per-turn total cap is reached, even progress cannot allow infinite loops.
    recovery.note_progress();
    assert!(recovery.intercept("past turn cap").is_err());
}

#[test]
fn gate_captures_compact_sentence_bounded_prefix_digest() {
    let mut gate = Gate::new();
    gate.record_prefix("First, inspect Cargo.lock to find which workspace crates share version 0.4.2. Second, anchor the replacement regex on package header boundaries so third-party crates like getrandom are never matched. ");
    gate.record_prefix(&"Repeating filler sentence over and over. ".repeat(30));
    let digest = gate.prefix_digest().expect("digest captured");
    assert!(digest.len() <= PREFIX_MAX_BYTES);
    assert!(digest.contains("First, inspect Cargo.lock"));
    assert!(digest.ends_with('.'));
}
