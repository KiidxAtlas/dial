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

/// Digest and serialized length of one value, from a single serialization.
fn digest(value: &impl Serialize) -> (Vec<u8>, usize) {
    let bytes = serde_json::to_vec(value).expect("JSON value serialization cannot fail");
    (Sha256::digest(&bytes).to_vec(), bytes.len())
}

impl Tracker {
    /// Runs once per request on the whole history, so each message is serialized once: the
    /// system identity is derived from the system messages' own digests.
    pub fn observe(&mut self, model: &str, request: &Chat) -> Observation {
        let messages: Vec<_> = request.messages.iter().map(digest).collect();
        let mut system = Sha256::new();
        for (message, (hash, _)) in request.messages.iter().zip(&messages) {
            if message["role"] == "system" {
                system.update(hash);
            }
        }
        let system = system.finalize().to_vec();
        let (tools, _) = digest(&request.tools);
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
