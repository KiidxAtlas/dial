//! Cache identity at the provider-neutral request boundary, after hooks.
//! Hashes only: no extra retained transcript or claims about provider wire/token cache hits.
use crate::llm::Chat;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(in crate::agent) struct Tracker {
    /// Model of the previous request; `None` until the first one is observed.
    model: Option<String>,
    system: Vec<u8>,
    tools: Vec<u8>,
    messages: Vec<(Vec<u8>, usize)>,
}

/// Why the request prefix could not be reused, by the first differing part in prefix order. The
/// snake_case names are the journaled `request_prefix.invalidation` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::agent) enum Invalidation {
    Initial,
    ModelChanged,
    SystemChanged,
    ToolsChanged,
    HistoryChanged,
    None,
}

impl Invalidation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::ModelChanged => "model_changed",
            Self::SystemChanged => "system_changed",
            Self::ToolsChanged => "tools_changed",
            Self::HistoryChanged => "history_changed",
            Self::None => "none",
        }
    }

    /// Model, system and tools are unchanged, so the common leading messages are reusable.
    fn keeps_message_prefix(self) -> bool {
        matches!(self, Self::None | Self::HistoryChanged)
    }
}

impl std::fmt::Display for Invalidation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Serialize)]
pub(in crate::agent) struct Observation {
    pub boundary: &'static str,
    pub invalidation: Invalidation,
    pub stable_message_bytes: usize,
    pub request_message_bytes: usize,
}

/// Digest and serialized length of one value, from a single serialization.
fn digest(value: &impl Serialize) -> (Vec<u8>, usize) {
    // Only JSON values (string-keyed maps) are digested; serializing them cannot fail.
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
        let invalidation = match self.model.as_deref() {
            None => Invalidation::Initial,
            Some(previous) if previous != model => Invalidation::ModelChanged,
            _ if self.system != system => Invalidation::SystemChanged,
            _ if self.tools != tools => Invalidation::ToolsChanged,
            _ if common < self.messages.len() => Invalidation::HistoryChanged,
            _ => Invalidation::None,
        };
        let stable = if invalidation.keeps_message_prefix() {
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
        self.model = Some(model.to_owned());
        self.system = system;
        self.tools = tools;
        self.messages = messages;
        observation
    }
}

#[cfg(test)]
mod tests;
