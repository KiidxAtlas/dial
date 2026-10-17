use super::*;

#[test]
fn run_code_defaults_timeout_and_clamps() {
    let request = parse_run(&json!({"code": "  return 1; "})).unwrap();
    assert_eq!(request.source, RunSource::Code("return 1;".into()));
    assert_eq!(request.timeout_ms, RUN_TIMEOUT_DEFAULT_SECS * 1000);

    let high = parse_run(&json!({"code": "return 1", "timeout": 99999})).unwrap();
    assert_eq!(high.timeout_ms, RUN_TIMEOUT_MAX_SECS * 1000);
    let low = parse_run(&json!({"code": "return 1", "timeout": 0})).unwrap();
    assert_eq!(low.timeout_ms, RUN_TIMEOUT_MIN_SECS * 1000);
}

#[test]
fn run_requires_exactly_one_source() {
    assert!(parse_run(&json!({})).is_err());
    assert!(parse_run(&json!({"code": "return 1", "fn": "() => 1"})).is_err());
    assert!(parse_run(&json!({"code": "   ", "fn": ""})).is_err());
    assert!(parse_run(&json!({"code": "return 1", "args": [1]})).is_err());
}

#[test]
fn run_fn_renders_awaited_call() {
    let request = parse_run(&json!({"fn": "(a, b) => a + b", "args": [1, "x"]})).unwrap();
    assert_eq!(
        request.code(),
        "return await (\n(a, b) => a + b\n)(...[1,\"x\"]);"
    );
}

#[test]
fn run_enforces_source_cap() {
    let big = "x".repeat(RUN_SOURCE_MAX_BYTES + 1);
    assert!(parse_run(&json!({ "code": big })).is_err());
}

#[test]
fn call_rejects_unknown_method_and_bad_shapes() {
    let error = parse_call(&json!({"chain": [{"method": "eval"}]})).unwrap_err();
    assert!(error.contains("unknown call method \"eval\""));
    assert!(parse_call(&json!({"chain": []})).is_err());
    assert!(parse_call(&json!({})).is_err());
    assert!(parse_call(&json!({"chain": [{"method": "click", "args": "x"}]})).is_err());
    let long: Vec<Value> = (0..=RUN_CALL_MAX_STEPS)
        .map(|_| json!({"method": "url"}))
        .collect();
    assert!(parse_call(&json!({ "chain": long })).is_err());
}

#[test]
fn call_parses_steps_in_order() {
    let request = parse_call(&json!({
        "chain": [
            {"method": "goto", "args": ["https://example.com"]},
            {"method": "click", "args": ["#go"]}
        ],
        "timeout": 5
    }))
    .unwrap();
    assert_eq!(request.timeout_ms, 5000);
    assert_eq!(request.chain[0].method, "goto");
    assert_eq!(request.chain[1].args, vec![json!("#go")]);
}

#[test]
fn shape_output_caps_and_states_limit() {
    let driver = json!({"displays": ["hello\n"], "value": "y".repeat(200)});
    let shaped = shape_output(&driver, 64);
    assert_eq!(shaped["truncated"], json!(true));
    assert_eq!(shaped["cap_bytes"], json!(64));
    let text = shaped["text"].as_str().unwrap();
    assert!(text.starts_with("hello\n"));
    assert!(text.contains("inline cap is 64 bytes"));
}

#[test]
fn shape_output_passes_small_values_through() {
    let shaped = shape_output(&json!({"value": {"a": 1}}), RUN_OUTPUT_MAX_BYTES);
    assert_eq!(shaped["truncated"], json!(false));
    assert_eq!(shaped["text"], json!("{\n  \"a\": 1\n}"));
}
