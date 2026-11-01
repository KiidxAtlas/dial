//! Model-aware token counting for the wire. Only explicit OpenAI model IDs with a documented
//! tokenizer are counted with it. Every other provider or model keeps the conservative byte
//! estimate that `wire::estimate_parts` already uses. Matching is exact (no prefixes, aliases or
//! regex-like guesses), and the count cache keys on digests so it never retains prompt text.
use super::{IMAGE_ESTIMATE_TOKENS, estimate, estimate_parts, json_bytes};
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::LazyLock};

#[cfg(test)]
mod tests;

/// Documented OpenAI tokenizer encodings usable offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    O200kBase,
    Cl100kBase,
}

/// Provider id whose model IDs are checked against the OpenAI tokenizer table.
const OPENAI_PROVIDER: &str = "openai";

/// Returns the documented tokenizer for an explicit official model ID, or `None` when the
/// provider or model is not confidently recognized. The IDs are the exact-name entries of the
/// official tiktoken `MODEL_TO_ENCODING` table (`tiktoken/model.py`); its prefix entries are not
/// used, so dated snapshots and variant names stay on the estimate.
pub fn encoding_for(provider: &str, model: &str) -> Option<Encoding> {
    if provider != OPENAI_PROVIDER {
        return None;
    }
    match model {
        "o1" | "o3" | "o4-mini" | "gpt-5" | "gpt-4.1" | "gpt-4o" => Some(Encoding::O200kBase),
        "gpt-4" | "gpt-3.5-turbo" => Some(Encoding::Cl100kBase),
        _ => None,
    }
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Self::O200kBase => "o200k_base",
            Self::Cl100kBase => "cl100k_base",
        }
    }

    fn count(self, text: &str) -> u64 {
        let bpe = match self {
            Self::O200kBase => tiktoken_rs::o200k_base_singleton(),
            Self::Cl100kBase => tiktoken_rs::cl100k_base_singleton(),
        };
        // Special-token text is ordinary content on the wire, so it must not be interpreted.
        bpe.encode_ordinary(text).len() as u64
    }
}

/// Bound on cached counts. The cache is cleared when full instead of tracking recency.
const CACHE_LIMIT: usize = 4096;

/// Counts by digest of (encoding, text). Only the digest and the count are retained.
#[derive(Default)]
struct CountCache {
    counts: HashMap<[u8; 32], u64>,
}

static COUNTS: LazyLock<Mutex<CountCache>> = LazyLock::new(|| Mutex::new(CountCache::default()));

/// Tokens of `text` under `encoding`, memoized by digest. Tokenizing happens outside the lock.
fn counted(encoding: Encoding, text: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(encoding.name().as_bytes());
    hasher.update([0u8]);
    hasher.update(text.as_bytes());
    let key: [u8; 32] = hasher.finalize().into();
    if let Some(&count) = COUNTS.lock().counts.get(&key) {
        return count;
    }
    let count = encoding.count(text);
    let mut cache = COUNTS.lock();
    if cache.counts.len() >= CACHE_LIMIT {
        cache.counts.clear();
    }
    cache.counts.insert(key, count);
    count
}

/// Tokens of free text. Known models use their tokenizer. Unknown models use one token per three
/// bytes, a conservative heuristic, not a guarantee of provider-token accuracy.
pub fn text_tokens(provider: &str, model: &str, text: &str) -> u64 {
    match encoding_for(provider, model) {
        Some(encoding) => counted(encoding, text),
        None => (text.len() as u64).div_ceil(3),
    }
}

/// Serialized portable copy of a message as the provider receives it: `dial_*` bookkeeping is
/// removed, matching `wire::wire_parts`.
fn portable_text(message: &Value) -> String {
    let portable = match message.as_object() {
        Some(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !key.starts_with("dial_"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        None => message.clone(),
    };
    serde_json::to_string(&portable).expect("JSON value serialization cannot fail")
}

/// Serialized provider-native replay when it is present and non-empty; it replaces the portable
/// copy on the wire, as in `wire::wire_parts`.
fn native_text(message: &Value) -> Option<String> {
    let content = message
        .get("dial_provider_content")
        .filter(|content| content.as_array().is_some_and(|items| !items.is_empty()))?;
    Some(
        serde_json::to_string(&(message.get("role"), content))
            .expect("JSON value serialization cannot fail"),
    )
}

/// Tokens of one message as the request sends it. Known models count the portable copy or the
/// native replay, whichever is larger, plus a fixed image cost per attachment. Images and the
/// compaction-output floor apply to every model. Unknown models keep `wire::estimate` exactly.
pub fn message_tokens(provider: &str, model: &str, message: &Value) -> u64 {
    let Some(encoding) = encoding_for(provider, model) else {
        return estimate(message);
    };
    let portable = counted(encoding, &portable_text(message));
    // Opaque replay is not ordinary text; never lower its previous byte-based allowance.
    let native = native_text(message).map_or(0, |text| {
        counted(encoding, &text).max(super::wire_parts(message).native.div_ceil(4) as u64)
    });
    let floor = message["dial_compaction_output_tokens"]
        .as_u64()
        .unwrap_or(0);
    let images = message
        .get("dial_images")
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    portable
        .max(native)
        .max(floor)
        .saturating_add(images.saturating_mul(IMAGE_ESTIMATE_TOKENS as u64))
}

/// Tokens of tool definitions. They are counted in full, including any `dial_*` key, because the
/// schema is sent as written. Unknown models keep the byte estimate.
pub fn schema_tokens(provider: &str, model: &str, tools: &[Value]) -> u64 {
    match encoding_for(provider, model) {
        Some(encoding) => tools
            .iter()
            .map(|tool| {
                counted(
                    encoding,
                    &serde_json::to_string(tool).expect("JSON value serialization cannot fail"),
                )
            })
            .sum(),
        None => (tools.iter().map(json_bytes).sum::<usize>() as u64).div_ceil(4),
    }
}

/// Tokens of a whole request: messages plus tool definitions. Unknown models return exactly
/// `wire::estimate_parts`, so existing callers see no change.
pub fn request_tokens(provider: &str, model: &str, messages: &[Value], tools: &[Value]) -> u64 {
    if encoding_for(provider, model).is_none() {
        return estimate_parts(messages, tools);
    }
    messages
        .iter()
        .map(|message| message_tokens(provider, model, message))
        .sum::<u64>()
        .saturating_add(schema_tokens(provider, model, tools))
}
