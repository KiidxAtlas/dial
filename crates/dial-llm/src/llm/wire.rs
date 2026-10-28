//! Fingerprints of final outbound payloads. Never retains prompt text, media or credentials.
//! Common bytes describe serialized prompt parts, not provider tokenization or cache hits.
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{cell::RefCell, sync::Arc};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Observation {
    pub boundary: &'static str,
    pub transport: &'static str,
    /// Sent as a delta over `previous_response_id`; prompt parts still describe the full prompt.
    pub chained: bool,
    pub payload_bytes: usize,
    pub payload_sha256: String,
    pub common_prompt_bytes: usize,
    pub prompt_bytes: usize,
    pub invalidation: &'static str,
}

#[derive(Default)]
pub(crate) struct Tracker {
    model: Value,
    transport: Option<&'static str>,
    parts: Vec<([u8; 32], usize)>,
    latest: Option<Observation>,
}
pub(crate) type Shared = Arc<Mutex<Tracker>>;
thread_local! { static ACTIVE: RefCell<Option<Shared>> = const { RefCell::new(None) }; }
pub(crate) struct Scope(Option<Shared>);
impl Scope {
    pub fn enter(tracker: Shared) -> Self {
        tracker.lock().latest = None;
        Self(ACTIVE.with(|active| active.replace(Some(tracker))))
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.replace(self.0.take()));
    }
}
impl Tracker {
    pub fn latest(&self) -> Option<Observation> {
        self.latest.clone()
    }
    fn observe(&mut self, bytes: &[u8], prompt: Option<&Value>, transport: &'static str) {
        let parsed;
        let body = match prompt {
            Some(prompt) => prompt,
            None => {
                let Ok(body) = serde_json::from_slice::<Value>(bytes) else {
                    return;
                };
                parsed = body;
                &parsed
            }
        };
        // Preserve provider-specific message/block order. Fixed system and tools precede history.
        let body = body.get("request").unwrap_or(body);
        let mut parts = Vec::new();
        for key in [
            "instructions",
            "system",
            "systemInstruction",
            "tools",
            "messages",
            "input",
            "contents",
        ] {
            let Some(value) = body.get(key) else { continue };
            let values: Vec<_> = value
                .as_array()
                .map(|a| a.iter().collect())
                .unwrap_or_else(|| vec![value]);
            for value in values {
                let mut encoded =
                    serde_json::to_vec(&(key, value)).expect("JSON value serialization");
                if key == "system" {
                    reset_attestation(&mut encoded);
                }
                parts.push((Sha256::digest(&encoded).into(), encoded.len()));
            }
        }
        // Transport does not change the prompt; a switch is reported only when the prompt held.
        let same_model = self.model == body["model"];
        let common = if same_model {
            self.parts
                .iter()
                .zip(&parts)
                .take_while(|(a, b)| a == b)
                .count()
        } else {
            0
        };
        self.latest = Some(Observation {
            boundary: "final_provider_wire",
            transport,
            chained: prompt.is_some(),
            payload_bytes: bytes.len(),
            payload_sha256: format!("{:x}", Sha256::digest(bytes)),
            common_prompt_bytes: parts[..common].iter().map(|p| p.1).sum(),
            prompt_bytes: parts.iter().map(|p| p.1).sum(),
            invalidation: if self.transport.is_none() {
                "initial"
            } else if !same_model {
                "model_changed"
            } else if common < self.parts.len() {
                "prompt_changed"
            } else if self.transport != Some(transport) {
                "transport_changed"
            } else {
                "none"
            },
        });
        self.model = body["model"].clone();
        self.transport = Some(transport);
        self.parts = parts;
    }
}

/// The Claude billing header carries `cch=`, a hash of the whole request body, so it differs on
/// every request by construction. Restore its placeholder before fingerprinting the part, or the
/// first system block reports a prompt change that never reached the provider's prefix cache.
fn reset_attestation(encoded: &mut [u8]) {
    use super::provider::{CCH_PLACEHOLDER, CLAUDE_BILLING_PREFIX};
    let Some(start) = encoded
        .windows(CLAUDE_BILLING_PREFIX.len())
        .position(|w| w == CLAUDE_BILLING_PREFIX.as_bytes())
    else {
        return;
    };
    let from = start + CLAUDE_BILLING_PREFIX.len();
    let to = (from + 150).min(encoded.len());
    if let Some(at) = encoded[from..to].windows(4).position(|w| w == b"cch=") {
        let digits = from + at + 4;
        if let Some(slot) = encoded.get_mut(digits..digits + CCH_PLACEHOLDER.len() - 4) {
            slot.fill(b'0');
        }
    }
}
pub(crate) fn record(bytes: &[u8], transport: &'static str) {
    record_with_prompt(bytes, None, transport);
}
/// `bytes` went on the wire; `prompt` is the full logical request a chained delta continues.
pub(crate) fn record_with_prompt(bytes: &[u8], prompt: Option<&Value>, transport: &'static str) {
    ACTIVE.with(|active| {
        if let Some(tracker) = active.borrow().as_ref() {
            tracker.lock().observe(bytes, prompt, transport);
        }
    });
}

#[derive(Default)]
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn json_bytes(value: &serde_json::Value) -> usize {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value).expect("JSON value serialization cannot fail");
    counter.0
}

/// Conservative wire cost of one attached image; `dial_images` stores only small JSON pointers.
pub const IMAGE_ESTIMATE_TOKENS: usize = 1_600;

/// One message's wire bytes split by what the provider receives: the portable copy, the
/// provider-native replay, and attached images (each image costs `IMAGE_ESTIMATE_TOKENS` tokens).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WireParts {
    pub portable: usize,
    pub native: usize,
    pub images: usize,
}

impl WireParts {
    /// Bytes the request carries: provider-native replay replaces the portable copy when larger.
    pub fn total(self) -> usize {
        self.portable.max(self.native) + self.images
    }
}

/// Serialized bytes of a message as a provider receives it, split into parts. Harness bookkeeping
/// (`dial_*`) never reaches the wire. Provider-native replay content is sent instead of, not beside,
/// the portable content/tool-call fields, so the larger of the two counts rather than their sum.
pub fn wire_parts(message: &serde_json::Value) -> WireParts {
    let Some(object) = message.as_object() else {
        return WireParts {
            portable: json_bytes(message),
            ..WireParts::default()
        };
    };
    let portable = object
        .iter()
        .filter(|(key, _)| !key.starts_with("dial_"))
        .map(|(key, value)| key.len() + 4 + json_bytes(value))
        .sum::<usize>()
        + 2;
    let native = object
        .get("dial_provider_content")
        .filter(|content| content.as_array().is_some_and(|items| !items.is_empty()))
        .map_or(0, |content| {
            json_bytes(content) + object.get("role").map_or(0, json_bytes) + 24
        });
    let images = object
        .get("dial_images")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    WireParts {
        portable,
        native,
        images: images * IMAGE_ESTIMATE_TOKENS * 4,
    }
}

/// Serialized wire bytes of a message: see [`wire_parts`].
pub fn wire_message_bytes(message: &serde_json::Value) -> usize {
    wire_parts(message).total()
}

pub fn estimate_parts(messages: &[serde_json::Value], tools: &[serde_json::Value]) -> u64 {
    (messages.iter().map(wire_message_bytes).sum::<usize>()
        + tools.iter().map(json_bytes).sum::<usize>())
    .div_ceil(4) as u64
}

pub fn estimate(message: &Value) -> u64 {
    estimate_parts(std::slice::from_ref(message), &[]).max(
        message["dial_compaction_output_tokens"]
            .as_u64()
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests;
