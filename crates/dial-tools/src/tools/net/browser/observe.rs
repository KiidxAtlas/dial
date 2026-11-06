//! Opt-in, bounded post-action observations. Execution reuses the native driver owners.
use serde_json::{Value, json};

pub(super) const ACTIONS: &[&str] = &[
    "open",
    "navigate",
    "back",
    "forward",
    "reload",
    "click",
    "dblclick",
    "fill",
    "type",
    "press",
    "select",
    "check",
    "uncheck",
    "hover",
    "focus",
    "upload",
    "scroll",
    "drag",
    "click_at",
    "mouse_move",
    "mouse_down",
    "mouse_up",
    "wheel",
    "key_down",
    "key_up",
    "clipboard_paste",
];

pub(super) fn schema() -> Value {
    json!({"type":"object","additionalProperties":false,
    "description":"Opt-in observations after successful navigation, form or input actions; no action replay on observation failure. snapshot/screenshot/events default false. Screenshot uses the native image channel; events contains only new console/network failures. Not supported by cmux.",
    "properties":{
        "snapshot":{"type":"boolean"},"screenshot":{"type":"boolean"},"events":{"type":"boolean"},
        "max_nodes":{"type":"integer","minimum":1,"maximum":1000,"description":"Snapshot node limit; default 300. Observations have a bounded 3-second budget within the remaining action deadline."}
    }})
}

pub(super) fn validate(action: &str, args: &Value) -> Result<(), String> {
    let Some(value) = args.get("observe") else {
        return Ok(());
    };
    if !ACTIONS.contains(&action) {
        return Err(format!("observe is not supported for action={action}"));
    }
    let object = value.as_object().ok_or("observe must be an object")?;
    for (key, value) in object {
        match key.as_str() {
            "snapshot" | "screenshot" | "events" if value.is_boolean() => {}
            "max_nodes" if value.as_u64().is_some_and(|n| (1..=1000).contains(&n)) => {}
            _ => return Err(format!("invalid observe option {key:?}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
