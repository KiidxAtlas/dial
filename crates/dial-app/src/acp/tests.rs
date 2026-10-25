use super::*;
use crate::agent::ui::ui_event::{EventKind, UiEvent};

/// Session updates for one active session, after each event passes through the adapter.
fn session_updates(events: Vec<Value>) -> Vec<Value> {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut adapter = Adapter::new(layout);
    adapter.emit(json!({"event":"active_session","session_id":"s1"}));
    events
        .into_iter()
        .flat_map(|event| adapter.emit(event))
        .map(|message| {
            assert_eq!(message["method"], "session/update");
            assert_eq!(message["params"]["sessionId"], "s1");
            message["params"]["update"].clone()
        })
        .collect()
}

#[test]
fn overlapping_same_name_calls_keep_their_own_results() {
    let updates = session_updates(vec![
        json!({"event":"tool_call","call_id":"a","name":"read","summary":"first"}),
        json!({"event":"tool_call","call_id":"b","name":"read","summary":"second"}),
        json!({"event":"tool_output","call_id":"a","name":"read","delta":"partial a"}),
        json!({"event":"tool_output","call_id":"b","name":"read","delta":"partial b"}),
        json!({"event":"tool_result","call_id":"b","name":"read","text":"second result","is_error":true}),
        json!({"event":"tool_result","call_id":"a","name":"read","text":"first result","data":{"answer":1}}),
    ]);
    assert_eq!(updates.len(), 6);
    let a = &updates[0]["toolCallId"];
    let b = &updates[1]["toolCallId"];
    assert_ne!(a, b);
    for index in [2, 5] {
        assert_eq!(&updates[index]["toolCallId"], a);
    }
    for index in [3, 4] {
        assert_eq!(&updates[index]["toolCallId"], b);
    }
    assert_eq!(updates[2]["content"][0]["content"]["text"], "partial a");
    assert_eq!(updates[3]["content"][0]["content"]["text"], "partial b");
    assert_eq!(updates[4]["status"], "failed");
    assert_eq!(updates[5]["status"], "completed");
    assert_eq!(updates[5]["rawOutput"], json!({"answer":1}));
}

#[test]
fn approval_and_legacy_output_follow_the_call_and_session_lifetime() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut adapter = Adapter::new(layout);
    adapter.emit(json!({"event":"active_session","session_id":"s1"}));
    let approval = adapter.emit(json!({"event":"approval","approval_id":"p1","tool":"bash"}));
    let id = approval[0]["params"]["toolCall"]["toolCallId"].clone();
    let call = adapter.emit(json!({"event":"tool_call","call_id":"a","name":"bash"}));
    assert_eq!(call[0]["params"]["update"]["toolCallId"], id);
    let partial = adapter.emit(json!({"event":"tool_output","name":"bash","delta":"output"}));
    assert_eq!(partial[0]["params"]["update"]["toolCallId"], id);
    assert!(
        adapter
            .emit(json!({"event":"tool_result","call_id":"unknown","name":"bash"}))
            .is_empty()
    );
    let result =
        adapter.emit(json!({"event":"tool_result","call_id":"a","name":"bash","text":"done"}));
    assert_eq!(result[0]["params"]["update"]["toolCallId"], id);
    assert!(adapter.tools.is_empty());
    adapter.emit(json!({"event":"tool_call","call_id":"old","name":"read"}));
    adapter.emit(json!({"event":"active_session","session_id":"s2"}));
    assert!(adapter.tools.is_empty());
    assert!(
        adapter
            .emit(json!({"event":"tool_result","call_id":"old","name":"read"}))
            .is_empty()
    );
}

#[test]
fn typed_notice_keeps_text_chunk_and_carries_dial_source_meta() {
    let event = crate::rpc::typed_notice(
        &UiEvent::new(EventKind::Compaction, "History compacted").with_tokens(900, 120),
    );
    let updates = session_updates(vec![event]);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(
        updates[0]["content"],
        json!({"type":"text","text":"History compacted"})
    );
    assert_eq!(
        updates[0]["_meta"]["dial"],
        json!({"event":"notice","kind":"compaction","lane":"divider","tokens":[900,120]})
    );
}

#[test]
fn plain_notice_background_and_fatal_omit_absent_fields() {
    let updates = session_updates(vec![
        json!({"event":"notice","message":"plain"}),
        json!({"event":"background","message":"started"}),
        json!({"event":"background","name":"job","message":"partial","replace":true}),
        json!({"event":"background","name":"job","message":"append","replace":false}),
        json!({"event":"fatal","message":"boom"}),
    ]);
    assert_eq!(updates.len(), 5);
    assert_eq!(updates[0]["content"]["text"], "plain");
    assert_eq!(updates[0]["_meta"]["dial"], json!({"event":"notice"}));
    assert_eq!(updates[1]["_meta"]["dial"], json!({"event":"background"}));
    assert_eq!(
        updates[2]["_meta"]["dial"],
        json!({"event":"background","name":"job","replace":true})
    );
    assert_eq!(
        updates[3]["_meta"]["dial"],
        json!({"event":"background","name":"job","replace":false})
    );
    assert_eq!(updates[4]["content"]["text"], "boom");
    assert_eq!(updates[4]["_meta"]["dial"], json!({"event":"fatal"}));
}
#[test]
fn cancellation_cannot_steal_a_prompt_response_and_notifications_have_no_reply() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut adapter = Adapter::new(layout);
    adapter.initialized = true;
    adapter.sessions.extend(["s1".into(), "s2".into()]);
    adapter.active = Some("s1".into());
    let (prompt, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":1,
        "method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"hello"}]}}));
    assert_eq!(prompt.unwrap()["method"], "send");
    assert!(replies.is_empty());
    adapter
        .permissions
        .insert("permission".into(), "approval".into());
    let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":1,
        "method":"session/cancel","params":{"sessionId":"s1"}}));
    assert!(request.is_none());
    assert!(replies[0].get("error").is_some());
    assert_eq!(adapter.pending["1"].method, "session/prompt");
    assert_eq!(adapter.permissions.len(), 1);

    let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":2,
        "method":"session/cancel","params":{"sessionId":"s1"}}));
    assert_eq!(request.unwrap()["method"], "abort");
    assert!(replies.is_empty());
    assert!(adapter.permissions.is_empty());
    assert_eq!(
        adapter.emit(json!({"id":2,"result":{"requested":true}})),
        vec![response(&json!(2), json!({}))]
    );

    let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0",
        "method":"session/cancel","params":{"sessionId":"s1"}}));
    assert_eq!(request.unwrap()["method"], "abort");
    assert!(replies.is_empty());
    assert!(
        adapter
            .emit(json!({"id":null,"result":{"requested":true}}))
            .is_empty()
    );
    let prompt_reply = adapter.emit(json!({"id":1,"result":{"interrupted":true}}));
    assert_eq!(
        prompt_reply.last().unwrap(),
        &response(&json!(1), json!({"stopReason":"cancelled"}))
    );
    assert!(adapter.pending.is_empty());

    let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":3,
        "method":"session/cancel","params":{"sessionId":"s2"}}));
    assert!(request.is_none());
    assert_eq!(replies, vec![response(&json!(3), json!({}))]);
}

#[test]
fn prompt_stop_reasons_distinguish_outcomes() {
    assert_eq!(
        prompt_stop(&json!({"outcome":"completed","text":"done"})),
        json!({"stopReason":"end_turn"})
    );
    assert_eq!(
        prompt_stop(&json!({"interrupted":true,"outcome":"interrupted"})),
        json!({"stopReason":"cancelled"})
    );
    for outcome in ["failed", "blocked", "needs_input", "incomplete"] {
        let stop = prompt_stop(&json!({"outcome":outcome,"text":"report"}));
        assert_eq!(stop["stopReason"], "end_turn", "{outcome}");
        assert_eq!(stop["_meta"]["dial"]["outcome"], outcome);
        assert_eq!(stop["_meta"]["dial"]["report"], "report");
    }
    let limited = prompt_stop(&json!({"outcome":"incomplete","step_limit":true,"text":"Stopped"}));
    assert_eq!(limited["stopReason"], "max_turn_requests");
    assert_eq!(limited["_meta"]["dial"]["outcome"], "incomplete");
}

#[test]
fn plan_entries_map_every_todo_status_without_silent_pending() {
    let entry = |status: &str| plan_entry(&json!({"content":"task","status":status,"note":"why"}));
    assert_eq!(entry("pending")["status"], "pending");
    assert_eq!(entry("in_progress")["status"], "in_progress");
    assert_eq!(entry("completed")["status"], "completed");
    assert_eq!(entry("completed")["_meta"], Value::Null);
    assert_eq!(entry("abandoned")["status"], "completed");
    assert_eq!(
        entry("abandoned")["_meta"]["dial"],
        json!({"status":"abandoned","note":"why"})
    );
    assert_eq!(entry("blocked")["status"], "pending");
    assert_eq!(
        entry("blocked")["_meta"]["dial"],
        json!({"status":"blocked","note":"why"})
    );
}

#[test]
fn approvals_attach_by_call_id_and_expire_when_decided() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut adapter = Adapter::new(layout);
    adapter.emit(json!({"event":"active_session","session_id":"s1"}));
    adapter.emit(json!({"event":"tool_call","call_id":"a","name":"bash"}));
    adapter.emit(json!({"event":"tool_call","call_id":"b","name":"bash"}));
    // Same-name calls: the approval for call "a" attaches to its own card, not the latest one.
    let attached =
        adapter.emit(json!({"event":"approval","approval_id":"p1","tool":"bash","call_id":"a"}));
    assert_eq!(
        attached[0]["params"]["toolCall"]["toolCallId"],
        "dial-tool-1"
    );
    // An approval without a call card gets a placeholder, which a same-name call with another
    // call ID must not adopt, and which the decision removes.
    let orphan =
        adapter.emit(json!({"event":"approval","approval_id":"p2","tool":"bash","call_id":"zz"}));
    assert_eq!(orphan[0]["params"]["toolCall"]["toolCallId"], "p2");
    let call = adapter.emit(json!({"event":"tool_call","call_id":"c","name":"bash"}));
    assert_eq!(call[0]["params"]["update"]["toolCallId"], "dial-tool-3");
    assert_eq!(adapter.tools.len(), 4);
    adapter.emit(json!({"event":"approval_resolved","approval_id":"p2"}));
    assert_eq!(adapter.tools.len(), 3);
    assert!(
        !adapter
            .permissions
            .contains_key(&key(&json!("dial-permission-p2")))
    );
}

#[test]
fn reconnect_does_not_replay_pending_replies_permissions_or_partial_tools() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = || Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut old = Adapter::new(layout());
    old.initialized = true;
    old.sessions.insert("s1".into());
    old.active = Some("s1".into());
    old.receive(json!({"jsonrpc":"2.0","id":1,"method":"session/prompt",
        "params":{"sessionId":"s1","prompt":[{"type":"text","text":"review"}]}}));
    old.emit(json!({"event":"tool_call","call_id":"a","name":"read"}));
    old.emit(json!({"event":"tool_output","call_id":"a","name":"read","delta":"partial"}));
    old.emit(json!({"event":"approval","approval_id":"p1","tool":"read","call_id":"a"}));
    assert!(!old.pending.is_empty() && !old.permissions.is_empty() && !old.tools.is_empty());
    drop(old);

    let mut fresh = Adapter::new(layout());
    assert!(!fresh.initialized);
    assert!(fresh.sessions.is_empty() && fresh.pending.is_empty());
    assert!(fresh.permissions.is_empty() && fresh.tools.is_empty());
    assert!(
        fresh
            .emit(json!({"id":1,"result":{"interrupted":true}}))
            .is_empty()
    );
    assert!(
        fresh
            .emit(json!({"event":"tool_result","call_id":"a","name":"read","text":"late"}))
            .is_empty()
    );
    let (request, replies) = fresh.receive(json!({"jsonrpc":"2.0","id":2,
        "method":"session/prompt","params":{"sessionId":"s1","prompt":[]}}));
    assert!(request.is_none());
    assert!(replies[0].get("error").is_some());
}
