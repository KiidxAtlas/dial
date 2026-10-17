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
    assert!(changed.common_prompt_bytes > 0 && changed.common_prompt_bytes < changed.prompt_bytes);
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
    let delta =
        json!({"model":"m","previous_response_id":"r1","input":[{"role":"user","content":"b"}]});
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
        format!("x-anthropic-billing-header: cc_version=2.1.280.abc; cc_entrypoint=cli; cch={cch};")
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
