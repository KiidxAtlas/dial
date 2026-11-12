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
    assert_eq!(entry("blocked")["content"], "[blocked] task (why)");
    assert_eq!(entry("abandoned")["content"], "[abandoned] task (why)");
    assert_eq!(entry("pending")["content"], "task");
}

#[test]
fn unfinished_prompts_say_so_in_the_transcript() {
    let mut adapter = initialized(json!({}));
    adapter.sessions.insert("s1".into());
    adapter.active = Some("s1".into());
    adapter.modes.insert("s1".into(), false);
    adapter.receive(json!({"jsonrpc":"2.0","id":1,"method":"session/prompt",
        "params":{"sessionId":"s1","prompt":[{"type":"text","text":"hi"}]}}));
    let replies = adapter.emit(json!({"id":1,"result":{"outcome":"needs_input","text":"which?"}}));
    let chunk = &replies[0]["params"]["update"];
    assert_eq!(chunk["sessionUpdate"], "agent_message_chunk");
    assert!(
        chunk["content"]["text"]
            .as_str()
            .unwrap()
            .contains("needs input")
    );
    assert_eq!(replies.last().unwrap()["result"]["stopReason"], "end_turn");

    adapter.receive(json!({"jsonrpc":"2.0","id":2,"method":"session/prompt",
        "params":{"sessionId":"s1","prompt":[{"type":"text","text":"hi"}]}}));
    let done = adapter.emit(json!({"id":2,"result":{"outcome":"completed"}}));
    assert_eq!(done.len(), 1);
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

fn initialized(fs: Value) -> Adapter {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut adapter = Adapter::new(layout);
    let (_, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":0,"method":"initialize",
        "params":{"protocolVersion":1,"clientCapabilities":{"fs":fs}}}));
    assert!(replies[0].get("result").is_some(), "{replies:?}");
    adapter.active = Some("s1".into());
    adapter
}

#[test]
fn initialize_records_and_reports_client_fs_bridges() {
    for (fs, caps, bridges) in [
        (
            json!({"readTextFile":true,"writeTextFile":true}),
            (true, true),
            json!(["fs/read_text_file", "fs/write_text_file"]),
        ),
        (
            json!({"writeTextFile":true}),
            (false, true),
            json!(["fs/write_text_file"]),
        ),
        // Reads alone are never bridged: an edit must commit through the same client.
        (json!({"readTextFile":true}), (true, false), json!([])),
        (Value::Null, (false, false), json!([])),
    ] {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
        let mut adapter = Adapter::new(layout);
        let (_, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":1,"clientCapabilities":{"fs":fs}}}));
        assert_eq!(adapter.client_fs(), caps);
        assert_eq!(
            replies[0]["result"]["_meta"]["dial"]["clientBridges"],
            bridges
        );
    }
}

#[test]
fn fs_requests_go_to_the_client_and_answers_return_to_the_call() {
    let mut adapter = initialized(json!({"readTextFile":true,"writeTextFile":true}));
    let read = adapter.emit(json!({"event":"client_fs_request","call_id":"host-1",
        "operation":"read","path":"/p/a.rs"}));
    assert_eq!(read.len(), 1);
    assert_eq!(read[0]["method"], "fs/read_text_file");
    assert_eq!(
        read[0]["params"],
        json!({"sessionId":"s1","path":"/p/a.rs"})
    );
    let (relay, replies) = adapter
        .receive(json!({"jsonrpc":"2.0","id":read[0]["id"],"result":{"content":"buffer text"}}));
    assert!(replies.is_empty());
    assert_eq!(
        relay.unwrap()["params"],
        json!({"call_id":"host-1","output":"buffer text","is_error":false})
    );
    // A repeated answer has no slot and is not mistaken for a permission reply.
    let (relay, _) =
        adapter.receive(json!({"jsonrpc":"2.0","id":read[0]["id"],"result":{"content":"again"}}));
    assert!(relay.is_none());

    let write = adapter.emit(json!({"event":"client_fs_request","call_id":"host-2",
        "operation":"write","path":"/p/a.rs","content":"new"}));
    assert_eq!(write[0]["method"], "fs/write_text_file");
    assert_eq!(write[0]["params"]["content"], "new");
    let (relay, _) = adapter.receive(json!({"jsonrpc":"2.0","id":write[0]["id"],
        "error":{"code":-32000,"message":"read-only buffer"}}));
    assert_eq!(
        relay.unwrap()["params"],
        json!({"call_id":"host-2","output":"read-only buffer","is_error":true})
    );

    let read = adapter.emit(json!({"event":"client_fs_request","call_id":"host-3",
        "operation":"read","path":"/p/a.rs"}));
    let (relay, _) = adapter.receive(json!({"jsonrpc":"2.0","id":read[0]["id"],"result":{}}));
    assert_eq!(relay.unwrap()["params"]["is_error"], true);
}

#[test]
fn unadvertised_fs_operations_are_never_sent() {
    let mut adapter = initialized(json!({"writeTextFile":true}));
    assert!(
        adapter
            .emit(json!({"event":"client_fs_request","call_id":"host-1","operation":"read","path":"/p/a"}))
            .is_empty()
    );
    let mut adapter = initialized(Value::Null);
    assert!(
        adapter
            .emit(json!({"event":"client_fs_request","call_id":"host-1","operation":"write","path":"/p/a","content":""}))
            .is_empty()
    );
}

#[test]
fn cancel_drops_pending_fs_requests() {
    let mut adapter = initialized(json!({"readTextFile":true,"writeTextFile":true}));
    adapter.sessions.insert("s1".into());
    let read = adapter.emit(json!({"event":"client_fs_request","call_id":"host-1",
        "operation":"read","path":"/p/a"}));
    adapter.receive(json!({"jsonrpc":"2.0","id":9,"method":"session/cancel",
        "params":{"sessionId":"s1"}}));
    let (relay, _) =
        adapter.receive(json!({"jsonrpc":"2.0","id":read[0]["id"],"result":{"content":"late"}}));
    assert!(relay.is_none());
}

#[test]
fn unknown_methods_are_method_not_found_with_or_without_params() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut adapter = Adapter::new(layout);
    adapter.receive(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
    );
    for wire in [
        json!({"jsonrpc":"2.0","id":2,"method":"bogus"}),
        json!({"jsonrpc":"2.0","id":3,"method":"bogus","params":{}}),
    ] {
        let (request, replies) = adapter.receive(wire);
        assert!(request.is_none());
        assert_eq!(replies[0]["error"]["code"], -32601, "{replies:?}");
    }
    let (_, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":4,"method":"session/new"}));
    assert_eq!(replies[0]["error"]["code"], -32602, "{replies:?}");
}

#[test]
fn model_and_thought_level_config_options_follow_actor_results() {
    let mut adapter = initialized(json!({}));
    // A session/new answer carrying the actor's `_acp_models`.
    adapter.pending.insert(
        key(&json!(1)),
        Pending {
            method: "session/new".into(),
            session: None,
        },
    );
    let opened = adapter.emit(json!({"id":1,"result":{"session_id":"s1","plan_mode":false,
        "_acp_models":{"model":"p/a","models":[{"value":"p/a","name":"A"},{"value":"p/b","name":"B"}],
        "thought_level":null,"levels":["low","high"],"settable":true}}}));
    let opened = opened.last().unwrap()["result"].clone();
    assert!(opened.get("_acp_models").is_none());
    let options = &opened["configOptions"];
    let ids: Vec<&str> = options
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mode", "model", "thought_level"]);
    assert_eq!(options[1]["currentValue"], "p/a");
    assert_eq!(options[1]["options"].as_array().unwrap().len(), 2);
    // An unset level shows as the model default, the first choice.
    assert_eq!(options[2]["currentValue"], "default");
    assert_eq!(options[2]["options"][0]["value"], "default");

    // Selecting a model maps to set_model with the provider split at the first slash.
    let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":2,
        "method":"session/set_config_option","params":{"sessionId":"s1","configId":"model","value":"p/org/b"}}));
    assert!(replies.is_empty(), "{replies:?}");
    let request = request.unwrap();
    assert_eq!(request["method"], "set_model");
    assert_eq!(request["params"]["provider"], "p");
    assert_eq!(request["params"]["model_id"], "org/b");
    // The answer keeps the mode, keeps the list and reports the new choice.
    let answered = adapter.emit(json!({"id":2,"result":{"model":"org/b",
        "_acp_models":{"model":"p/b","thought_level":"high","levels":["low","high"],"settable":true}}}));
    assert!(
        !answered
            .iter()
            .any(|message| message["params"]["update"]["sessionUpdate"] == "current_mode_update")
    );
    let result = &answered.last().unwrap()["result"]["configOptions"];
    assert_eq!(result[0]["currentValue"], "default");
    assert_eq!(result[1]["currentValue"], "p/b");
    assert_eq!(result[1]["options"].as_array().unwrap().len(), 2);
    assert_eq!(result[2]["currentValue"], "high");
    assert_eq!(result[2]["options"].as_array().unwrap().len(), 3);

    for level in ["low", "default"] {
        let (request, _) = adapter.receive(json!({"jsonrpc":"2.0","id":3,
            "method":"session/set_config_option","params":{"sessionId":"s1","configId":"thought_level","value":level}}));
        let request = request.unwrap();
        assert_eq!(request["method"], "set_thinking");
        assert_eq!(request["params"]["level"], level);
        adapter.emit(json!({"id":3,"result":{"level":null}}));
    }
    for (id, config, value) in [(5, "model", "no-provider"), (6, "temperature", "1")] {
        let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":id,
            "method":"session/set_config_option","params":{"sessionId":"s1","configId":config,"value":value}}));
        assert!(request.is_none());
        assert!(replies[0].get("error").is_some(), "{config}: {replies:?}");
    }
}

#[test]
fn prompt_results_refresh_changed_model_options_only() {
    let mut adapter = initialized(json!({}));
    adapter.sessions.insert("s1".into());
    adapter.modes.insert("s1".into(), false);
    adapter.store_models(
        "s1",
        &json!({"model":"p/a","models":[{"value":"p/a","name":"A"}],"thought_level":null,"levels":[],"settable":false}),
    );
    let prompt = |adapter: &mut Adapter, id: u64, model: &str| {
        adapter.receive(json!({"jsonrpc":"2.0","id":id,"method":"session/prompt",
            "params":{"sessionId":"s1","prompt":[{"type":"text","text":"hi"}]}}));
        adapter.emit(
            json!({"id":id,"result":{"outcome":"completed","plan_mode":false,
            "_acp_models":{"model":model,"thought_level":null,"levels":[],"settable":false}}}),
        )
    };
    assert_eq!(prompt(&mut adapter, 1, "p/a").len(), 1);
    let changed = prompt(&mut adapter, 2, "p/b");
    assert_eq!(changed.len(), 2);
    let update = &changed[0]["params"]["update"];
    assert_eq!(update["sessionUpdate"], "config_option_update");
    assert_eq!(update["configOptions"][1]["currentValue"], "p/b");
}

#[test]
fn permission_requests_offer_and_map_the_project_scope() {
    let mut adapter = initialized(json!({}));
    let request = adapter.emit(json!({"event":"approval","approval_id":"p1","tool":"bash"}));
    let options: Vec<&str> = request[0]["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["allow", "always", "project", "deny"]);
    let (decision, _) = adapter.receive(json!({"jsonrpc":"2.0","id":request[0]["id"],
        "result":{"outcome":{"outcome":"selected","optionId":"project"}}}));
    let decision = decision.unwrap();
    assert_eq!(decision["method"], "approve");
    assert_eq!(decision["params"]["decision"], "project");
}
