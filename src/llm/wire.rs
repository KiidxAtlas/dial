//! Fingerprints of final outbound payloads. Never retains prompt text, media or credentials.
//! Common bytes describe serialized prompt parts, not provider tokenization or cache hits.
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{cell::RefCell, sync::Arc};

#[derive(Clone, Debug, Serialize)]
pub struct Observation {
    pub boundary: &'static str,
    pub transport: &'static str,
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
    fn observe(&mut self, bytes: &[u8], transport: &'static str) {
        let Ok(body) = serde_json::from_slice::<Value>(bytes) else {
            return;
        };
        // Preserve provider-specific message/block order. Fixed system and tools precede history.
        let body = body.get("request").unwrap_or(&body);
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
                let encoded = serde_json::to_vec(&(key, value)).expect("JSON value serialization");
                parts.push((Sha256::digest(&encoded).into(), encoded.len()));
            }
        }
        let same_model = self.model == body["model"] && self.transport == Some(transport);
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
            payload_bytes: bytes.len(),
            payload_sha256: format!("{:x}", Sha256::digest(bytes)),
            common_prompt_bytes: parts[..common].iter().map(|p| p.1).sum(),
            prompt_bytes: parts.iter().map(|p| p.1).sum(),
            invalidation: if self.transport.is_none() {
                "initial"
            } else if !same_model {
                "model_or_transport_changed"
            } else if common < self.parts.len() {
                "prompt_changed"
            } else {
                "none"
            },
        });
        self.model = body["model"].clone();
        self.transport = Some(transport);
        self.parts = parts;
    }
}
pub(crate) fn record(bytes: &[u8], transport: &'static str) {
    ACTIVE.with(|active| {
        if let Some(tracker) = active.borrow().as_ref() {
            tracker.lock().observe(bytes, transport);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn wire_changes_include_images_tools_and_signed_bytes_without_retaining_content() {
        let mut tracker = Tracker::default();
        let mut body = json!({"model":"fixture", "messages":[{"role":"system","content":"fixed"},{"role":"user","content":[{"image_url":{"url":"data:image/png;base64,AAAA"}}]}],"tools":[]});
        tracker.observe(&serde_json::to_vec(&body).unwrap(), "http");
        assert_eq!(tracker.latest().unwrap().invalidation, "initial");
        tracker.observe(&serde_json::to_vec_pretty(&body).unwrap(), "http");
        let same = tracker.latest().unwrap();
        assert_eq!(same.common_prompt_bytes, same.prompt_bytes);
        body["messages"][1]["content"][0]["image_url"]["url"] = json!("data:image/png;base64,BBBB");
        tracker.observe(&serde_json::to_vec(&body).unwrap(), "http");
        let changed = tracker.latest().unwrap();
        assert_eq!(changed.invalidation, "prompt_changed");
        assert!(
            changed.common_prompt_bytes > 0 && changed.common_prompt_bytes < changed.prompt_bytes
        );
        assert!(!serde_json::to_string(&changed).unwrap().contains("BBBB"));
    }
    #[test]
    fn nested_scopes_restore_owner() {
        let outer = Shared::default();
        let inner = Shared::default();
        let _outer = Scope::enter(outer.clone());
        {
            let _inner = Scope::enter(inner.clone());
            record(b"{}", "http");
        }
        assert!(outer.lock().latest().is_none());
        record(b"{}", "http");
        assert!(outer.lock().latest().is_some() && inner.lock().latest().is_some());
    }
}
