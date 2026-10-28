use super::*;

#[test]
fn tool_names_follow_the_webmcp_grammar() {
    assert!(is_valid_name("search.docs-v2_x"));
    assert!(!is_valid_name(""));
    assert!(!is_valid_name("has space"));
    assert!(!is_valid_name(&"a".repeat(129)));
}

#[test]
fn invoke_requires_a_valid_name_and_object_params() {
    assert!(parse("webmcp_invoke", &json!({})).is_err());
    assert!(parse("webmcp_invoke", &json!({"name": "bad name"})).is_err());
    assert!(parse("webmcp_invoke", &json!({"name": "ok", "params": [1]})).is_err());
    let parsed = parse("webmcp_invoke", &json!({"name": "ok"})).unwrap();
    assert_eq!(parsed["params"], json!({}));
}

#[test]
fn events_reject_a_non_integer_cursor() {
    assert!(parse("webmcp_events", &json!({"since": "3"})).is_err());
    let parsed = parse("webmcp_events", &json!({"since": 4, "clear": true})).unwrap();
    assert_eq!(parsed, json!({"since": 4, "clear": true}));
}

#[test]
fn list_omits_schemas_unless_a_name_is_requested() {
    let raw = json!({
        "status": "ready",
        "name": null,
        "tools": [{"name": "find", "description": "Find things", "input_schema_json": "{}"}],
    });
    let shaped = shape_list(&raw);
    assert_eq!(shaped["tools"][0]["name"], "find");
    assert!(shaped["tools"][0].get("input_schema").is_none());
    assert_eq!(shaped["untrusted"], true);

    let filtered = json!({"status": "ready", "name": "find", "tools": raw["tools"].clone()});
    assert_eq!(shape_list(&filtered)["tools"][0]["input_schema"], "{}");
}

#[test]
fn list_caps_the_number_of_tools() {
    let tools: Vec<Value> = (0..WEBMCP_TOOLS_MAX + 4)
        .map(|index| json!({"name": format!("t{index}"), "description": ""}))
        .collect();
    let shaped = shape_list(&json!({"status": "ready", "tools": tools}));
    assert_eq!(shaped["tools"].as_array().unwrap().len(), WEBMCP_TOOLS_MAX);
    assert_eq!(shaped["truncated"], true);
}

#[test]
fn absent_bridge_reports_a_reason_instead_of_an_empty_success() {
    let shaped =
        shape_list(&json!({"status": "unavailable", "detail": "hook not installed", "tools": []}));
    assert_eq!(shaped["status"], "unavailable");
    assert_eq!(shaped["reason"], "hook not installed");
}

#[test]
fn invoke_failure_is_returned_as_untrusted_error_text() {
    let shaped = shape_invoke(&json!({"ok": false, "error": "boom"})).unwrap();
    assert_eq!(shaped["ok"], false);
    assert_eq!(shaped["error"], "boom");
    assert_eq!(shaped["untrusted"], true);
}

#[test]
fn invoke_parses_the_page_result_or_keeps_truncated_json_as_text() {
    let parsed =
        shape_invoke(&json!({"ok": true, "result_json": "{\"n\":1}", "truncated": false})).unwrap();
    assert_eq!(parsed["result"]["n"], 1);
    let truncated =
        shape_invoke(&json!({"ok": true, "result_json": "{\"n\":", "truncated": true})).unwrap();
    assert_eq!(truncated["result"]["truncated_json"], "{\"n\":");
    assert!(
        shape_invoke(&json!({"ok": true, "result_json": "not json", "truncated": false})).is_err()
    );
}

#[test]
fn events_are_shaped_with_their_cursor() {
    let shaped = shape_events(&json!({
        "events": [{"sequence": 2, "type": "registered", "name": "find"}],
        "cursor": 2,
        "truncated": false,
    }));
    assert_eq!(shaped["events"][0]["name"], "find");
    assert_eq!(shaped["cursor"], 2);
    assert_eq!(shaped["untrusted"], true);
}
