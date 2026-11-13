//! Whole-execution view: parent trace, worker snapshot and ledger groups stay disjoint, and any
//! missing source stays unknown rather than zero.
use super::{RunMetrics, ToolTrace, ToolTraceScope, WorkerExecution};
use serde_json::json;

mod coverage;

fn trace(complete: bool, invoked: &[(&str, u64)]) -> ToolTrace {
    ToolTrace {
        scope: ToolTraceScope::MainAgent,
        complete,
        successful_requests: 2,
        offered: Default::default(),
        invoked: invoked
            .iter()
            .map(|(name, count)| ((*name).to_owned(), *count))
            .collect(),
    }
}

fn worker(running: u64, tool_starts: Option<u64>, elapsed_ms: Option<u64>) -> WorkerExecution {
    WorkerExecution {
        workers: 2,
        running,
        tool_starts,
        model_steps: Some(4),
        elapsed_ms,
    }
}

fn ledger(main: u64, workers: u64) -> serde_json::Value {
    json!({
        "all": {"requests": main + workers},
        "main": {"requests": main},
        "workers": {"requests": workers},
    })
}

#[test]
fn legacy_reports_without_worker_or_ledger_fields_stay_unknown() {
    let legacy: RunMetrics = serde_json::from_value(json!({
        "prompt_tokens": 3, "output_tokens": 4, "elapsed_ms": 5, "tool_calls": 0, "tool_errors": 0
    }))
    .unwrap();
    assert_eq!(legacy.worker_execution, None);
    assert_eq!(legacy.whole_execution, None);
    let whole = legacy.derive_whole_execution();
    assert_eq!(whole.parent_tool_dispatches, None);
    assert_eq!(whole.worker_tool_dispatches, None);
    assert_eq!(whole.tool_dispatches, None);
    assert_eq!(whole.parent_requests, None);
    assert_eq!(whole.worker_requests, None);
    assert!(!whole.complete);
    let value = serde_json::to_value(RunMetrics::default()).unwrap();
    assert!(
        value.get("worker_execution").is_none() && value.get("whole_execution").is_none(),
        "absent snapshots must not add keys"
    );
}

#[test]
fn complete_sources_sum_parent_and_worker_dispatches_once_and_serialize() {
    let mut metrics = RunMetrics {
        tool_trace: Some(trace(true, &[("read", 3), ("task", 1)])),
        worker_execution: Some(worker(0, Some(5), Some(900))),
        runtime_usage: Some(ledger(2, 4)),
        ..Default::default()
    };
    metrics.whole_execution = Some(metrics.derive_whole_execution());
    let whole = metrics.whole_execution.unwrap();
    assert_eq!(whole.parent_tool_dispatches, Some(4));
    assert_eq!(whole.worker_tool_dispatches, Some(5));
    assert_eq!(whole.tool_dispatches, Some(9));
    assert_eq!(whole.parent_requests, Some(2));
    assert_eq!(whole.worker_requests, Some(4));
    assert_eq!(whole.worker_elapsed_ms, Some(900));
    assert!(whole.complete);
    let value = serde_json::to_value(&metrics).unwrap();
    assert_eq!(value["whole_execution"]["tool_dispatches"], 9);
    assert_eq!(value["whole_execution"]["worker_requests"], 4);
}

/// Each row: metrics, then the expected parent dispatches, worker dispatches, combined dispatches,
/// worker elapsed and the ledger-derived request groups. `None` means unknown, never zero.
#[test]
fn partial_sources_keep_their_own_totals_and_block_the_combined_total() {
    // Keep the worker step count aligned with this table's one-request ledger so each row
    // isolates its named missing source, rather than failing the independent coverage guard.
    let worker = |running, tool_starts, elapsed_ms| WorkerExecution {
        model_steps: Some(1),
        ..worker(running, tool_starts, elapsed_ms)
    };
    type Row = (
        &'static str,
        RunMetrics,
        Option<u64>,
        Option<u64>,
        Option<u64>,
        Option<u64>,
        Option<u64>,
    );
    let rows: [Row; 5] = [
        (
            "running worker",
            RunMetrics {
                tool_trace: Some(trace(true, &[("read", 1)])),
                worker_execution: Some(worker(1, Some(2), None)),
                runtime_usage: Some(ledger(1, 1)),
                ..Default::default()
            },
            Some(1),
            None,
            None,
            None,
            Some(1),
        ),
        (
            "incomplete parent trace",
            RunMetrics {
                tool_trace: Some(trace(false, &[("read", 1)])),
                worker_execution: Some(worker(0, Some(2), Some(10))),
                runtime_usage: Some(ledger(1, 1)),
                ..Default::default()
            },
            None,
            Some(2),
            None,
            Some(10),
            Some(1),
        ),
        (
            "worker missing tool counter",
            RunMetrics {
                tool_trace: Some(trace(true, &[("read", 1)])),
                worker_execution: Some(worker(0, None, Some(10))),
                runtime_usage: Some(ledger(1, 1)),
                ..Default::default()
            },
            Some(1),
            None,
            None,
            None,
            Some(1),
        ),
        (
            "ledger missing",
            RunMetrics {
                tool_trace: Some(trace(true, &[("read", 1)])),
                worker_execution: Some(worker(0, Some(2), Some(10))),
                ..Default::default()
            },
            Some(1),
            None,
            None,
            None,
            None,
        ),
        (
            "partial ledger is a lower bound, not a total",
            RunMetrics {
                tool_trace: Some(trace(true, &[("read", 1)])),
                worker_execution: Some(worker(0, Some(2), Some(10))),
                runtime_usage: Some(ledger(1, 1)),
                token_usage_incomplete: true,
                ..Default::default()
            },
            Some(1),
            None,
            None,
            None,
            None,
        ),
    ];
    for (name, metrics, parent, worker_dispatches, combined, elapsed, requests) in rows {
        let whole = metrics.derive_whole_execution();
        assert_eq!(whole.parent_tool_dispatches, parent, "{name}: parent");
        assert_eq!(
            whole.worker_tool_dispatches, worker_dispatches,
            "{name}: worker"
        );
        assert_eq!(whole.tool_dispatches, combined, "{name}: combined");
        assert_eq!(whole.worker_elapsed_ms, elapsed, "{name}: elapsed");
        assert_eq!(whole.parent_requests, requests, "{name}: requests");
        assert_eq!(whole.worker_requests, requests, "{name}: worker requests");
        assert!(!whole.complete, "{name}: complete");
    }
}

#[test]
fn zero_workers_is_a_known_zero_only_when_the_snapshot_exists() {
    let observed = RunMetrics {
        tool_trace: Some(trace(true, &[("read", 2)])),
        worker_execution: Some(WorkerExecution {
            workers: 0,
            running: 0,
            tool_starts: Some(0),
            model_steps: Some(0),
            elapsed_ms: Some(0),
        }),
        runtime_usage: Some(ledger(3, 0)),
        ..Default::default()
    };
    let whole = observed.derive_whole_execution();
    assert_eq!(whole.worker_tool_dispatches, Some(0));
    assert_eq!(whole.tool_dispatches, Some(2));
    assert!(whole.complete);
    assert_eq!(
        RunMetrics::default()
            .derive_whole_execution()
            .worker_tool_dispatches,
        None
    );
}

/// Absent worker nodes are not a known zero on their own: retired, rebound or legacy trees omit
/// them. Worker totals are known only when the ledger agrees with the snapshot.
#[test]
fn worker_totals_require_the_ledger_to_agree_with_the_snapshot() {
    let snapshot = |workers: u64, tool_starts: Option<u64>| WorkerExecution {
        workers,
        running: 0,
        tool_starts,
        model_steps: Some(if workers == 0 { 0 } else { 3 }),
        elapsed_ms: Some(0),
    };
    let cases = [
        (
            "no nodes and no worker requests",
            snapshot(0, Some(0)),
            Some(0),
            Some(0),
        ),
        (
            "no nodes but worker requests",
            snapshot(0, Some(0)),
            Some(3),
            None,
        ),
        (
            "nodes but no worker requests",
            snapshot(2, Some(4)),
            Some(0),
            None,
        ),
        (
            "nodes and worker requests agree",
            snapshot(2, Some(4)),
            Some(3),
            Some(4),
        ),
        ("ledger unknown", snapshot(0, Some(0)), None, None),
    ];
    for (name, worker_snapshot, worker_requests, expected) in cases {
        let metrics = RunMetrics {
            tool_trace: Some(trace(true, &[("read", 1)])),
            worker_execution: Some(worker_snapshot),
            runtime_usage: worker_requests.map(|requests| ledger(1, requests)),
            ..Default::default()
        };
        let whole = metrics.derive_whole_execution();
        assert_eq!(whole.worker_tool_dispatches, expected, "{name}");
    }
}
