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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn wire_changes_include_images_tools_and_signed_bytes_without_retaining_content() {
        let mut tracker = Tracker::default();
        let mut body = json!({"model":"fixture", "messages":[{"role":"system","content":"fixed"},{"role":"user","content":[{"image_url":{"url":"data:image/png;base64,AAAA"}}]}],"tools":[]});
        tracker.observe(&serde_json::to_vec(&body).unwrap(), None, "http");
        assert_eq!(tracker.latest().unwrap().invalidation, "initial");
        tracker.observe(&serde_json::to_vec_pretty(&body).unwrap(), None, "http");
        let same = tracker.latest().unwrap();
        assert_eq!(same.common_prompt_bytes, same.prompt_bytes);
        body["messages"][1]["content"][0]["image_url"]["url"] = json!("data:image/png;base64,BBBB");
        tracker.observe(&serde_json::to_vec(&body).unwrap(), None, "http");
        let changed = tracker.latest().unwrap();
        assert_eq!(changed.invalidation, "prompt_changed");
        assert!(
            changed.common_prompt_bytes > 0 && changed.common_prompt_bytes < changed.prompt_bytes
        );
        assert!(!serde_json::to_string(&changed).unwrap().contains("BBBB"));
    }
    #[test]
    fn transport_switch_and_chained_delta_keep_the_full_prompt_comparison() {
        let mut tracker = Tracker::default();
        let mut body = json!({"model":"m","instructions":"fixed","tools":[],"input":[{"role":"user","content":"a"}]});
        tracker.observe(&serde_json::to_vec(&body).unwrap(), None, "websocket");
        body["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":"b"}));
        // The chained wire carries only the delta, yet the full prompt still extends the last one.
        let delta = json!({"model":"m","previous_response_id":"r1","input":[{"role":"user","content":"b"}]});
        tracker.observe(
            &serde_json::to_vec(&delta).unwrap(),
            Some(&body),
            "websocket",
        );
        let chained = tracker.latest().unwrap();
        assert_eq!((chained.invalidation, chained.chained), ("none", true));
        assert!(chained.payload_bytes < chained.prompt_bytes);
        body["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":"c"}));
        tracker.observe(&serde_json::to_vec(&body).unwrap(), None, "http");
        let switched = tracker.latest().unwrap();
        assert_eq!(switched.invalidation, "transport_changed");
        assert!(switched.common_prompt_bytes > 0);
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
    #[test]
    fn the_per_request_billing_attestation_is_not_a_prompt_change() {
        let header = |cch: &str| {
            format!(
                "x-anthropic-billing-header: cc_version=2.1.280.abc; cc_entrypoint=cli; cch={cch};"
            )
        };
        let body = |cch: &str, tail: &str| {
            json!({"model":"m","system":[{"type":"text","text":header(cch)},{"type":"text","text":"fixed"}],
                   "messages":[{"role":"user","content":tail}]})
        };
        let mut tracker = Tracker::default();
        tracker.observe(
            &serde_json::to_vec(&body("1a2b3", "a")).unwrap(),
            None,
            "http",
        );
        tracker.observe(
            &serde_json::to_vec(&body("9f8e7", "a")).unwrap(),
            None,
            "http",
        );
        let same = tracker.latest().unwrap();
        assert_eq!(same.invalidation, "none");
        assert_eq!(same.common_prompt_bytes, same.prompt_bytes);
        // A real change in the system text after the header is still reported.
        let mut changed = body("9f8e7", "a");
        changed["system"][1]["text"] = json!("different");
        tracker.observe(&serde_json::to_vec(&changed).unwrap(), None, "http");
        assert_eq!(tracker.latest().unwrap().invalidation, "prompt_changed");
    }
}
