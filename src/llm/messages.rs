//! Remove bookkeeping without silently accepting unsupported model-message fields.
use serde_json::Value;

/// Evidence pointers belong to the harness, not the chat template. Keep all other fields:
/// validation must still reject malformed messages and unsupported image-bearing examples.
pub(crate) fn canonical(message: &Value) -> Value {
    let mut message = message.clone();
    canonicalize(&mut message);
    message
}

/// Strip harness bookkeeping from an already-owned message without copying its payload.
pub(crate) fn canonicalize(message: &mut Value) {
    if let Some(object) = message.as_object_mut() {
        object.remove("dial_evidence");
        object.remove("dial_recall");
        object.remove("dial_transient");
        object.remove("dial_generated_user");
        object.remove("dial_context_request");
        object.remove("dial_prompt_band");
        object.remove("dial_tool_contracts");
        object.remove("dial_execution_status");
        object.remove("dial_effects");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn removes_only_bookkeeping_and_preserves_evidence_text() {
        let original = json!({"role":"tool","content":"answer [artifact://proof]","tool_call_id":"1",
            "dial_evidence":{"artifact":"artifact://proof"},"dial_recall":"notes",
            "dial_transient":true,"unknown":"reject me",
            "dial_images":[{"url":"artifact://image"}],
            "dial_provider_content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]});
        let projected = canonical(&original);
        assert!(projected.get("dial_evidence").is_none());
        assert!(projected.get("dial_transient").is_none());
        assert!(projected.get("dial_recall").is_none());
        assert_eq!(projected["content"], original["content"]);
        assert_eq!(projected["unknown"], "reject me");
        assert_eq!(projected["dial_images"], original["dial_images"]);
        assert_eq!(
            projected["dial_provider_content"],
            original["dial_provider_content"]
        );
        assert!(original.get("dial_evidence").is_some());
    }
}
