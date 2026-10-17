use super::*;
use std::time::Duration;
#[test]
fn live_counts_cover_hidden_output_and_exact_counts_replace_estimates() {
    let now = Instant::now();
    let mut tracker = RequestProgress::new();
    tracker.text("abcdefgh", now);
    tracker.text("ijklmnop", now + Duration::from_secs(1));
    assert_eq!(tracker.value.output_tokens, Some(4));
    assert_eq!(tracker.value.tokens_per_second, Some(2.0));
    assert!(tracker.value.estimated);
    let mut exact = ModelProgress::new(InferencePhase::Generating);
    exact.output_tokens = Some(3);
    tracker.observe(exact.clone(), now + Duration::from_secs(2));
    assert_eq!(tracker.value.output_tokens, Some(3));
    assert!(!tracker.value.estimated);
    tracker.text("tool arguments", now + Duration::from_secs(3));
    assert_eq!(tracker.value.output_tokens, Some(3));
    exact.output_tokens = Some(8);
    exact.tokens_per_second = Some(30.0);
    tracker.observe(exact.clone(), now + Duration::from_secs(4));
    assert_eq!(tracker.value.tokens_per_second, Some(30.0));
    exact.tokens_per_second = None;
    tracker.observe(exact, now + Duration::from_secs(5));
    assert_eq!(
        tracker.value.tokens_per_second,
        Some(30.0),
        "final usage must preserve the engine rate"
    );
    assert_eq!(RequestProgress::new().value.output_tokens, None);
}

#[test]
fn prompt_eta_follows_the_measured_processing_rate() {
    let now = Instant::now();
    let mut tracker = RequestProgress::new();
    let report = |done: u64, cached: Option<u64>| {
        let mut value = ModelProgress::new(InferencePhase::Processing);
        value.prompt_tokens = Some(10_000);
        value.processed_tokens = Some(done);
        value.cached_tokens = cached;
        value
    };
    tracker.observe(report(2_000, Some(2_000)), now);
    assert_eq!(tracker.value.eta_secs, None, "no rate after one report");
    assert_eq!(tracker.value.cached_tokens, Some(2_000));
    tracker.observe(report(4_000, None), now + Duration::from_secs(2));
    // 2000 tokens in 2 s leaves 6000 tokens: 6 s.
    assert_eq!(tracker.value.eta_secs, Some(6.0));
    assert_eq!(tracker.value.cached_tokens, Some(2_000));
    tracker.text("first", now + Duration::from_secs(3));
    assert_eq!(
        tracker.value.eta_secs, None,
        "generation clears the prompt ETA"
    );
}
