//! Cache identity at the provider-neutral request boundary, after hooks.
//! Hashes only: no extra retained transcript or claims about provider wire/token cache hits.
use crate::llm::Chat;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(in crate::agent) struct Tracker {
    /// Model of the previous request; `None` until the first one is observed.
    model: Option<String>,
    system: [u8; 32],
    tools: [u8; 32],
    messages: Vec<([u8; 32], usize)>,
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
    pub stable_message_count: usize,
    /// Full serialized messages outside the reusable leading prefix, not provider cache misses.
    pub changed_message_bytes: usize,
    pub system_bytes: usize,
    /// Serialized tools array, including JSON separators; not a token estimate.
    pub tool_schema_bytes: usize,
    pub tool_count: usize,
    pub request_message_bytes: usize,
}

/// Digest and serialized length using request-local scratch space. The buffer is never retained
/// by the tracker: projected/hook-rewritten messages are measured afresh at every boundary.
fn digest(value: &impl Serialize, scratch: &mut Vec<u8>) -> ([u8; 32], usize) {
    scratch.clear();
    // Only JSON values (string-keyed maps) are digested; serializing them cannot fail.
    serde_json::to_writer(&mut *scratch, value).expect("JSON value serialization cannot fail");
    (Sha256::digest(&*scratch).into(), scratch.len())
}

impl Tracker {
    /// Runs once per request on the whole history. Reuse serialization capacity across values,
    /// not fingerprints across requests: projection and hooks may change any historical message.
    pub fn observe(&mut self, model: &str, request: &Chat) -> Observation {
        let mut scratch = Vec::new();
        let messages: Vec<_> = request
            .messages
            .iter()
            .map(|value| digest(value, &mut scratch))
            .collect();
        let mut system = Sha256::new();
        let mut system_bytes = 0;
        for (message, (hash, bytes)) in request.messages.iter().zip(&messages) {
            if matches!(message["role"].as_str(), Some("system" | "developer")) {
                system.update(hash);
                system_bytes += bytes;
            }
        }
        let system = system.finalize().into();
        let (tools, tool_schema_bytes) = digest(&request.tools, &mut scratch);
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
        let request_message_bytes = messages.iter().map(|m| m.1).sum::<usize>();
        let observation = Observation {
            boundary: "provider_neutral_after_hooks",
            invalidation,
            stable_message_bytes: stable,
            request_message_bytes,
            stable_message_count: if invalidation.keeps_message_prefix() {
                common
            } else {
                0
            },
            changed_message_bytes: request_message_bytes - stable,
            system_bytes,
            tool_schema_bytes,
            tool_count: request.tools.len(),
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
