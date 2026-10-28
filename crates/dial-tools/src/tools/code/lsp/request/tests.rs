use super::*;

const URI: &str = "file:///project/src/lib.rs";

#[test]
fn a_missing_or_malformed_method_is_rejected_before_any_server_starts() {
    assert!(prepare(None, None, URI, None).is_err());
    assert!(prepare(Some("   "), None, URI, None).is_err());
    assert!(prepare(Some("textDocument/hover?"), None, URI, None).is_err());
    let long = "a/".repeat(MAX_METHOD_CHARS);
    assert!(prepare(Some(&long), None, URI, None).is_err());
}

#[test]
fn lifecycle_notifications_and_dedicated_actions_are_refused_with_a_pointer_to_them() {
    for method in [
        "initialize",
        "shutdown",
        "$/cancelRequest",
        "textDocument/didOpen",
        "workspace/didChangeConfiguration",
        "workspace/executeCommand",
        "workspace/applyEdit",
        "codeAction/resolve",
        "window/showMessageRequest",
        "client/registerCapability",
        "workspace/diagnostic/refresh",
    ] {
        let error = prepare(Some(method), None, URI, None).unwrap_err();
        assert!(error.contains("refused"), "{method}: {error}");
    }
}

#[test]
fn known_methods_carry_their_capability_pointer_and_unknown_ones_carry_none() {
    let known = prepare(Some("textDocument/inlayHint"), None, URI, None).unwrap();
    assert_eq!(known.pointer, Some("/inlayHintProvider"));
    let vendor = prepare(Some("rust-analyzer/expandMacro"), None, URI, None).unwrap();
    assert_eq!(vendor.pointer, None);
    assert_eq!(vendor.method, "rust-analyzer/expandMacro");
}

#[test]
fn defaults_fill_only_fields_the_payload_leaves_out() {
    let payload = json!({"textDocument": {"uri": "file:///other.rs"}});
    let call = prepare(
        Some("textDocument/hover"),
        Some(&payload),
        URI,
        Some((3, 7)),
    )
    .unwrap();
    assert_eq!(call.params["textDocument"]["uri"], "file:///other.rs");
    assert_eq!(call.params["position"], json!({"line": 3, "character": 7}));
}

#[test]
fn no_position_is_injected_without_a_line() {
    let call = prepare(Some("textDocument/documentSymbol"), None, URI, None).unwrap();
    assert_eq!(call.params, json!({"textDocument": {"uri": URI}}));
}

#[test]
fn payload_must_be_an_object_and_absent_payload_is_empty() {
    let error = prepare(Some("workspace/symbol"), Some(&json!("x")), URI, None).unwrap_err();
    assert!(error.contains("JSON object"));
    let call = prepare(Some("workspace/symbol"), Some(&Value::Null), URI, None).unwrap();
    assert_eq!(call.params, json!({"textDocument": {"uri": URI}}));
}

#[test]
fn oversized_payloads_are_refused() {
    let big = json!({"blob": "x".repeat(MAX_PAYLOAD_BYTES)});
    let error = prepare(Some("workspace/symbol"), Some(&big), URI, None).unwrap_err();
    assert!(error.contains("larger than"));
}

#[test]
fn failures_echo_a_bounded_params_preview() {
    let payload = json!({"query": "y".repeat(MAX_PREVIEW_CHARS * 2)});
    let call = prepare(Some("workspace/symbol"), Some(&payload), URI, None).unwrap();
    let text = failure("rust-analyzer", &call, "boom");
    assert!(text.starts_with("LSP error from rust-analyzer on workspace/symbol: boom"));
    assert!(text.contains('…'));
    assert!(text.chars().count() < MAX_PREVIEW_CHARS * 2);
}

#[test]
fn results_are_pretty_json_and_truncation_is_reported() {
    assert!(result_text(&json!({"a": 1})).contains("\"a\": 1"));
    let huge = json!({"s": "z".repeat(MAX_RESULT_CHARS * 2)});
    let text = result_text(&huge);
    assert!(text.contains("[truncated:"));
}

#[test]
fn control_characters_in_results_are_escaped_not_emitted() {
    let text = result_text(&json!({"s": "\u{1b}[31mred"}));
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn workspace_edits_are_found_in_a_result_or_in_code_action_edits() {
    let rename = json!({"changes": {URI: []}});
    assert_eq!(workspace_edits(&rename).len(), 1);
    let actions = json!([
        {"title": "fix", "edit": {"documentChanges": []}},
        {"title": "run", "command": "x"},
        {"title": "other", "edit": {"notAnEdit": true}},
    ]);
    assert_eq!(workspace_edits(&actions).len(), 1);
    assert!(workspace_edits(&json!({"contents": "hover"})).is_empty());
}

#[test]
fn experimental_capabilities_name_a_method_as_a_string_list_or_key() {
    let method = "rust-analyzer/expandMacro";
    assert!(names_method(&json!(method), method));
    assert!(names_method(&json!([method, "other/x"]), method));
    assert!(names_method(&json!({method: true}), method));
    assert!(!names_method(&json!({method: false}), method));
    assert!(!names_method(&json!({"other/x": true}), method));
    assert!(!names_method(&Value::Null, method));
}
