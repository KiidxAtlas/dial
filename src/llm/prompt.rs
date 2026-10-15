//! Shared prompt stability vocabulary. Only the leading system prefix is ordered; conversation
//! history and request-tail control messages retain their exact chronology.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    Frozen,
    Stable,
    Dynamic,
    Volatile,
}

pub fn system(band: Band, text: impl Into<String>) -> Value {
    json!({"role":"system", "content":text.into(), "dial_prompt_band":band})
}

pub fn band(message: &Value) -> Band {
    if message["dial_transient"] == true {
        return Band::Volatile;
    }
    serde_json::from_value(message["dial_prompt_band"].clone()).unwrap_or(Band::Stable)
}

pub fn order(messages: &mut [Value]) {
    let end = messages
        .iter()
        .take_while(|message| message["role"] == "system" && message["dial_transient"] != true)
        .count();
    messages[..end].sort_by_key(band);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transient_control_messages_are_volatile_and_preserve_their_boundary() {
        let control = json!({"role":"system", "content":"steering", "dial_transient":true});
        assert_eq!(band(&control), Band::Volatile);
        let mut messages = vec![
            system(Band::Dynamic, "memory"),
            control.clone(),
            system(Band::Frozen, "historical policy"),
        ];
        let original = messages.clone();
        order(&mut messages);
        assert_eq!(messages, original);
    }
    #[test]
    fn changing_dynamic_sections_preserves_stable_prefix_and_history_order() {
        let mut messages = vec![
            system(Band::Frozen, "policy"),
            system(Band::Dynamic, "memory"),
            system(Band::Stable, "workspace"),
            json!({"role":"user","content":"hi"}),
            system(Band::Volatile, "tail"),
        ];
        order(&mut messages);
        let prefix = messages[..2].to_vec();
        messages[2]["content"] = json!("new memory");
        order(&mut messages);
        assert_eq!(messages[..2], prefix);
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[4]["content"], "tail");
    }
}
