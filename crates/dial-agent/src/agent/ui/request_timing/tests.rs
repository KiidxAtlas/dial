use super::*;

#[test]
fn partitions_wait_prefill_generation_backoff_and_cancellation() {
    let start = Instant::now();
    let mut clock = RequestClock::new(start);
    clock.phase(RequestPhase::Loading, start + Duration::from_millis(10));
    clock.phase(RequestPhase::Processing, start + Duration::from_millis(30));
    clock.phase(RequestPhase::Generating, start + Duration::from_millis(60));
    clock.phase(RequestPhase::Generating, start + Duration::from_millis(70));
    clock.phase(RequestPhase::Backoff, start + Duration::from_millis(100));
    clock.phase(RequestPhase::Backoff, start + Duration::from_millis(110));
    clock.phase(RequestPhase::Retrying, start + Duration::from_millis(150));
    clock.phase(RequestPhase::Cancelling, start + Duration::from_millis(210));
    let (_, report) = clock.finish(false, start + Duration::from_millis(280));
    assert_eq!(report["response_wait_ms"], 70);
    assert_eq!(report["loading_ms"], 20);
    assert_eq!(report["prompt_processing_ms"], 30);
    assert_eq!(report["generation_ms"], 40);
    assert_eq!(report["retry_backoff_ms"], 50);
    assert_eq!(report["cancellation_ms"], 70);
    assert_eq!(report["total_ms"], 280);
    assert_eq!(report["first_generation_ms"], 60);
    assert_eq!(report["retry_backoffs"], 1);
    assert_eq!(report["failed_requests"], 1);
}

#[test]
fn silent_failure_has_no_invented_first_token_and_totals_keep_submillisecond_time() {
    let start = Instant::now();
    let mut totals = Timing::default();
    for succeeded in [false, true] {
        let clock = RequestClock::new(start);
        let (timing, report) = clock.finish(succeeded, start + Duration::from_micros(750));
        assert!(report["first_generation_ms"].is_null());
        totals.add(&timing);
    }
    let report = totals.snapshot();
    assert_eq!(report["requests"], 2);
    assert_eq!(report["failed_requests"], 1);
    assert_eq!(report["response_wait_ms"], 1);
    assert_eq!(report["total_ms"], 1);
}

#[test]
fn silent_stream_gaps_during_generation_are_reported_as_stalls_within_generation() {
    let start = Instant::now();
    let mut clock = RequestClock::new(start);
    clock.phase(RequestPhase::Generating, start + Duration::from_secs(1));
    clock.delta(start + Duration::from_secs(2));
    clock.delta(start + Duration::from_secs(3));
    // A 40 s silent stream gap, then deltas resume.
    clock.delta(start + Duration::from_secs(43));
    clock.delta(start + Duration::from_secs(44));
    let (timing, report) = clock.finish(true, start + Duration::from_secs(45));
    assert_eq!(report["generation_ms"], 44_000);
    assert_eq!(report["generation_stall_ms"], 40_000);
    assert_eq!(
        report["total_ms"], 45_000,
        "stalls are a subset, not added time"
    );
    let mut totals = Timing::default();
    totals.add(&timing);
    assert_eq!(totals.snapshot()["generation_stall_ms"], 40_000);

    // A stream that goes silent and then ends counts its trailing gap.
    let mut clock = RequestClock::new(start);
    clock.phase(RequestPhase::Generating, start);
    clock.delta(start + Duration::from_secs(1));
    let (_, report) = clock.finish(true, start + Duration::from_secs(21));
    assert_eq!(report["generation_stall_ms"], 20_000);

    // Gaps while waiting for the first byte are response wait, not generation stalls.
    let mut clock = RequestClock::new(start);
    clock.delta(start + Duration::from_secs(30));
    let (_, report) = clock.finish(true, start + Duration::from_secs(31));
    assert_eq!(report["generation_stall_ms"], 0);
}
