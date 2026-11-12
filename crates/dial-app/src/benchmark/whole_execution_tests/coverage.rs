use super::{RunMetrics, ledger, trace, worker};

#[test]
fn partial_worker_tree_does_not_claim_a_complete_execution_total() {
    let metrics = RunMetrics {
        tool_trace: Some(trace(true, &[("read", 1)])),
        // The tree accounts for four worker model steps, but the ledger saw five requests.
        worker_execution: Some(worker(0, Some(5), Some(900))),
        runtime_usage: Some(ledger(2, 5)),
        ..Default::default()
    };
    let whole = metrics.derive_whole_execution();
    assert_eq!(whole.worker_requests, Some(5));
    assert_eq!(whole.worker_tool_dispatches, None);
    assert_eq!(whole.worker_elapsed_ms, None);
    assert_eq!(whole.tool_dispatches, None);
    assert!(!whole.complete);
}
