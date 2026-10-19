//! Tool-trace contracts: legacy compatibility, snapshot parsing, and the real suite parser.
use super::{BenchmarkSuite, RunMetrics, ToolTrace, ToolTraceScope};
use serde_json::json;
use std::path::Path;

fn repository_suite() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/benchmarks/tool-effectiveness.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn legacy_metrics_without_a_trace_stay_unknown_and_serialize_unchanged() {
    let legacy: RunMetrics = serde_json::from_value(json!({
        "prompt_tokens": 3, "output_tokens": 4, "elapsed_ms": 5, "tool_calls": 0, "tool_errors": 0
    }))
    .unwrap();
    assert_eq!(legacy.tool_trace, None);
    let value = serde_json::to_value(RunMetrics::default()).unwrap();
    assert!(
        value.get("tool_trace").is_none(),
        "absent traces must not add a key"
    );
}

#[test]
fn trace_round_trips_with_per_tool_counts() {
    let trace = ToolTrace {
        scope: ToolTraceScope::MainAgent,
        complete: true,
        successful_requests: 2,
        offered: [("read".to_owned(), 2), ("find".to_owned(), 1)]
            .into_iter()
            .collect(),
        invoked: [("read".to_owned(), 1)].into_iter().collect(),
    };
    let metrics = RunMetrics {
        tool_trace: Some(trace.clone()),
        ..Default::default()
    };
    let value = serde_json::to_value(&metrics).unwrap();
    assert_eq!(value["tool_trace"]["scope"], "main_agent");
    let parsed: RunMetrics = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.tool_trace, Some(trace));
}

#[test]
fn snapshot_parsing_accepts_only_complete_counter_shapes() {
    let snapshot =
        json!({"successful_requests": 3, "offered": {"read": 3}, "invoked": {"read": 2}});
    let parsed = ToolTrace::from_snapshot(&snapshot, false).unwrap();
    assert!(
        !parsed.complete,
        "completeness is decided by the caller, not the snapshot"
    );
    assert_eq!(parsed.successful_requests, 3);
    assert_eq!(parsed.invoked.get("read"), Some(&2));

    let malformed = [
        json!({"offered": {}, "invoked": {}}),
        json!({"successful_requests": "3", "offered": {}, "invoked": {}}),
        json!({"successful_requests": 1, "offered": {"read": -1}, "invoked": {}}),
        json!({"successful_requests": 1, "offered": {"read": 1.5}, "invoked": {}}),
        json!({"successful_requests": 1, "offered": {"": 1}, "invoked": {}}),
        json!({"successful_requests": 1, "offered": [], "invoked": {}}),
        json!("not an object"),
    ];
    for snapshot in malformed {
        assert_eq!(
            ToolTrace::from_snapshot(&snapshot, true),
            None,
            "{snapshot}"
        );
    }
}

#[test]
fn repository_tool_effectiveness_suite_parses_with_the_real_suite_parser() {
    let suite =
        BenchmarkSuite::parse_toml(&repository_suite()).expect("suite must parse and validate");
    assert_eq!(suite.name, "tool-effectiveness-v1");
    assert_eq!(suite.cases.len(), 5);
    for case in &suite.cases {
        assert!(!case.checks.is_empty(), "{}", case.id);
        assert!(
            case.checks
                .iter()
                .all(|check| !matches!(check, super::Assertion::Metric { .. })),
            "{} must not use tool-count metrics as success",
            case.id
        );
    }
}
