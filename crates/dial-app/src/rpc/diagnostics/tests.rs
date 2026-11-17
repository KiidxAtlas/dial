use super::*;

#[test]
fn recorded_check_rows_are_redacted_and_keep_their_shape() {
    let secret = "sk-test-0123456789abcdef";
    let rows = json!([{
        "name": "fmt",
        "status": "failed",
        "evidence_ref": "artifact://abc",
        "note": format!("Authorization: Bearer {secret}"),
        "nested": {"tail": format!("Authorization: Bearer {secret}")},
    }]);
    let redacted = redact_value(rows.clone());
    let text = redacted.to_string();
    assert!(!text.contains(secret), "{text}");
    assert_eq!(redacted[0]["name"], "fmt");
    assert_eq!(redacted[0]["status"], "failed");
    assert_eq!(redacted[0]["evidence_ref"], "artifact://abc");
    assert!(redacted[0]["note"].is_string() && redacted[0]["nested"]["tail"].is_string());
    assert_eq!(redact_value(json!(7)), json!(7));
}
