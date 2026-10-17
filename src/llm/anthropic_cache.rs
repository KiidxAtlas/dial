//! Native Messages prompt caching. Breakpoints cover reusable prefixes, never request-local guidance.
use super::profiles::{CacheRetention, ModelProfile};
use serde_json::{Value, json};

/// Apply after schema/profile transforms and before OAuth body attestation.
/// Native-compatible endpoints can opt out with prompt_cache=false.
pub(super) fn apply(body: &mut Value, profile: Option<&ModelProfile>, official_oauth: bool) {
    let enabled = profile.and_then(|p| p.prompt_cache).unwrap_or(true);
    let retention = profile.map(|p| p.cache_retention).unwrap_or_default();
    let long =
        retention == CacheRetention::Long || (retention == CacheRetention::Auto && official_oauth);
    let control = if long {
        json!({"type":"ephemeral", "ttl":"1h"})
    } else {
        json!({"type":"ephemeral"})
    };

    // Native tool order must not depend on registry insertion order. Do not retain withdrawn
    // tools: exposing them would contradict approval/recovery and tool-loading contracts.
    // Mutable indexing inserts null for absent keys. Tool-free compaction requests must
    // retain absent optional fields instead of sending invalid tools/system null values.
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        if enabled && let Some(last) = tools.last_mut() {
            last["cache_control"] = control.clone();
        }
    }
    if enabled {
        if let Some(text) = body["system"].as_str().map(str::to_owned) {
            body["system"] = json!([{"type":"text", "text":text, "cache_control":control}]);
        } else if let Some(blocks) = body.get_mut("system").and_then(Value::as_array_mut) {
            mark_last(blocks, &control);
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        let mut stable_end = messages.len();
        for (index, message) in messages.iter_mut().enumerate() {
            if message
                .as_object_mut()
                .is_some_and(|m| m.remove("dial_transient").and_then(|v| v.as_bool()) == Some(true))
            {
                stable_end = stable_end.min(index);
            }
        }
        if enabled {
            // Two head anchors + at most two rolling history anchors obey the four-marker limit.
            // The previous tail remains discoverable as the conversation grows. Never mark generated
            // thinking/redacted_thinking, and never place an anchor beyond a transient suffix.
            let mut marked = 0;
            for message in messages[..stable_end].iter_mut().rev() {
                let cached = if let Some(text) = message["content"].as_str().map(str::to_owned) {
                    message["content"] =
                        json!([{"type":"text","text":text,"cache_control":control.clone()}]);
                    true
                } else {
                    message
                        .get_mut("content")
                        .and_then(Value::as_array_mut)
                        .is_some_and(|blocks| mark_last(blocks, &control))
                };
                if cached {
                    marked += 1;
                    if marked == 2 {
                        break;
                    }
                }
            }
        }
    }
}

fn mark_last(blocks: &mut [Value], control: &Value) -> bool {
    for block in blocks.iter_mut().rev() {
        if matches!(
            block["type"].as_str(),
            Some("text" | "image" | "tool_use" | "tool_result")
        ) {
            block["cache_control"] = control.clone();
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests;
