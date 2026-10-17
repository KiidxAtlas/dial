use super::*;

fn event() -> LifecycleEvent {
    LifecycleEvent {
        event: "tool_finished".into(),
        session_id: Some("session-1".into()),
        turn_id: Some("turn-2".into()),
        tool_name: Some("mcp__example__echo".into()),
        is_error: Some(true),
    }
}

#[test]
fn lifecycle_payload_is_opt_in_and_contains_only_sanitized_metadata() {
    let event = event();
    assert!(lifecycle_payload(&[], &event).is_none());
    let payload = lifecycle_payload(&["turn_started".into()], &event);
    assert!(payload.is_none(), "unsubscribed events are filtered");

    let payload = lifecycle_payload(&["tool_finished".into()], &event).unwrap();
    assert_eq!(
        payload,
        json!({
            "apiVersion": EXTENSION_API_VERSION,
            "event": "tool_finished",
            "sessionId": "session-1",
            "turnId": "turn-2",
            "toolName": "mcp__example__echo",
            "isError": true,
        })
    );
    assert_eq!(
        payload.as_object().unwrap().len(),
        6,
        "no arguments, prompts, outputs, or credentials"
    );
}
