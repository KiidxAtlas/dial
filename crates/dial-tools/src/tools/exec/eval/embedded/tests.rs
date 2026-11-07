use super::{completion_source, serve};

#[test]
fn portable_helpers_work_without_node_or_python_and_keep_partial_batch_receipts() {
    let frames = drive(&[
        json!({"id":0,"code":include_str!("../portable.js")}),
        json!({"id":1,"code":"Array.from(await dial.readBytes('input.bin'))"}),
        json!({"callback_id":1,"operation_id":1,"result":{"is_error":false,"text":"receipt","data":{"content":"AP8NCg=="}}}),
        json!({"id":2,"code":"await dial.batch([{name:'read'}]); await dial.writeText('unstarted','bad')"}),
        json!({"callback_id":2,"operation_id":2,"result":{"is_error":false,"text":"batch","data":[{"is_error":true,"text":"denied","status":"blocked","effects":"none"}]}}),
    ]);
    let results: Vec<_> = frames
        .iter()
        .filter_map(|frame| frame.get("result"))
        .collect();
    assert_eq!(results[0]["error"], Value::Null);
    assert!(
        results[1]["value"]
            .as_str()
            .unwrap()
            .contains("0, 255, 13, 10"),
        "{results:?}"
    );
    assert!(
        results[2]["error"]
            .as_str()
            .unwrap()
            .contains("prerequisites failed")
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.get("callback").is_some())
            .count(),
        2
    );
}

#[test]
fn embedded_binary_helpers_preserve_endian_and_full_width_integers() {
    let source = format!(
        "{}\n{}\ndial.unpack('>Qh', dial.pack('>Qh', [18446744073709551615n, -2])).map(String).join(',')",
        include_str!("../binary.js"),
        include_str!("../portable.js")
    );
    let result = cell(&source);
    assert_eq!(result["error"], Value::Null, "{result}");
    assert!(
        result["value"]
            .as_str()
            .unwrap()
            .contains("18446744073709551615,-2"),
        "{result}"
    );
}
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::io::{self, Cursor, Write};
use std::sync::Arc;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Feeds newline-delimited controller frames and returns every frame the worker wrote.
fn drive(frames: &[Value]) -> Vec<Value> {
    let mut input = String::new();
    for frame in frames {
        input.push_str(&frame.to_string());
        input.push('\n');
    }
    let sink = Sink::default();
    serve(
        Box::new(Cursor::new(input.into_bytes())),
        Box::new(sink.clone()),
    )
    .expect("serve");
    let bytes = sink.0.lock().clone();
    String::from_utf8(bytes)
        .expect("utf8 frames")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json frame"))
        .collect()
}

/// Runs one cell with no callbacks and returns its result object.
fn cell(code: &str) -> Value {
    let frames = drive(&[json!({"id": 1, "code": code})]);
    frames
        .iter()
        .find(|frame| frame.get("id") == Some(&json!(1)) && frame.get("result").is_some())
        .expect("result frame")["result"]
        .clone()
}

#[test]
fn bigint_arithmetic_is_exact() {
    let result = cell(
        "(2n ** 200n).toString() === '1606938044258990275541962092341162602522202993782792835301376'",
    );
    assert_eq!(result["value"], json!("true"), "{result}");
}

#[test]
fn probe_cell_returns_zero() {
    let result = cell("0");
    assert_eq!(result["value"], json!("0"));
    assert_eq!(result["error"], Value::Null);
}

#[test]
fn state_persists_across_cells() {
    let frames = drive(&[
        json!({"id": 1, "code": "let counter = 40;"}),
        json!({"id": 2, "code": "counter + 2"}),
    ]);
    let second = frames
        .iter()
        .find(|frame| frame.get("id") == Some(&json!(2)))
        .expect("second result")["result"]
        .clone();
    assert_eq!(second["value"], json!("42"));
}

#[test]
fn async_cell_reports_await_completion_value() {
    let result = cell("let x = await Promise.resolve(7); x * 2");
    assert_eq!(result["value"], json!("14"));
    assert_eq!(result["error"], Value::Null);
}

#[test]
fn async_cell_timer_resolves_before_completion() {
    let result = cell("await new Promise(resolve => setTimeout(resolve, 5)); 'done'");
    assert_eq!(result["value"], json!("'done'"));
}

#[test]
fn stuck_top_level_await_reports_error() {
    let result = cell("await new Promise(() => {})");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|text| text.contains("top-level await never settled")),
        "{result}"
    );
}

#[test]
fn console_output_is_captured() {
    let result = cell("console.log('hello', 3); console.error('oops');");
    assert_eq!(result["stdout"], json!("hello 3\n"));
    assert_eq!(result["stderr"], json!("oops\n"));
}

#[test]
fn thrown_error_is_reported_with_name_and_message() {
    let result = cell("throw new TypeError('x')");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|text| text.starts_with("TypeError: x")),
        "{result}"
    );
}

#[test]
fn redeclaration_gets_hint() {
    let frames = drive(&[
        json!({"id": 1, "code": "let dup = 1;"}),
        json!({"id": 2, "code": "let dup = 2;"}),
    ]);
    let second = frames
        .iter()
        .find(|frame| frame.get("id") == Some(&json!(2)))
        .expect("second result")["result"]
        .clone();
    assert!(
        second["error"]
            .as_str()
            .is_some_and(|text| text.contains("dup")),
        "{second}"
    );
}

#[test]
fn embedded_missing_globals_do_not_recommend_unavailable_node_imports() {
    let result = cell("require('fs')");
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("Node modules are unavailable"), "{error}");
    assert!(error.contains("tool.read"), "{error}");
    assert!(!error.contains("await import('node:"), "{error}");
}

#[test]
fn tool_callback_round_trip_returns_data() {
    let frames = drive(&[
        json!({"id": 1, "code": "(await tool.read({path: 'a.txt'})).text"}),
        json!({"callback_id": 1, "operation_id": 1,
               "result": {"is_error": false, "text": "file body", "data": null}}),
    ]);
    let callback = frames
        .iter()
        .find(|frame| frame.get("callback").is_some())
        .expect("callback request");
    assert_eq!(callback["callback"], json!("tool"));
    assert_eq!(callback["name"], json!("read"));
    assert_eq!(callback["arguments"]["path"], json!("a.txt"));
    let result = frames
        .iter()
        .find(|frame| frame.get("id") == Some(&json!(1)) && frame.get("result").is_some())
        .expect("result frame")["result"]
        .clone();
    assert_eq!(result["value"], json!("'file body'"));
}

#[test]
fn completion_source_rewrites_trailing_expression() {
    assert_eq!(
        completion_source("let x = 1; x + 1"),
        "let x = 1; __dial_completion__ = (x + 1)"
    );
}

#[test]
fn completion_source_leaves_statements_and_parse_errors_alone() {
    assert_eq!(completion_source("let x = 1;"), "let x = 1;");
    assert_eq!(completion_source("let x = ;"), "let x = ;");
}
