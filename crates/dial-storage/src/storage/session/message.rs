//! Message and control-entry helpers shared by the journal, the view and the search index.

use super::*;

/// Message key of the experimental turn optimizer. A drafted user message keeps
/// `{"original": <typed text>}` here, while `content` holds the prompt the main model received.
/// Like every `dial_*` key it never reaches a provider.
pub const TURN_DRAFT_KEY: &str = "dial_turn_draft";

/// The text the user typed for a message the turn drafter rewrote; `None` for any other message.
pub fn typed_text(message: &Value) -> Option<&str> {
    message.get(TURN_DRAFT_KEY)?.get("original")?.as_str()
}

/// Display and search text: what the user typed, not the drafted prompt sent in its place.
pub(super) fn content_text(message: &Value) -> String {
    if let Some(typed) = typed_text(message) {
        return typed.to_owned();
    }
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Fold one `dial_state` entry into the merged control state: `state` replaces it wholesale,
/// `patch` overwrites the named top-level keys and removes the ones set to null.
pub(super) fn apply_control_entry(control: &mut Value, message: &Value) {
    if let Some(patch) = message["patch"].as_object() {
        if !control.is_object() {
            *control = json!({});
        }
        let object = control.as_object_mut().expect("control state is an object");
        for (key, value) in patch {
            if value.is_null() {
                object.remove(key);
            } else {
                object.insert(key.clone(), value.clone());
            }
        }
    } else {
        *control = message["state"].clone();
    }
}

/// A top-level field a `dial_state` entry sets, whether it is a full snapshot or a patch.
/// A patch omits unchanged keys, so `None` means "not set by this entry".
pub(super) fn control_entry_field<'a>(message: &'a Value, key: &str) -> Option<&'a Value> {
    message["patch"]
        .get(key)
        .or_else(|| message["state"].get(key))
        .filter(|value| !value.is_null())
}

pub(super) fn role_of(message: &Value) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or("")
}

/// Shared exclusion rules for original conversation records, not synthetic context.
pub(super) fn is_conversation_message(message: &Value) -> bool {
    matches!(role_of(message), "user" | "assistant") && is_original_record(message)
}

/// Records the user or model wrote or saw, not synthetic context or transient controls.
pub(super) fn is_original_record(message: &Value) -> bool {
    message["dial_transient"] != true
        && message["dial_portable_rules"] != true
        && message["dial_context_notes"] != true
        && message["dial_context_rollover"] != true
}

pub(super) fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

// ---------------------------------------------------------------- entry (de)serialisation

/// Collect object-field and array-element strings in deterministic traversal order.
pub(super) fn collect_strings<'a>(
    value: &'a Value,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, &'a str)>,
) {
    match value {
        Value::String(text) => out.push((path.clone(), text)),
        Value::Object(object) => {
            for (key, child) in object {
                path.push(key.clone());
                collect_strings(child, path, out);
                path.pop();
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                path.push(index.to_string());
                collect_strings(child, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}
pub(super) fn set_string(value: &mut Value, path: &[String], replacement: String) {
    if path.is_empty() {
        *value = Value::String(replacement);
        return;
    }
    match value {
        Value::Object(object) => {
            if let Some(next) = object.get_mut(&path[0]) {
                set_string(next, &path[1..], replacement);
            }
        }
        Value::Array(array) => {
            if let Ok(index) = path[0].parse::<usize>()
                && let Some(next) = array.get_mut(index)
            {
                set_string(next, &path[1..], replacement);
            }
        }
        _ => {}
    }
}
