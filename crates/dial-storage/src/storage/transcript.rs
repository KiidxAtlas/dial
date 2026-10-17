//! Transcript shape rules shared by storage and the agent: which messages count as conversation
//! history, and whether a prefix ends on a complete tool-call group.
use serde_json::Value;

pub fn is_history(message: &Value) -> bool {
    !matches!(message["role"].as_str(), Some("system" | "developer"))
        && message["dial_transient"] != true
        && message["dial_portable_rules"] != true
}

pub fn complete_boundary(messages: &[Value], at: usize) -> bool {
    let Some(prefix) = messages.get(..at) else {
        return false;
    };
    let mut pending = std::collections::BTreeSet::new();
    for message in prefix.iter().filter(|m| is_history(m)) {
        if message["role"] == "assistant" {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                if let Some(id) = call["id"].as_str() {
                    pending.insert(id.to_owned());
                }
            }
        } else if message["role"] == "tool"
            && let Some(id) = message["tool_call_id"].as_str()
        {
            pending.remove(id);
        }
    }
    pending.is_empty()
}
