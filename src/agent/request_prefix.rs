//! Cache identity at the provider-neutral request boundary, after hooks.
//! Hashes only: no extra retained transcript or claims about provider wire/token cache hits.
use crate::llm::Chat;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(super) struct Tracker {
    model: String,
    system: Vec<u8>,
    tools: Vec<u8>,
    messages: Vec<(Vec<u8>, usize)>,
}

#[derive(Debug, Serialize)]
pub(super) struct Observation {
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
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn append_reuses_prefix_and_changes_explain_invalidation() {
        let mut tracker = Tracker::default();
        let mut chat = Chat {
            messages: vec![
                json!({"role":"system","content":"fixed"}),
                json!({"role":"user","content":"hello"}),
            ],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 100,
            seed: None,
        };
        assert_eq!(tracker.observe("a", &chat).invalidation, "initial");
        chat.messages
            .push(json!({"role":"assistant","content":"hello"}));
        let observation = tracker.observe("a", &chat);
        assert_eq!(observation.invalidation, "none");
        assert!(observation.stable_message_bytes > 0);
        chat.messages[1]["content"] = json!("summary");
        assert_eq!(tracker.observe("a", &chat).invalidation, "history_changed");
        chat.tools.push(json!({"type":"function"}));
        assert_eq!(tracker.observe("a", &chat).invalidation, "tools_changed");
        assert_eq!(tracker.observe("b", &chat).invalidation, "model_changed");
        chat.messages[0]["content"] = json!("new instructions");
        assert_eq!(tracker.observe("b", &chat).invalidation, "system_changed");
    }
}
