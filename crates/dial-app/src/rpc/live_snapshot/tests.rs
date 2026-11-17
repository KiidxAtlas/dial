use super::*;

#[test]
fn snapshot_reconciles_closed_interactions_and_keeps_latest_todo() {
    let mut live = LiveSnapshot::default();
    live.observe(&json!({"event":"approval","approval_id":"a","call_id":"c","tool":"edit","summary":"write"}));
    live.observe(&json!({"event":"question","request":{"question_id":"q"}}));
    live.observe(&json!({"event":"todo","tasks":[{"content":"old"}]}));
    let snapshot = live.snapshot(Some("s"), 3);
    assert_eq!(snapshot["pending_approvals"][0]["call_id"], "c");
    assert_eq!(snapshot["question"]["question_id"], "q");
    live.observe(&json!({"event":"approval_resolved","approval_id":"a"}));
    live.observe(&json!({"event":"question_closed","question_id":"other"}));
    assert!(!live.snapshot(Some("s"), 5)["question"].is_null());
    live.observe(&json!({"event":"question_closed","question_id":"q"}));
    live.observe(&json!({"event":"todo","tasks":[]}));
    assert_eq!(
        live.snapshot(Some("s"), 7),
        json!({"session_id":"s","seq":7,"pending_approvals":[],"question":null,"todo":[]})
    );
}

#[test]
fn session_switch_drops_previous_interactions() {
    let mut live = LiveSnapshot::default();
    live.observe(&json!({"event":"approval","approval_id":"a"}));
    live.observe(&json!({"event":"todo","tasks":[{}]}));
    live.observe(&json!({"event":"active_session","session_id":"new"}));
    assert_eq!(
        live.snapshot(Some("new"), 3),
        json!({"session_id":"new","seq":3,"pending_approvals":[],"question":null,"todo":[]})
    );
}

#[test]
fn idle_recovery_matches_the_client_wire_fixture() {
    // This fixture is consumed over the transport by dial-client's recovery test as well.
    // Sharing wire data keeps the crates independent while detecting contract drift.
    let expected: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../dial-client/tests/fixtures/recovery_snapshot.json"
    )))
    .unwrap();
    let mut envelope = crate::rpc::Envelope::default();
    for mut event in [
        json!({"event":"ready","session_id":"s1"}),
        json!({"event":"approval","approval_id":"a","tool":"read","summary":"read","call_id":"c"}),
        json!({"event":"question","request":{"question_id":"q"}}),
        json!({"event":"todo","tasks":[{"content":"current task","status":"pending"}]}),
        json!({"event":"approval_resolved","approval_id":"a"}),
        json!({"event":"question_closed","question_id":"q"}),
    ] {
        envelope.stamp(&mut event);
    }
    let actual = envelope.live.snapshot(Some("s1"), envelope.seq);
    let mut expected_live = expected;
    expected_live.as_object_mut().unwrap().remove("messages");
    expected_live.as_object_mut().unwrap().remove("busy");
    assert_eq!(actual, expected_live);
}

#[test]
fn recovery_size_budget_counts_serialized_utf8_and_escaping_and_refuses_without_truncation() {
    for snapshot in [
        json!({}),
        json!({"messages":[{"content":"é\n\"\\\t"}]}),
        json!({"messages":[{"content":"x".repeat(4096)}],"todo":[]}),
    ] {
        let wire_bytes = serde_json::to_vec(&snapshot).unwrap().len();
        assert_eq!(bounded(snapshot.clone(), wire_bytes).unwrap(), snapshot);
        let error = bounded(snapshot.clone(), wire_bytes - 1).unwrap_err();
        let refusal = error.downcast_ref::<crate::rpc::RequestError>().unwrap();
        assert_eq!(refusal.code, crate::rpc::ErrorCode::Refused);
        assert!(refusal.message.contains("history was not truncated"));
        // Refusal is returned instead of an incomplete value; a later admitted response is intact.
        assert_eq!(bounded(snapshot.clone(), wire_bytes + 1).unwrap(), snapshot);
    }
}
