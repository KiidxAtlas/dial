//! Remove bookkeeping without silently accepting unsupported model-message fields.
use serde_json::Value;

/// Evidence pointers belong to the harness, not the chat template. Keep all other fields:
/// validation must still reject malformed messages and unsupported image-bearing examples.
pub(crate) fn canonical(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(object) = message.as_object_mut() { object.remove("dial_evidence"); object.remove("dial_recall"); }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn removes_only_bookkeeping_and_preserves_evidence_text() {
        let original = json!({"role":"tool","content":"answer [artifact://proof]","tool_call_id":"1",
            "dial_evidence":{"artifact":"artifact://proof"},"unknown":"reject me"});
        let projected = canonical(&original);
        assert!(projected.get("dial_evidence").is_none());
        assert_eq!(projected["content"], original["content"]);
        assert_eq!(projected["unknown"], "reject me");
        assert!(original.get("dial_evidence").is_some());
    }
}
