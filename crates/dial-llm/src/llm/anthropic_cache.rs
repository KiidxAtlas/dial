//! Native Messages prompt caching. Breakpoints cover reusable prefixes, never request-local guidance.
//!
//! Some endpoints reject `cache_control` (or its `ttl`) outright. A refusal steps the request down
//! one rung at a time: full 1h/5m markers, then 5m markers without `ttl`, then no markers at all.
//! Each endpoint and model latches the narrowest level it has needed, so later requests skip the
//! refused rungs instead of paying a failed round trip every turn.
use super::profiles::{CacheRetention, ModelProfile};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// How much of the cache-control request an endpoint has accepted, widest first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Level {
    /// Markers with the profile's retention (1h when long retention applies).
    #[default]
    Full,
    /// Markers without `ttl`: the default 5-minute lifetime.
    WithoutTtl,
    /// No cache markers; the request is sent as if caching were disabled.
    Off,
}

impl Level {
    /// The next narrower rung after a refusal, or `None` once no narrower rung exists.
    /// A rung that would not change the request (no `ttl` to drop) is skipped.
    pub(super) fn narrower(self, ttl_applies: bool) -> Option<Self> {
        match self {
            Self::Full if ttl_applies => Some(Self::WithoutTtl),
            Self::Full | Self::WithoutTtl => Some(Self::Off),
            Self::Off => None,
        }
    }
}

/// Whether the profile and account select the 1h lifetime for `Level::Full`.
pub(super) fn ttl_applies(profile: Option<&ModelProfile>, official_oauth: bool) -> bool {
    let retention = profile.map(|p| p.cache_retention).unwrap_or_default();
    retention == CacheRetention::Long || (retention == CacheRetention::Auto && official_oauth)
}

/// Effective retention, shared by wire markers and the paid warming schedule.
pub(super) fn tier(
    profile: Option<&ModelProfile>,
    official_oauth: bool,
    level: Level,
) -> Option<super::cache_warmer::Tier> {
    use super::cache_warmer::Tier;
    if level == Level::Off || profile.and_then(|p| p.prompt_cache) == Some(false) {
        None
    } else if level == Level::Full && ttl_applies(profile, official_oauth) {
        Some(Tier::Long)
    } else {
        Some(Tier::Short)
    }
}

/// Apply after schema/profile transforms and before OAuth body attestation.
/// Native-compatible endpoints can opt out with prompt_cache=false.
pub(super) fn apply(
    body: &mut Value,
    profile: Option<&ModelProfile>,
    official_oauth: bool,
    level: Level,
) {
    let tier = tier(profile, official_oauth, level);
    let enabled = tier.is_some();
    let long = tier == Some(super::cache_warmer::Tier::Long);
    let control = if long {
        json!({"type":"ephemeral", "ttl":"1h"})
    } else {
        json!({"type":"ephemeral"})
    };

    // Native tool order must not depend on registry insertion order. Do not retain withdrawn
    // tools: exposing them would contradict approval/recovery and tool-loading contracts.
    // Mutable indexing inserts null for absent keys. Tool-free compaction requests must
    // retain absent optional fields instead of sending invalid tools/system null values.
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        if enabled && let Some(last) = tools.last_mut() {
            last["cache_control"] = control.clone();
        }
    }
    if enabled {
        if let Some(text) = body["system"].as_str().map(str::to_owned) {
            body["system"] = json!([{"type":"text", "text":text, "cache_control":control}]);
        } else if let Some(blocks) = body.get_mut("system").and_then(Value::as_array_mut) {
            mark_last(blocks, &control);
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        let mut stable_end = messages.len();
        for (index, message) in messages.iter_mut().enumerate() {
            if message
                .as_object_mut()
                .is_some_and(|m| m.remove("dial_transient").and_then(|v| v.as_bool()) == Some(true))
            {
                stable_end = stable_end.min(index);
            }
        }
        if enabled {
            // Two head anchors + at most two rolling history anchors obey the four-marker limit.
            // The previous tail remains discoverable as the conversation grows. Never mark generated
            // thinking/redacted_thinking, and never place an anchor beyond a transient suffix.
            let mut marked = 0;
            for message in messages[..stable_end].iter_mut().rev() {
                let cached = if let Some(text) = message["content"].as_str().map(str::to_owned) {
                    message["content"] =
                        json!([{"type":"text","text":text,"cache_control":control.clone()}]);
                    true
                } else {
                    message
                        .get_mut("content")
                        .and_then(Value::as_array_mut)
                        .is_some_and(|blocks| mark_last(blocks, &control))
                };
                if cached {
                    marked += 1;
                    if marked == 2 {
                        break;
                    }
                }
            }
        }
    }
}

fn mark_last(blocks: &mut [Value], control: &Value) -> bool {
    for block in blocks.iter_mut().rev() {
        if matches!(
            block["type"].as_str(),
            Some("text" | "image" | "tool_use" | "tool_result")
        ) {
            block["cache_control"] = control.clone();
            return true;
        }
    }
    false
}

/// Whether a 400 response says the endpoint does not accept `cache_control` (or its `ttl`).
/// Only a 400 whose body names the field and a rejection qualifies, so an unrelated 400 never
/// steps the ladder down.
pub(super) fn refused(status: u16, body: &[u8]) -> bool {
    const REJECTIONS: [&str; 12] = [
        "unexpected",
        "unrecognized",
        "unknown",
        "not permitted",
        "not allowed",
        "not recognized",
        "not supported",
        "unsupported",
        "extra input",
        "extra field",
        "invalid_field",
        "invalid field",
    ];
    if status != 400 {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    let names_field = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| word == "cache_control");
    names_field && REJECTIONS.iter().any(|word| text.contains(word))
}

/// Narrowest level each endpoint and model has needed, keyed by the URL without its query.
static REFUSED: LazyLock<Mutex<HashMap<(String, String), Level>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn latch_key(url: &str, model: &str) -> (String, String) {
    (
        url.split('?').next().unwrap_or(url).to_owned(),
        model.to_owned(),
    )
}

/// The level a new request to this endpoint and model starts from.
pub(super) fn latched(url: &str, model: &str) -> Level {
    REFUSED
        .lock()
        .ok()
        .and_then(|map| map.get(&latch_key(url, model)).copied())
        .unwrap_or_default()
}

/// Remember a refused rung: later requests start at `level` or narrower, never wider.
pub(super) fn latch(url: &str, model: &str, level: Level) {
    if let Ok(mut map) = REFUSED.lock() {
        let entry = map.entry(latch_key(url, model)).or_default();
        *entry = (*entry).max(level);
    }
}

/// Drops every latched refusal for one origin, so a test server reusing a port starts fresh.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn forget_refusals(origin: &str) {
    if let Ok(mut map) = REFUSED.lock() {
        map.retain(|(url, _), _| !super::same_origin(url, origin));
    }
}

#[cfg(test)]
mod tests;
