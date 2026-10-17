//! Remove bookkeeping without silently accepting unsupported model-message fields.
use serde_json::Value;

/// Evidence pointers belong to the harness, not the chat template. Keep all other fields:
/// validation must still reject malformed messages and unsupported image-bearing examples.
pub fn canonical(message: &Value) -> Value {
    let mut message = message.clone();
    canonicalize(&mut message);
    message
}

/// Strip harness bookkeeping from an already-owned message without copying its payload.
pub fn canonicalize(message: &mut Value) {
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

/// Local chat templates commonly accept exactly one initial system message. Keep the
/// stable prefix there; like the Anthropic adapter, carry later/request-local guidance
/// as user messages at its original boundary rather than hoisting it over history.
/// This is a wire projection only: the canonical conversation is never rewritten.
pub(crate) fn local(messages: &[Value]) -> anyhow::Result<Vec<Value>> {
    use anyhow::Context;

    let mut projected: Vec<Value> = Vec::with_capacity(messages.len());
    let mut prefix = true;
    for message in messages {
        let instruction = matches!(message["role"].as_str(), Some("system" | "developer"));
        if prefix && instruction && message["dial_transient"] != true {
            let text = message["content"]
                .as_str()
                .context("local system instructions must have text content")?;
            if let Some(first) = projected.first_mut() {
                let content = first["content"].as_str().expect("validated system text");
                first["content"] = Value::String(format!("{content}\n\n{text}"));
            } else {
                let mut first = message.clone();
                first["role"] = Value::String("system".into());
                projected.push(first);
            }
        } else {
            prefix = false;
            let mut message = message.clone();
            if instruction {
                message["role"] = Value::String("user".into());
            }
            projected.push(message);
        }
    }
    Ok(projected)
}

#[cfg(test)]
mod tests;
