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
mod tests;
