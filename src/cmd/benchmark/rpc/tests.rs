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
fn journey_compaction_requires_an_observed_history_rewrite() {
    assert!(require_compaction(&json!({"compacted":true})).is_ok());
    for result in [
        json!({"compacted":false,"outcome":{"kind":"unchanged"}}),
        json!({"context_reset":true}),
        json!({}),
    ] {
        let error = require_compaction(&result).unwrap_err();
        assert!(error.to_string().contains("history was not compacted"));
    }
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

// Observe the real child launch without starting Dial or a model. Closing stdout after
// recording argv deliberately stops the benchmark at the ready handshake.
#[cfg(unix)]
#[test]
fn rpc_child_inherits_only_a_bounded_case_deadline() {
    use clap::Parser as _;
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let child = dir.path().join("rpc-child");
    std::fs::write(
        &child,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$DIAL_HOME/args\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
    for limit in [180, 0] {
        let home = tempfile::tempdir().unwrap();
        let error = run_configured(
            &child,
            home.path(),
            &BenchmarkCase::default(),
            dir.path(),
            CaseLimits {
                wall_secs: limit,
                inactivity_secs: 0,
            },
            None,
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("benchmark RPC closed"),
            "{error:#}"
        );
        let args = std::fs::read_to_string(home.path().join("args")).unwrap();
        let cli =
            crate::args::Cli::try_parse_from(std::iter::once("dial").chain(args.lines())).unwrap();
        match limit {
            0 => assert!(cli.model_flags.max_time.is_none()),
            _ => assert!(
                cli.model_flags
                    .max_time
                    .is_some_and(|s| s > 0.0 && s <= limit as f64),
                "missing or invalid child time budget: {args}"
            ),
        }
    }
}

#[test]
fn live_progress_is_kept_beside_usage_and_cleared_by_usage() {
    let mut metrics = RunMetrics::default();
    observe(
        &mut metrics,
        &json!({"event":"model_progress","progress":{"phase":"generating","output_tokens":17,"prompt_tokens":40,"estimated":true,"tokens_per_second":3.5}}),
    );
    assert_eq!(
        metrics
            .inflight_progress
            .as_ref()
            .and_then(|p| p.output_tokens),
        Some(17)
    );
    assert_eq!((metrics.prompt_tokens, metrics.output_tokens), (0, 0));
    assert!(!metrics.token_usage_observed && !metrics.token_usage_incomplete);
    assert_eq!(
        metrics.last_child_event.as_ref().map(|e| e.kind.as_str()),
        Some("model_progress")
    );
    observe(
        &mut metrics,
        &json!({"event":"usage","prompt_tokens":40,"output_tokens":17}),
    );
    assert_eq!(metrics.inflight_progress, None);
    assert_eq!((metrics.prompt_tokens, metrics.output_tokens), (40, 17));
}

// A child that is ready, accepts one send, reports live generation and then stalls silently.
// The deadline must keep the live counts and must not present unread usage as zero.
#[cfg(unix)]
#[test]
fn timeout_during_generation_keeps_live_counts_without_claiming_zero_usage() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let child = dir.path().join("rpc-child");
    std::fs::write(
        &child,
        r#"#!/bin/sh
printf '%s\n' '{"event":"ready"}'
read -r _request
printf '%s\n' '{"event":"model_progress","progress":{"phase":"generating","output_tokens":17}}'
sleep 30
"#,
    )
    .unwrap();
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = tempfile::tempdir().unwrap();
    let error = run_configured(
        &child,
        home.path(),
        &BenchmarkCase::default(),
        dir.path(),
        CaseLimits {
            wall_secs: 1,
            inactivity_secs: 0,
        },
        None,
        None,
    )
    .unwrap_err();
    let AgentFailure { metrics, detail } = error
        .downcast::<AgentFailure>()
        .expect("a timed-out case keeps its observed metrics");
    assert!(metrics.agent_error && !metrics.agent_succeeded);
    assert!(!metrics.token_usage_observed);
    assert!(
        metrics.token_usage_incomplete,
        "a send with no ledger read is unknown usage, not zero"
    );
    assert_eq!(
        metrics
            .inflight_progress
            .as_ref()
            .and_then(|p| p.output_tokens),
        Some(17)
    );
    assert!(detail.contains("timed out"), "{detail}");
    assert!(detail.contains("send"), "{detail}");
    assert!(
        detail.contains("last child event model_progress") && detail.contains("in flight"),
        "{detail}"
    );
}

#[test]
fn closed_rpc_is_not_misreported_as_timeout() {
    let (tx, rx) = mpsc::channel();
    drop(tx);
    let error = receive(
        &rx,
        Guard::wall(Some(Instant::now() + Duration::from_secs(1))),
        &RunMetrics::default(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "benchmark RPC closed");
    let (_tx, rx) = mpsc::channel();
    let error = receive(
        &rx,
        Guard::wall(Some(Instant::now())),
        &RunMetrics::default(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "benchmark RPC timed out");
}

#[test]
fn expired_receive_rejects_buffered_events_and_replies() {
    for event in [json!({"event":"ready"}), json!({"id":4,"result":{}})] {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(event)).unwrap();
        let error = receive(
            &rx,
            Guard::wall(Some(Instant::now())),
            &RunMetrics::default(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "benchmark RPC timed out");
    }
}

#[test]
fn request_failures_identify_the_pending_operation_without_logging_params() {
    let (_tx, rx) = mpsc::channel();
    let mut input = Vec::new();
    let error = request(
        &mut input,
        &rx,
        4,
        "compact",
        json!({"private_context":"not-for-diagnostics"}),
        Guard::wall(Some(Instant::now())),
        &mut RunMetrics::default(),
    )
    .unwrap_err();
    let detail = format!("{error:#}");
    assert!(detail.contains("compact"));
    assert!(detail.contains("request 4"));
    assert!(detail.contains("timed out"));
    assert!(!detail.contains("not-for-diagnostics"));
    assert!(input.is_empty(), "expired request must not emit bytes");
}

#[test]
fn deadline_failures_attribute_time_to_completed_and_pending_phases() {
    let started = Instant::now();
    let mut phases = Phases::default();
    assert_eq!(
        phases.summary(started, started + Duration::from_millis(2_500)),
        "case elapsed 2.5 s; completed: none; pending for 2.5 s"
    );
    phases.start(started);
    phases.done("ready", started + Duration::from_secs(3));
    phases.done("send#1", started + Duration::from_secs(43));
    phases.done("compact#2", started + Duration::from_secs(100));
    assert_eq!(
        phases.summary(started, started + Duration::from_secs(180)),
        "case elapsed 180.0 s; completed: ready 3.0 s, send#1 40.0 s, compact#2 57.0 s; \
         pending for 80.0 s"
    );
}

#[test]
fn timing_survives_a_lost_reply_and_only_counts_passes_from_this_send() {
    let mut metrics = RunMetrics::default();
    for (id, method, events) in [
        (
            1,
            "send",
            vec![
                json!({"event":"tool_result","is_error":false}),
                json!({"event":"check_passed","request_id":2}),
                json!({"id":1,"result":{}}),
            ],
        ),
        (
            2,
            "send",
            vec![
                json!({"event":"check_passed","request_id":2}),
                json!({"event":"check_passed","request_id":2}),
                json!({"id":2,"result":{}}),
            ],
        ),
        (
            3,
            "send",
            vec![json!({"event":"check_passed","request_id":3})],
        ),
        (
            4,
            "state",
            vec![
                json!({"event":"check_passed","request_id":4}),
                json!({"id":4,"result":{}}),
            ],
        ),
    ] {
        let (tx, rx) = mpsc::channel();
        for event in events {
            tx.send(Ok(event)).unwrap();
        }
        drop(tx);
        let result = request(
            &mut Vec::new(),
            &rx,
            id,
            method,
            json!({}),
            Guard::wall(None),
            &mut metrics,
        );
        assert_eq!(result.is_err(), id == 3);
    }
    let observed: Vec<_> = metrics
        .post_check_timings
        .iter()
        .map(|t| (t.send_id, t.end))
        .collect();
    assert_eq!(
        observed,
        [(2, PostCheckEnd::Reply), (3, PostCheckEnd::RpcError)]
    );
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

#[test]
fn ledger_keeps_orthogonal_scopes_and_unknown_discarded_costs() {
    let mut state = json!({"usage":{"total":dial::llm::usage::Groups::default()}});
    let total = &mut state["usage"]["total"];
    total["all"]["input"] = json!(30);
    total["all"]["requests"] = json!(2);
    total["all"]["successful_requests"] = json!(1);
    total["all"]["failed_requests"] = json!(1);
    total["all"]["reported_responses"] = json!(1);
    total["all"]["discarded_attempts"] = json!(1);
    total["all"]["unreported_attempts"] = json!(1);
    total["main"]["input"] = json!(10);
    total["workers"]["input"] = json!(20);
    total["by_purpose"] = json!({"agent":{"input":10},"repair":{"input":20}});
    let mut metrics = RunMetrics::default();
    apply_ledger(&mut metrics, read_ledger(&state));
    assert_eq!(metrics.prompt_tokens, 30);
    assert_eq!(
        metrics.runtime_usage,
        state.pointer("/usage/total").cloned()
    );
    assert!(metrics.token_usage_incomplete);
    assert_eq!(metrics.observed_cost_usd, None);
    assert_eq!(
        metrics.runtime_usage.as_ref().unwrap()["all"].get("discarded_cost_usd"),
        None
    );
}

#[test]
fn efficiency_retains_each_turn_and_labels_recovery_without_inventing_a_turn() {
    let mut metrics = RunMetrics::default();
    retain_efficiency(&mut metrics, &json!({}), Some(1));
    assert!(metrics.efficiency.is_none());
    retain_efficiency(
        &mut metrics,
        &json!({"efficiency":{"duration_ms":10}}),
        Some(1),
    );
    retain_efficiency(
        &mut metrics,
        &json!({"efficiency":{"duration_ms":20}}),
        Some(2),
    );
    retain_efficiency(
        &mut metrics,
        &json!({"efficiency":{"completed":false}}),
        None,
    );
    let report = metrics.efficiency.unwrap();
    assert_eq!(report["turns"].as_array().unwrap().len(), 2);
    assert_eq!(report["turns"][0]["report"]["duration_ms"], 10);
    assert_eq!(report["turns"][1]["send"], 2);
    assert_eq!(report["recovery_observation"]["completed"], false);
}

#[test]
fn shared_auth_requires_opt_in_and_does_not_replace_isolated_home() {
    let isolated = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let key = dial::config::paths::SUBSCRIPTION_AUTH_HOME;
    let mut command = Command::new("dial");
    command
        .env("DIAL_HOME", isolated.path())
        .env(key, "ambient-override");
    configure_subscription_auth(&mut command, None).unwrap();
    assert_eq!(
        command
            .get_envs()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value),
        Some(None)
    );
    configure_subscription_auth(&mut command, Some(shared.path())).unwrap();
    let canonical = std::fs::canonicalize(shared.path()).unwrap();
    assert_eq!(
        command
            .get_envs()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value),
        Some(Some(canonical.as_os_str()))
    );
    assert_eq!(
        command
            .get_envs()
            .find(|(name, _)| *name == "DIAL_HOME")
            .map(|(_, value)| value),
        Some(Some(isolated.path().as_os_str()))
    );
    assert!(
        configure_subscription_auth(&mut command, Some(&shared.path().join("missing"))).is_err()
    );
}

#[test]
fn benchmark_accepts_answered_turns_without_claiming_work_completion() {
    for previous in [0, 1, 3] {
        let answer = json!({"outcome":"answered","metrics":{"turns_completed":previous}});
        assert_eq!(completed_turns(&answer, previous).unwrap(), previous);
        let completed = json!({"outcome":"completed","metrics":{"turns_completed":previous+1}});
        assert_eq!(completed_turns(&completed, previous).unwrap(), previous + 1);
    }
    for outcome in [
        "blocked",
        "incomplete",
        "failed",
        "needs_input",
        "interrupted",
        "unknown",
    ] {
        assert!(
            completed_turns(
                &json!({"outcome":outcome,"metrics":{"turns_completed":1}}),
                0
            )
            .is_err()
        );
    }
    assert!(completed_turns(&json!({"outcome":"answered"}), 0).is_err());
    assert!(
        completed_turns(
            &json!({"outcome":"completed","metrics":{"turns_completed":0}}),
            0
        )
        .is_err()
    );
    assert!(
        completed_turns(
            &json!({"outcome":"answered","metrics":{"turns_completed":1}}),
            0
        )
        .is_err()
    );
}

#[test]
fn worker_counters_sum_only_when_every_worker_reports_them() {
    let node = |kind: &str, phase: &str, data: Value| json!({"kind": kind, "result": {"phase": phase, "data": data}});
    let tree = |nodes: Vec<Value>| json!({"tree": nodes});
    let cases = [
        (
            "terminal workers with every counter",
            tree(vec![
                node(
                    "worker",
                    "completed",
                    json!({"tools": 2, "steps": 3, "elapsed_ms": 40}),
                ),
                node(
                    "worker",
                    "failed",
                    json!({"tools": 1, "steps": 1, "elapsed_ms": 10}),
                ),
                node("task", "completed", json!({"tools": 99, "steps": 99})),
            ]),
            Some((2, 0, Some(3), Some(4), Some(50))),
        ),
        (
            "running worker keeps the partial snapshot incomplete",
            tree(vec![node(
                "worker",
                "running",
                json!({"tools": 2, "steps": 1}),
            )]),
            Some((1, 1, Some(2), Some(1), None)),
        ),
        (
            "terminal worker without elapsed time",
            tree(vec![node(
                "worker",
                "completed",
                json!({"tools": 2, "steps": 1}),
            )]),
            Some((1, 0, Some(2), Some(1), None)),
        ),
        (
            "unknown or corrupt phase is never terminal",
            tree(vec![node(
                "worker",
                "paused",
                json!({"tools": 1, "steps": 1, "elapsed_ms": 5}),
            )]),
            Some((1, 1, Some(1), Some(1), None)),
        ),
        (
            "worker without a tool counter",
            tree(vec![node(
                "worker",
                "completed",
                json!({"steps": 1, "elapsed_ms": 5}),
            )]),
            Some((1, 0, None, Some(1), Some(5))),
        ),
        (
            "no workers in a present tree is a known zero",
            tree(vec![node("session", "completed", json!({}))]),
            Some((0, 0, Some(0), Some(0), Some(0))),
        ),
        ("missing tree stays unknown", json!({}), None),
    ];
    for (name, state, expected) in cases {
        let actual = read_worker_execution(&state).map(|w| {
            (
                w.workers,
                w.running,
                w.tool_starts,
                w.model_steps,
                w.elapsed_ms,
            )
        });
        assert_eq!(actual, expected, "{name}");
    }
}

// Scripted children for the inactivity guard. Each speaks just enough RPC for one send, state and
// shutdown; the first send's reply is the only one that differs between them.
#[cfg(unix)]
const SILENT_CHILD: &str = r#"#!/bin/sh
printf '%s\n' '{"event":"ready"}'
read -r _request
sleep 30
"#;
#[cfg(unix)]
const HEARTBEAT_CHILD: &str = r#"#!/bin/sh
printf '%s\n' '{"event":"ready"}'
read -r _request
i=0
while [ $i -lt 150 ]; do
  printf '%s\n' '{"event":"model_progress","progress":{"phase":"generating","output_tokens":5}}'
  sleep 0.2
  i=$((i+1))
done
"#;
#[cfg(unix)]
const PROGRESSING_CHILD: &str = r#"#!/bin/sh
printf '%s\n' '{"event":"ready"}'
read -r _request
i=0
while [ $i -lt 10 ]; do
  sleep 0.2
  printf '%s\n' "{\"event\":\"model_progress\",\"progress\":{\"phase\":\"generating\",\"output_tokens\":$i}}"
  i=$((i+1))
done
printf '%s\n' '{"id":1,"result":{"text":"done","interrupted":false,"metrics":{"turns_completed":1}}}'
read -r _request
printf '%s\n' '{"id":2,"result":{}}'
read -r _request
printf '%s\n' '{"id":3,"result":{}}'
"#;

/// The inactivity bound is opt-in, separate from the wall deadline, and restarts only on meaningful
/// progress. Failures keep the metrics observed before the bound fired.
#[cfg(unix)]
#[test]
fn inactivity_guard_is_opt_in_and_resets_only_on_meaningful_progress() {
    use std::os::unix::fs::PermissionsExt as _;

    // (child script, wall s, inactivity s, expected failure needle or None for success,
    //  output tokens preserved from in-flight progress)
    type Row = (&'static str, u64, u64, Option<&'static str>, Option<u64>);
    let cases: [Row; 4] = [
        (
            SILENT_CHILD,
            0,
            1,
            Some("inactive: no meaningful progress for 1 s"),
            None,
        ),
        // Identical progress reports do not reset the window, so the bound still fires.
        (HEARTBEAT_CHILD, 0, 1, Some("inactive"), Some(5)),
        // Advancing progress keeps a 2 s generation alive under a 1 s bound.
        (PROGRESSING_CHILD, 0, 1, None, None),
        // Without the opt-in, silence ends at the wall deadline and never as an inactivity failure.
        (SILENT_CHILD, 1, 0, Some("timed out"), None),
    ];
    for (script, wall, inactivity, expected, preserved) in cases {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("rpc-child");
        std::fs::write(&child, script).unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = tempfile::tempdir().unwrap();
        let outcome = run_configured(
            &child,
            home.path(),
            &BenchmarkCase::default(),
            dir.path(),
            CaseLimits {
                wall_secs: wall,
                inactivity_secs: inactivity,
            },
            None,
            None,
        );
        match (outcome, expected) {
            (Ok(metrics), None) => {
                assert!(metrics.agent_succeeded && !metrics.agent_error);
                assert_eq!(metrics.turns_completed, 1);
            }
            (Err(error), Some(needle)) => {
                let AgentFailure { metrics, detail } = error
                    .downcast::<AgentFailure>()
                    .expect("a bounded case keeps its observed metrics");
                assert!(detail.contains(needle), "{detail}");
                assert!(metrics.agent_error && !metrics.agent_succeeded);
                assert!(!metrics.token_usage_observed && metrics.token_usage_incomplete);
                assert_eq!(
                    metrics
                        .inflight_progress
                        .as_ref()
                        .and_then(|p| p.output_tokens),
                    preserved
                );
                assert_eq!(
                    detail.contains("in flight"),
                    preserved.is_some(),
                    "{detail}"
                );
            }
            (outcome, expected) => panic!(
                "expected {expected:?}, got {:?}",
                outcome.map(|metrics| metrics.agent_succeeded)
            ),
        }
    }
}

#[test]
fn inactivity_limit_cannot_exceed_the_wall_deadline() {
    let error = run_configured(
        Path::new("unused"),
        Path::new("unused"),
        &BenchmarkCase::default(),
        Path::new("unused"),
        CaseLimits {
            wall_secs: 10,
            inactivity_secs: 11,
        },
        None,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("inactivity limit"), "{error}");
}
