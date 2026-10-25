use super::*;
use serde_json::json;

#[test]
fn removes_only_bookkeeping_and_preserves_evidence_text() {
    let original = json!({"role":"tool","content":"answer [artifact://proof]","tool_call_id":"1",
        "dial_evidence":{"artifact":"artifact://proof"},"dial_recall":"notes",
        "dial_transient":true,"unknown":"reject me","dial_turn_draft":{"original":"typed"},
        "dial_images":[{"url":"artifact://image"}],
        "dial_provider_content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]});
    let projected = canonical(&original);
    assert!(projected.get("dial_evidence").is_none());
    assert!(projected.get("dial_transient").is_none());
    assert!(projected.get("dial_recall").is_none());
    assert!(projected.get("dial_turn_draft").is_none());
    assert_eq!(projected["content"], original["content"]);
    assert_eq!(projected["unknown"], "reject me");
    assert_eq!(projected["dial_images"], original["dial_images"]);
    assert_eq!(
        projected["dial_provider_content"],
        original["dial_provider_content"]
    );
    assert!(original.get("dial_evidence").is_some());
}

#[test]
fn local_prefix_is_merged_without_moving_guidance_or_tool_history() {
    let messages = vec![
        json!({"role":"system","content":"policy"}),
        json!({"role":"developer","content":"workspace"}),
        json!({"role":"user","content":[{"type":"text","text":"inspect"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}),
        json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1",
            "type":"function","function":{"name":"read","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"c1","content":"file"}),
        json!({"role":"system","content":"steer","dial_transient":true}),
        json!({"role":"developer","content":"continue"}),
    ];
    let original = messages.clone();
    let wire = local(&messages).unwrap();
    assert_eq!(
        wire[0],
        json!({"role":"system","content":"policy\n\nworkspace"})
    );
    assert_eq!(&wire[1..4], &messages[2..5]);
    assert_eq!(
        wire[4],
        json!({"role":"user","content":"steer","dial_transient":true})
    );
    assert_eq!(wire[5], json!({"role":"user","content":"continue"}));
    assert_eq!(messages, original);
    assert_eq!(local(&wire).unwrap(), wire);
}

#[test]
fn local_preserves_empty_and_already_compatible_conversations() {
    for messages in [
        vec![],
        vec![json!({"role":"user","content":"hello"})],
        vec![
            json!({"role":"system","content":""}),
            json!({"role":"user","content":"hello"}),
        ],
    ] {
        assert_eq!(local(&messages).unwrap(), messages);
    }
}

#[test]
fn local_transient_guidance_ends_the_system_prefix() {
    let wire = local(&[
        json!({"role":"system","content":"steer","dial_transient":true}),
        json!({"role":"system","content":"later"}),
    ])
    .unwrap();
    assert_eq!(
        wire.iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "user"]
    );
    assert_eq!(wire[0]["content"], "steer");
    assert_eq!(wire[1]["content"], "later");
}

#[test]
fn local_rejects_invalid_system_content_instead_of_losing_it() {
    for content in [Value::Null, json!(42), json!({"text":"policy"})] {
        assert!(local(&[json!({"role":"system","content":content})]).is_err());
    }
}
