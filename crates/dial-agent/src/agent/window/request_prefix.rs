//! Cache identity at the provider-neutral request boundary, after hooks.
//! Hashes only: no extra retained transcript or claims about provider wire/token cache hits.
use crate::llm::Chat;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(in crate::agent) struct Tracker {
    model: String,
    system: Vec<u8>,
    tools: Vec<u8>,
    messages: Vec<(Vec<u8>, usize)>,
}

#[derive(Debug, Serialize)]
pub(in crate::agent) struct Observation {
    pub boundary: &'static str,
    pub invalidation: &'static str,
    pub stable_message_bytes: usize,
    pub request_message_bytes: usize,
}

impl Tracker {
    pub fn observe(&mut self, model: &str, request: &Chat) -> Observation {
        let hash =
            |value: &serde_json::Value| Sha256::digest(value.to_string().as_bytes()).to_vec();
        let system = hash(&serde_json::json!(
            request
                .messages
                .iter()
                .filter(|m| m["role"] == "system")
                .collect::<Vec<_>>()
        ));
        let tools = hash(&serde_json::json!(request.tools));
        let messages: Vec<_> = request
            .messages
            .iter()
            .map(|m| (hash(m), m.to_string().len()))
            .collect();
        let common = self
            .messages
            .iter()
            .zip(&messages)
            .take_while(|(a, b)| a == b)
            .count();
        let invalidation = if self.model.is_empty() {
            "initial"
        } else if self.model != model {
            "model_changed"
        } else if self.system != system {
            "system_changed"
        } else if self.tools != tools {
            "tools_changed"
        } else if common < self.messages.len() {
            "history_changed"
        } else {
            "none"
        };
        let stable = if matches!(invalidation, "none" | "history_changed") {
            messages[..common].iter().map(|m| m.1).sum()
        } else {
            0
        };
        let observation = Observation {
            boundary: "provider_neutral_after_hooks",
            invalidation,
            stable_message_bytes: stable,
            request_message_bytes: messages.iter().map(|m| m.1).sum(),
        };
        self.model = model.into();
        self.system = system;
        self.tools = tools;
        self.messages = messages;
        observation
    }
}

#[cfg(test)]
mod tests;
