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
            usize::from(map.contains_key("cache_control")) + map.values().map(count).sum::<usize>()
        }
        Value::Array(array) => array.iter().map(count).sum(),
        _ => 0,
    }
}

#[test]
fn tool_free_requests_do_not_gain_null_optional_fields() {
    for enabled in [true, false] {
        let profile = ModelProfile {
            prompt_cache: Some(enabled),
            ..Default::default()
        };
        let mut body = json!({"messages":[{"role":"user","content":"summarize history"}]});
        apply(&mut body, Some(&profile), false, Level::Full);
        assert!(!body.as_object().unwrap().contains_key("tools"), "{body}");
        assert!(!body.as_object().unwrap().contains_key("system"), "{body}");
    }
}
#[test]
fn growing_history_keeps_the_previous_tail_and_four_marker_budget() {
    let mut first = body();
    apply(&mut first, None, false, Level::Full);
    assert_eq!(count(&first), 4);
    assert_eq!(
        first["messages"][2]["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    let mut next = body();
    next["messages"].as_array_mut().unwrap().push(json!({
        "role":"assistant","content":[{"type":"text","text":"fixed"}]
    }));
    apply(&mut next, None, false, Level::Full);
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
    apply(&mut body, None, false, Level::Full);
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
        apply(&mut body, Some(&profile), oauth, Level::Full);
        assert_eq!(
            tier(Some(&profile), oauth, Level::Full),
            Some(if expected_long {
                super::super::cache_warmer::Tier::Long
            } else {
                super::super::cache_warmer::Tier::Short
            })
        );
        assert_eq!(
            tier(Some(&profile), oauth, Level::WithoutTtl),
            Some(super::super::cache_warmer::Tier::Short)
        );
        assert_eq!(tier(Some(&profile), oauth, Level::Off), None);
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
        Level::Full,
    );
    assert_eq!(count(&disabled), 0);
    assert!(disabled["messages"][2].get("dial_transient").is_none());
    assert_eq!(
        tier(
            Some(&ModelProfile {
                prompt_cache: Some(false),
                ..Default::default()
            }),
            true,
            Level::Full
        ),
        None
    );
}

#[test]
fn tool_order_is_stable_but_withdrawn_tools_are_not_reintroduced() {
    let mut a = body();
    let mut b = body();
    b["tools"].as_array_mut().unwrap().reverse();
    apply(&mut a, None, false, Level::Full);
    apply(&mut b, None, false, Level::Full);
    assert_eq!(a["tools"], b["tools"]);
    let mut withdrawn = body();
    withdrawn["tools"]
        .as_array_mut()
        .unwrap()
        .retain(|t| t["name"] == "read");
    apply(&mut withdrawn, None, false, Level::Full);
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
    apply(&mut body, None, false, Level::Full);
    assert_eq!(count(&body), 2);
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(body["messages"][1]["content"][0]["text"], "reply");
    assert_eq!(body["messages"][2]["content"], "changing guidance");
    assert!(body["messages"][2].get("dial_transient").is_none());
}

#[test]
fn refusal_ladder_drops_ttl_before_markers_and_skips_rungs_that_change_nothing() {
    assert_eq!(Level::Full.narrower(true), Some(Level::WithoutTtl));
    assert_eq!(Level::WithoutTtl.narrower(true), Some(Level::Off));
    assert_eq!(Level::Full.narrower(false), Some(Level::Off));
    assert_eq!(Level::Off.narrower(true), None);

    let mut body = body();
    apply(&mut body, None, true, Level::WithoutTtl);
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    assert_eq!(count(&body), 4);

    let mut off = self::body();
    apply(&mut off, None, true, Level::Off);
    assert_eq!(count(&off), 0);
}

#[test]
fn only_a_400_naming_cache_control_with_a_rejection_steps_the_ladder() {
    let refusal = br#"{"error":{"message":"cache_control.ttl: Extra inputs are not permitted"}}"#;
    assert!(refused(400, refusal));
    assert!(!refused(500, refusal));
    assert!(!refused(
        400,
        br#"{"error":{"message":"invalid x-api-key"}}"#
    ));
    assert!(!refused(
        400,
        br#"{"error":{"message":"cache_control is required"}}"#
    ));
    assert!(!refused(
        400,
        br#"{"error":{"message":"cache_controlled: unsupported"}}"#
    ));
}

#[test]
fn latched_refusals_start_later_requests_narrow_and_never_widen() {
    let url = "https://ladder-latch.invalid/v1/messages?beta=1";
    assert_eq!(latched(url, "model-a"), Level::Full);
    latch(url, "model-a", Level::Off);
    assert_eq!(latched(url, "model-a"), Level::Off);
    latch(url, "model-a", Level::WithoutTtl);
    assert_eq!(latched(url, "model-a"), Level::Off);
    assert_eq!(latched(url, "model-b"), Level::Full);
    forget_refusals("https://ladder-latch.invalid");
    assert_eq!(latched(url, "model-a"), Level::Full);
}
