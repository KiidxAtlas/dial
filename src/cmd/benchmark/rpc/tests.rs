use super::*;

#[test]
fn only_actual_usage_is_counted_and_partial_usage_stays_incomplete() {
    let mut metrics = RunMetrics::default();
    observe(
        &mut metrics,
        &json!({"event":"usage","prompt_tokens":null,"output_tokens":12}),
    );
    observe(
        &mut metrics,
        &json!({"event":"usage","prompt_tokens":30,"output_tokens":7}),
    );
    assert!(metrics.token_usage_observed && metrics.token_usage_incomplete);
    assert_eq!((metrics.prompt_tokens, metrics.output_tokens), (30, 7));
    assert_eq!(metrics.repair_attempts, None);
    observe(
        &mut metrics,
        &json!({"event":"tool_result","name":"edit","is_error":true}),
    );
    assert_eq!(metrics.tool_errors, 1);
    assert_eq!(
        metrics.repair_attempts, None,
        "tool errors are not repair attempts"
    );
}

#[test]
fn diagnostics_are_bounded_and_drained() {
    let input = vec![b'x'; MAX_DIAGNOSTIC_BYTES * 3];
    let mut cursor = std::io::Cursor::new(input);
    let captured = Mutex::new(Vec::new());
    capture_diagnostics(&mut cursor, &captured);
    assert_eq!(cursor.position(), (MAX_DIAGNOSTIC_BYTES * 3) as u64);
    assert_eq!(*captured.lock().unwrap(), vec![b'x'; MAX_DIAGNOSTIC_BYTES]);
}

#[test]
fn closed_rpc_is_not_misreported_as_timeout() {
    let (tx, rx) = mpsc::channel();
    drop(tx);
    let error = receive(&rx, Some(Instant::now() + Duration::from_secs(1))).unwrap_err();
    assert_eq!(error.to_string(), "benchmark RPC closed");
    let (_tx, rx) = mpsc::channel();
    let error = receive(&rx, Some(Instant::now())).unwrap_err();
    assert_eq!(error.to_string(), "benchmark RPC timed out");
}
#[test]
fn runtime_ledger_replaces_main_agent_events_across_turns() {
    let mut metrics = RunMetrics::default();
    // Two turns of main-agent usage events; compaction and worker calls emit none.
    for (prompt, output) in [(100u64, 10u64), (120, 12)] {
        observe(
            &mut metrics,
            &json!({"event":"usage","prompt_tokens":prompt,"output_tokens":output}),
        );
    }
    assert_eq!((metrics.prompt_tokens, metrics.output_tokens), (220, 22));
    // The runtime ledger already contains those events plus the other requests.
    let ledger = json!({"usage":{"total":{"all":{
        "input":400,"output":40,"cache_read":50,"reasoning":7,"requests":5,
        "successful_requests":5,"reported_responses":5,"unreported_attempts":0,
        "failed_requests":0,"cost_coverage":"observed_priced","observed_cost_usd":0.25}}}});
    apply_ledger(&mut metrics, read_ledger(&ledger));
    assert_eq!((metrics.prompt_tokens, metrics.output_tokens), (400, 40));
    assert_eq!(
        (
            metrics.cached_tokens,
            metrics.reasoning_tokens,
            metrics.requests
        ),
        (50, 7, 5)
    );
    assert_eq!(metrics.observed_cost_usd, Some(0.25));
    assert!(metrics.token_usage_observed && !metrics.token_usage_incomplete);
    assert!(metrics.observed_cost_complete);
    let mut partial_price = ledger.clone();
    partial_price["usage"]["total"]["all"]["cost_coverage"] = json!("partial");
    apply_ledger(&mut metrics, read_ledger(&partial_price));
    assert!(!metrics.observed_cost_complete);
    assert!(!metrics.token_usage_incomplete);
}

#[test]
fn partial_or_missing_ledger_keeps_coverage_unknown() {
    let all = |requests: u64, successful: u64, reported: u64, unreported: u64| {
        json!({"usage":{"total":{"all":{
            "input":10,"output":1,"cache_read":0,"reasoning":0,"requests":requests,
            "successful_requests":successful,"reported_responses":reported,
            "failed_requests":0,"unreported_attempts":unreported}}}})
    };
    // An attempt whose usage never arrived.
    let mut metrics = RunMetrics::default();
    apply_ledger(&mut metrics, read_ledger(&all(3, 3, 2, 1)));
    assert!(metrics.token_usage_incomplete);
    assert_eq!(metrics.prompt_tokens, 10);
    // A successful response without usage, with nothing marked unreported.
    let mut metrics = RunMetrics::default();
    apply_ledger(&mut metrics, read_ledger(&all(2, 2, 1, 0)));
    assert!(metrics.token_usage_incomplete);
    // A worker still in flight cannot be reported as settled, even with no send yet.
    apply_ledger(&mut metrics, read_ledger(&all(2, 1, 1, 0)));
    assert!(metrics.token_usage_incomplete);
    // An older runtime without the counters, or no snapshot, is unknown, not zero.
    let mut metrics = RunMetrics::default();
    apply_ledger(&mut metrics, read_ledger(&json!({"state":{}})));
    assert!(metrics.token_usage_incomplete);
    // A complete ledger with no requests is a real zero, not missing coverage.
    let mut metrics = RunMetrics::default();
    apply_ledger(&mut metrics, read_ledger(&all(0, 0, 0, 0)));
    assert!(!metrics.token_usage_incomplete && !metrics.token_usage_observed);
}

#[test]
fn snapshot_from_the_real_usage_type_parses() {
    let state = json!({"usage":{"total":dial::llm::usage::Groups::default()}});
    let ledger = read_ledger(&state).expect("real usage snapshot");
    assert!(ledger.complete);
    assert_eq!(ledger.requests, 0);
}

fn snapshot(successful: u64, offered: u64, invoked: u64) -> Value {
    json!({"successful_requests": successful, "offered": {"read": offered}, "invoked": {"read": invoked}})
}

#[test]
fn multi_turn_trace_uses_the_last_cumulative_snapshot() {
    let trace = tool_trace_for_run(Some(&snapshot(4, 4, 3)), 2, 2, true).expect("snapshot present");
    assert!(trace.complete);
    assert_eq!(trace.successful_requests, 4);
    assert_eq!(trace.offered.get("read"), Some(&4));
    assert_eq!(trace.invoked.get("read"), Some(&3));
}

#[test]
fn failed_runs_and_missing_turn_snapshots_are_incomplete_not_absent() {
    let failed =
        tool_trace_for_run(Some(&snapshot(2, 2, 1)), 2, 2, false).expect("partial trace kept");
    assert!(!failed.complete);
    assert_eq!(failed.invoked.get("read"), Some(&1));
    let partial =
        tool_trace_for_run(Some(&snapshot(2, 2, 1)), 3, 2, true).expect("partial trace kept");
    assert!(!partial.complete);
}

#[test]
fn legacy_children_without_snapshots_stay_unknown() {
    assert_eq!(tool_trace_for_run(None, 2, 0, true), None);
    assert_eq!(
        tool_trace_for_run(Some(&json!({"offered": {}})), 1, 1, true),
        None
    );
}
