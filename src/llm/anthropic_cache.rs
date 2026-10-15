//! Native Messages prompt caching. Breakpoints cover reusable prefixes, never request-local guidance.
use super::profiles::{CacheRetention, ModelProfile};
use serde_json::{Value, json};

/// Apply after schema/profile transforms and before OAuth body attestation.
/// Native-compatible endpoints can opt out with prompt_cache=false.
pub(super) fn apply(body: &mut Value, profile: Option<&ModelProfile>, official_oauth: bool) {
    let enabled = profile.and_then(|p| p.prompt_cache).unwrap_or(true);
    let retention = profile.map(|p| p.cache_retention).unwrap_or_default();
    let long =
        retention == CacheRetention::Long || (retention == CacheRetention::Auto && official_oauth);
    let control = if long {
        json!({"type":"ephemeral", "ttl":"1h"})
    } else {
        json!({"type":"ephemeral"})
    };

    // Native tool order must not depend on registry insertion order. Do not retain withdrawn
    // tools: exposing them would contradict approval/recovery and tool-loading contracts.
    if let Some(tools) = body["tools"].as_array_mut() {
        tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        if enabled && let Some(last) = tools.last_mut() {
            last["cache_control"] = control.clone();
        }
    }
    if enabled {
        if let Some(text) = body["system"].as_str().map(str::to_owned) {
            body["system"] = json!([{"type":"text", "text":text, "cache_control":control}]);
        } else if let Some(blocks) = body["system"].as_array_mut() {
            mark_last(blocks, &control);
        }
    }
    if let Some(messages) = body["messages"].as_array_mut() {
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
                    message["content"]
                        .as_array_mut()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        json!({"system":"instructions", "tools":[{"name":"write"},{"name":"read"}], "messages":[
            {"role":"user","content":[{"type":"text","text":"fix it"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"a","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"source"}]}
        ]})
    }
    fn count(body: &Value) -> usize {
        match body {
            Value::Object(map) => {
                usize::from(map.contains_key("cache_control"))
                    + map.values().map(count).sum::<usize>()
            }
            Value::Array(array) => array.iter().map(count).sum(),
            _ => 0,
        }
    }

    #[test]
    fn growing_history_keeps_the_previous_tail_and_four_marker_budget() {
        let mut first = body();
        apply(&mut first, None, false);
        assert_eq!(count(&first), 4);
        assert_eq!(
            first["messages"][2]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        let mut next = body();
        next["messages"].as_array_mut().unwrap().push(json!({
            "role":"assistant","content":[{"type":"text","text":"fixed"}]
        }));
        apply(&mut next, None, false);
        assert_eq!(count(&next), 4);
        assert_eq!(first["messages"][2], next["messages"][2]);
        assert_eq!(first["tools"], next["tools"]);
        assert_eq!(first["system"], next["system"]);
        assert!(next["messages"][3]["content"][0]["cache_control"].is_object());
    }

    #[test]
    fn transient_guidance_and_signed_thinking_are_never_cache_anchors() {
        let mut body = body();
        let signed = json!({"type":"thinking","thinking":"reasoning","signature":"signed"});
        body["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":[signed.clone()]}),
            json!({"role":"user","dial_transient":true,"content":[{"type":"text","text":"changing guidance"}]}),
            json!({"role":"user","content":[{"type":"text","text":"also unstable"}]})
        ]);
        apply(&mut body, None, false);
        assert_eq!(body["messages"][3]["content"][0], signed);
        assert!(body["messages"][4].get("dial_transient").is_none());
        assert!(
            body["messages"][4]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert!(
            body["messages"][5]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(count(&body), 4);
    }

    #[test]
    fn retention_and_opt_out_follow_auth_and_explicit_configuration() {
        for (oauth, retention, expected_long) in [
            (false, CacheRetention::Auto, false),
            (true, CacheRetention::Auto, true),
            (true, CacheRetention::Short, false),
            (false, CacheRetention::Long, true),
        ] {
            let mut body = body();
            let profile = ModelProfile {
                cache_retention: retention,
                ..Default::default()
            };
            apply(&mut body, Some(&profile), oauth);
            assert_eq!(
                body["system"][0]["cache_control"]["ttl"].as_str(),
                expected_long.then_some("1h")
            );
            assert_eq!(
                body["messages"][2]["content"][0]["cache_control"],
                body["system"][0]["cache_control"]
            );
        }
        let mut disabled = body();
        disabled["messages"][2]["dial_transient"] = json!(true);
        apply(
            &mut disabled,
            Some(&ModelProfile {
                prompt_cache: Some(false),
                ..Default::default()
            }),
            true,
        );
        assert_eq!(count(&disabled), 0);
        assert!(disabled["messages"][2].get("dial_transient").is_none());
    }

    #[test]
    fn tool_order_is_stable_but_withdrawn_tools_are_not_reintroduced() {
        let mut a = body();
        let mut b = body();
        b["tools"].as_array_mut().unwrap().reverse();
        apply(&mut a, None, false);
        apply(&mut b, None, false);
        assert_eq!(a["tools"], b["tools"]);
        let mut withdrawn = body();
        withdrawn["tools"]
            .as_array_mut()
            .unwrap()
            .retain(|t| t["name"] == "read");
        apply(&mut withdrawn, None, false);
        assert_eq!(withdrawn["tools"].as_array().unwrap().len(), 1);
        assert_eq!(withdrawn["tools"][0]["name"], "read");
    }

    #[test]
    fn plain_text_history_is_cacheable_and_transient_text_stays_unmarked() {
        let mut body = json!({"messages":[
            {"role":"user","content":"hello"},
            {"role":"assistant","content":"reply"},
            {"role":"user","content":"changing guidance","dial_transient":true}
        ]});
        apply(&mut body, None, false);
        assert_eq!(count(&body), 2);
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(body["messages"][1]["content"][0]["text"], "reply");
        assert_eq!(body["messages"][2]["content"], "changing guidance");
        assert!(body["messages"][2].get("dial_transient").is_none());
    }
}
