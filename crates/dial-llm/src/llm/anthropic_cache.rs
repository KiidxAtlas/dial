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

/// Whether the profile and account select the 1h lifetime for the head markers at `Level::Full`.
pub(super) fn ttl_applies(profile: Option<&ModelProfile>, official_oauth: bool) -> bool {
    let retention = profile.map(|p| p.cache_retention).unwrap_or_default();
    retention == CacheRetention::Long || (retention == CacheRetention::Auto && official_oauth)
}

/// Transient tag on a system block naming its prompt band (`llm::prompt::Band`). The request
/// builder sets it; [`apply`] anchors the system breakpoint with it and always removes it.
pub(super) const BAND_TAG: &str = "dial_prompt_band";

/// Retention of the head markers (tools and system), shared by the wire and the paid warming
/// schedule. Rolling history markers always use the 5-minute default, so under `Long` a replay
/// keeps the head entries warm while older history entries may already have lapsed.
pub(super) fn tier(
    profile: Option<&ModelProfile>,
    official_oauth: bool,
    level: Level,
) -> Option<super::cache_warmer::Tier> {
    use super::cache_warmer::Tier;
    if !enabled(profile, level) {
        None
    } else if level == Level::Full && ttl_applies(profile, official_oauth) {
        Some(Tier::Long)
    } else {
        Some(Tier::Short)
    }
}

fn enabled(profile: Option<&ModelProfile>, level: Level) -> bool {
    level != Level::Off && profile.and_then(|p| p.prompt_cache) != Some(false)
}

/// Apply after schema/profile transforms and before OAuth body attestation.
/// Native-compatible endpoints can opt out with prompt_cache=false.
///
/// Budget: one marker on the last tool, one at the end of the last Frozen/Stable system band, and
/// at most two rolling history anchors, within the four-marker limit. The head markers (tools and
/// system) carry the long lifetime when it applies; the rolling ones always use the 5-minute
/// default, so every longer-lived entry precedes the shorter ones as the API requires.
pub(super) fn apply(
    body: &mut Value,
    profile: Option<&ModelProfile>,
    official_oauth: bool,
    level: Level,
) {
    let enabled = enabled(profile, level);
    let head = if enabled && level == Level::Full && ttl_applies(profile, official_oauth) {
        json!({"type":"ephemeral", "ttl":"1h"})
    } else {
        json!({"type":"ephemeral"})
    };
    let rolling = json!({"type":"ephemeral"});

    // Native tool order must not depend on registry insertion order. Do not retain withdrawn
    // tools: exposing them would contradict approval/recovery and tool-loading contracts.
    // Mutable indexing inserts null for absent keys. Tool-free compaction requests must
    // retain absent optional fields instead of sending invalid tools/system null values.
    // Deferred tools (`defer_loading`) are outside the prompt prefix and cannot carry
    // `cache_control`: they sort after the loaded ones and the marker goes on the last loaded tool.
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        let deferred = |tool: &Value| tool["defer_loading"] == true;
        tools.sort_by(|a, b| {
            (deferred(a), a["name"].as_str()).cmp(&(deferred(b), b["name"].as_str()))
        });
        if enabled && let Some(last) = tools.iter_mut().rev().find(|tool| !deferred(tool)) {
            last["cache_control"] = head.clone();
        }
    }
    if let Some(text) = body["system"].as_str().map(str::to_owned) {
        if enabled {
            body["system"] = json!([{"type":"text", "text":text, "cache_control":head}]);
        }
    } else if let Some(blocks) = body.get_mut("system").and_then(Value::as_array_mut) {
        let anchor = take_band_anchor(blocks);
        if enabled {
            match anchor {
                // Dynamic bands follow the breakpoint, so a per-turn note or memory change no
                // longer invalidates the cached tools and stable system prefix.
                BandAnchor::At(index) => blocks[index]["cache_control"] = head.clone(),
                BandAnchor::Untagged => {
                    mark_last(blocks, &head);
                }
                BandAnchor::DynamicOnly => {}
            }
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
            // Two rolling history anchors: the previous tail remains discoverable as the
            // conversation grows. Never mark generated thinking/redacted_thinking, and never place
            // an anchor beyond a transient suffix.
            let mut marked = 0;
            for message in messages[..stable_end].iter_mut().rev() {
                let cached = if let Some(text) = message["content"].as_str().map(str::to_owned) {
                    message["content"] =
                        json!([{"type":"text","text":text,"cache_control":rolling.clone()}]);
                    true
                } else {
                    message
                        .get_mut("content")
                        .and_then(Value::as_array_mut)
                        .is_some_and(|blocks| mark_last(blocks, &rolling))
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

/// Where the system breakpoint goes, read from the band tags (which are removed).
enum BandAnchor {
    /// The last text block of a Frozen or Stable band.
    At(usize),
    /// No block carried a band tag: mark the last block, as for any caller-built system array.
    Untagged,
    /// Only Dynamic bands: nothing reusable to anchor.
    DynamicOnly,
}

fn take_band_anchor(blocks: &mut [Value]) -> BandAnchor {
    use super::prompt::Band;
    let mut tagged = false;
    let mut anchor = None;
    for (index, block) in blocks.iter_mut().enumerate() {
        let Some(tag) = block.as_object_mut().and_then(|b| b.remove(BAND_TAG)) else {
            continue;
        };
        tagged = true;
        let band = serde_json::from_value(tag).unwrap_or(Band::Stable);
        if matches!(band, Band::Frozen | Band::Stable) && block["type"] == "text" {
            anchor = Some(index);
        }
    }
    match (anchor, tagged) {
        (Some(index), _) => BandAnchor::At(index),
        (None, false) => BandAnchor::Untagged,
        (None, true) => BandAnchor::DynamicOnly,
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
