use super::*;
use crate::storage::artifact::ArtifactStore;
use crate::tools::testkit;

fn parsed(value: Value, format: Format, limit: usize) -> Result<Value> {
    inspect(&serde_json::to_vec(&value).unwrap(), format, limit)
}
fn span(id: &str, parent: &str, start: &str, end: &str) -> Value {
    json!({"name":"operation","traceId":"11111111111111111111111111111111","spanId":id,"parentSpanId":parent,"startTimeUnixNano":start,"endTimeUnixNano":end,"status":{"code":2}})
}
fn otlp(spans: Vec<Value>) -> Value {
    json!({"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},"scopeSpans":[{"spans":spans}]}]})
}
#[test]
fn chrome_pairs_by_process_and_thread_and_reports_omissions() {
    let v = parsed(
        json!({"displayTimeUnit":"ns","traceEvents":[
            {"ph":"B","pid":1,"tid":1,"name":"a","ts":10},
            {"ph":"B","pid":1,"tid":2,"name":"b","ts":11},
            {"ph":"E","pid":1,"tid":1,"ts":20},
            {"ph":"E","pid":1,"tid":2,"ts":40},
            {"ph":"E","pid":2,"tid":1,"ts":50},
            {"ph":"B","pid":2,"tid":1,"name":"open","ts":60},
            {"ph":"X","pid":1,"tid":1,"name":"x","ts":0,"dur":50},
            {"ph":"i","name":"instant"}
        ]}),
        Format::ChromeTrace,
        2,
    )
    .unwrap();
    assert_eq!(v["time_unit"], "microseconds");
    assert_eq!(v["slowest_spans"][0]["name"], "x");
    assert_eq!(v["slowest_spans"][1]["name"], "b");
    assert_eq!(v["slowest_spans"][1]["duration_us"], 29.0);
    assert_eq!(v["coverage"]["unmatched_begins"], 1);
    assert_eq!(v["coverage"]["unmatched_ends"], 1);
    assert_eq!(v["coverage"]["unsupported_events_by_phase"]["i"], 1);
    assert_eq!(v["coverage"]["rows_omitted"], 1);
}
#[test]
fn chrome_rejects_invalid_supported_records_and_unsafe_arithmetic() {
    for event in [
        json!({"ph":"X","pid":1,"tid":1,"name":"x","ts":0,"dur":-1}),
        json!({"ph":"X","pid":1,"tid":1,"name":"x","ts":9007199254740992u64,"dur":1}),
        json!({"ph":"X","pid":1,"tid":1,"name":"x","ts":9007199254740991u64,"dur":2}),
        json!({"ph":"X","name":"missing thread","ts":0,"dur":1}),
    ] {
        assert!(parsed(json!([event]), Format::ChromeTrace, 10).is_err());
    }
    assert!(
        parsed(
            json!([
                {"ph":"B","pid":1,"tid":1,"name":"x","ts":9},
                {"ph":"E","pid":1,"tid":1,"ts":8}
            ]),
            Format::ChromeTrace,
            10
        )
        .is_err()
    );
}
#[test]
fn otlp_keeps_exact_nanoseconds_and_explicit_parent_links() {
    let v = parsed(
        otlp(vec![
            span(
                "aaaaaaaaaaaaaaaa",
                "",
                "1700000000000000001",
                "1700000000000000010",
            ),
            span(
                "bbbbbbbbbbbbbbbb",
                "aaaaaaaaaaaaaaaa",
                "1700000000000000002",
                "1700000000000000003",
            ),
            span("cccccccccccccccc", "dddddddddddddddd", "1", "2"),
        ]),
        Format::OtlpJson,
        10,
    )
    .unwrap();
    assert_eq!(v["slowest_spans"][0]["duration_ns"], "9");
    assert_eq!(
        v["slowest_spans"][0]["start_unix_ns"],
        "1700000000000000001"
    );
    assert_eq!(v["slowest_spans"][1]["parent_span_id"], "aaaaaaaaaaaaaaaa");
    assert_eq!(v["slowest_spans"][1]["parent_resolution"], "present");
    assert_eq!(v["slowest_spans"][1]["service_name"], "checkout");
    assert_eq!(v["coverage"]["orphan_parent_links"], 1);
    assert_eq!(v["coverage"]["roots"], 1);
    assert_eq!(v["coverage"]["error_status_spans"], 3);
}
#[test]
fn otlp_reports_cycles_and_does_not_resolve_across_traces() {
    let a = span("aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb", "1", "2");
    let b = span("bbbbbbbbbbbbbbbb", "aaaaaaaaaaaaaaaa", "1", "2");
    let mut c = span("cccccccccccccccc", "aaaaaaaaaaaaaaaa", "1", "2");
    c["traceId"] = json!("22222222222222222222222222222222");
    let v = parsed(otlp(vec![a, b, c]), Format::OtlpJson, 10).unwrap();
    assert_eq!(v["coverage"]["cycle_members"], 2);
    assert_eq!(v["coverage"]["orphan_parent_links"], 1);
}
#[test]
fn otlp_rejects_overflow_numbers_reversal_duplicate_and_bad_ids() {
    for (start, end) in [
        ("18446744073709551616", "18446744073709551616"),
        ("2", "1"),
        ("1.5", "2"),
        ("-1", "2"),
    ] {
        assert!(
            parsed(
                otlp(vec![span("aaaaaaaaaaaaaaaa", "", start, end)]),
                Format::OtlpJson,
                10
            )
            .is_err()
        );
    }
    let mut s = span("aaaaaaaaaaaaaaaa", "", "1", "2");
    s["startTimeUnixNano"] = json!(1);
    assert!(parsed(otlp(vec![s]), Format::OtlpJson, 10).is_err());
    let s = span("aaaaaaaaaaaaaaaa", "", "1", "2");
    assert!(parsed(otlp(vec![s.clone(), s]), Format::OtlpJson, 10).is_err());
    assert!(parsed(otlp(vec![span("bad", "", "1", "2")]), Format::OtlpJson, 10).is_err());
}
#[test]
fn folded_recursion_counts_function_once_and_leaf_as_self() {
    let v = folded::inspect("root;f;f 3\nroot;g 2\nf;f 1\n\n", 200).unwrap();
    assert_eq!(v["total_weight"], "6");
    let rows = v["functions"].as_array().unwrap();
    let f = rows.iter().find(|r| r["function"] == "f").unwrap();
    assert_eq!(f["inclusive_weight"], "4");
    assert_eq!(f["self_weight"], "4");
    assert_eq!(rows[0]["function"], "root");
    assert_eq!(rows[0]["inclusive_weight"], "5");
    assert_eq!(rows[0]["self_weight"], "0");
    assert_eq!(v["coverage"]["blank_lines"], 1);
    assert_eq!(
        folded::inspect("a 1\nb 2", 1).unwrap()["coverage"]["rows_omitted"],
        1
    );
}
#[test]
fn folded_rejects_malformed_overflow_and_deep_stacks() {
    for s in [
        "no-weight",
        "a -1",
        "a 1.5",
        "a 18446744073709551616",
        "a 18446744073709551615\nb 1",
        "a;;b 1",
    ] {
        assert!(folded::inspect(s, 10).is_err(), "{s}");
    }
    assert!(folded::inspect(&format!("{} 1", vec!["a"; 257].join(";")), 10).is_err());
}
#[test]
fn inputs_are_bounded_before_interpretation() {
    assert!(inspect(&vec![b' '; MAX_BYTES + 1], Format::ChromeTrace, 10).is_err());
    assert!(
        parsed(
            json!(vec![json!({"ph":"i"}); MAX_RECORDS + 1]),
            Format::ChromeTrace,
            10
        )
        .is_err()
    );
    assert!(folded::inspect(&"a 1\n".repeat(MAX_RECORDS + 1), 10).is_err());
    assert!(inspect(b"{", Format::OtlpJson, 10).is_err());
}
#[test]
fn publishes_representation_and_snapshot_without_retaining_original() {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    std::fs::write(work.path().join("cpu.folded"), "root;leaf 7").unwrap();
    let out = run(
        &ws,
        &json!({"action":"inspect","path":"cpu.folded","format":"folded_stacks","limit":1}),
    )
    .unwrap();
    let data = out.data.unwrap();
    let reference = data["evidence_references"][0].as_str().unwrap();
    let record = ws.artifacts().unwrap().evidence_record(reference).unwrap();
    assert!(
        record
            .evidence
            .context
            .snapshot
            .as_ref()
            .unwrap()
            .artifact
            .is_none()
    );
    assert_eq!(record.evidence.context.snapshot.as_ref().unwrap().bytes, 11);
    assert_eq!(
        record.evidence.context.coverage.as_ref().unwrap().extent,
        CoverageExtent::Partial
    );
    let retained = ws
        .artifacts()
        .unwrap()
        .evidence_bytes(&record, 1024 * 1024)
        .unwrap();
    let retained: Value = serde_json::from_slice(&retained).unwrap();
    assert_eq!(retained["format"], "folded_stacks");
    assert_eq!(retained["verification_credit"], false);
    assert_eq!(retained["coverage"]["rows_omitted"], 1);
}
#[test]
fn public_arguments_require_explicit_format_and_valid_limit() {
    let (_work, ws) = testkit::workspace();
    for args in [
        json!({"action":"inspect","path":"x"}),
        json!({"action":"inspect","path":"x","format":"auto"}),
        json!({"action":"inspect","path":"x","format":"folded_stacks","limit":0}),
        json!({"action":"inspect","path":"x","format":"folded_stacks","limit":201}),
        json!({"action":"profile","path":"x","format":"folded_stacks"}),
    ] {
        assert!(run(&ws, &args).is_err());
    }
}
