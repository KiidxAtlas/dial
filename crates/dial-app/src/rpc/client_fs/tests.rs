use super::*;

#[test]
fn unknown_or_repeated_answers_are_refused() {
    let calls = ClientFsCalls::default();
    assert!(calls.complete("host-1", "text".into(), false).is_err());
}

#[test]
fn uncertain_write_requires_late_acknowledgement_and_ordered_read_before_retry() {
    let calls = ClientFsCalls::default();
    let path = Path::new("/buffer.rs");
    let interrupt = AtomicBool::new(false);
    let mut id = String::new();
    let first = calls.write_call(
        path,
        &interrupt,
        Duration::ZERO,
        |call_id| {
            id = call_id.to_owned();
            true
        },
        || false,
    );
    assert_eq!(
        first.execution.effects,
        crate::runtime::execution::Effects::Unknown
    );
    let retry = || {
        calls.write_call(
            path,
            &interrupt,
            Duration::ZERO,
            |_| panic!("unreconciled retry was delivered"),
            || false,
        )
    };
    assert!(retry().is_error);
    assert!(
        calls.acknowledged(path).is_none(),
        "a read before acknowledgement cannot release the barrier"
    );
    calls.complete(&id, String::new(), false).unwrap();
    assert!(
        calls.complete(&id, String::new(), false).is_err(),
        "duplicate acknowledgement must be refused"
    );
    assert!(
        retry().is_error,
        "acknowledgement alone is not a fresh read"
    );
    let read_started_after_ack = calls.acknowledged(path).unwrap();
    calls.reconcile(path, &read_started_after_ack);
    let next = calls.write_call(
        path,
        &interrupt,
        Duration::from_secs(1),
        |id| {
            calls.complete(id, String::new(), false).unwrap();
            true
        },
        || false,
    );
    assert!(!next.is_error, "{}", next.text);
    assert!(calls.writes.lock().unwrap().is_empty());
}

#[test]
fn undelivered_write_does_not_leave_a_retry_barrier() {
    let calls = ClientFsCalls::default();
    let out = calls.write_call(
        Path::new("/buffer.rs"),
        &AtomicBool::new(false),
        Duration::ZERO,
        |_| false,
        || false,
    );
    assert!(out.is_error);
    assert_eq!(
        out.execution.effects,
        crate::runtime::execution::Effects::None
    );
    assert!(calls.writes.lock().unwrap().is_empty());
}

#[test]
fn acp_late_write_reply_survives_prompt_end_and_cancel_for_reconciliation() {
    use crate::acp::Adapter;
    use crate::config::paths::Layout;
    for cancel in [false, true] {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
        let mut adapter = Adapter::new(layout);
        adapter.receive(json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true}}}}));
        let (new, _) = adapter.receive(json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd":project.path().to_str().unwrap(),"mcpServers":[]}}));
        assert!(new.is_some());
        adapter.emit(json!({"id":1,"result":{"session_id":"s1"}}));
        let (prompt, _) = adapter.receive(json!({"jsonrpc":"2.0","id":2,"method":"session/prompt",
            "params":{"sessionId":"s1","prompt":[{"type":"text","text":"edit"}]}}));
        assert!(prompt.is_some());
        let calls = ClientFsCalls::default();
        let path = project.path().join("buffer.rs");
        let interrupt = AtomicBool::new(false);
        let mut wire_id = serde_json::Value::Null;
        let first = calls.write_call(
            &path,
            &interrupt,
            Duration::ZERO,
            |call| {
                let wire = adapter.emit(json!({"event":"client_fs_request","call_id":call,
                "operation":"write","path":path,"content":"applied buffer"}));
                wire_id = wire[0]["id"].clone();
                true
            },
            || false,
        );
        assert!(first.is_error);
        if cancel {
            adapter.receive(json!({"jsonrpc":"2.0","id":3,"method":"session/cancel","params":{"sessionId":"s1"}}));
        }
        adapter.emit(json!({"id":2,"result":{"outcome":"failed","interrupted":cancel}}));
        let (relay, _) = adapter.receive(json!({"jsonrpc":"2.0","id":wire_id,"result":{}}));
        let relay =
            relay.expect("late write acknowledgement must survive prompt completion/cancel");
        calls
            .complete(
                relay["params"]["call_id"].as_str().unwrap(),
                String::new(),
                false,
            )
            .unwrap();
        let read = calls.read_call(&path, &interrupt, Duration::from_secs(1), |call| {
            let wire = adapter.emit(json!({"event":"client_fs_request","call_id":call,"operation":"read","path":path}));
            let (relay, _) = adapter.receive(json!({"jsonrpc":"2.0","id":wire[0]["id"],"result":{"content":"applied buffer"}}));
            let relay = relay.unwrap();
            calls.complete(relay["params"]["call_id"].as_str().unwrap(), relay["params"]["output"].as_str().unwrap().into(), false).unwrap();
            true
        }, || false);
        assert_eq!(read.text, "applied buffer");
        let retry = calls.write_call(
            &path,
            &interrupt,
            Duration::from_secs(1),
            |call| {
                calls.complete(call, String::new(), false).unwrap();
                true
            },
            || false,
        );
        assert!(!retry.is_error, "{}", retry.text);
    }
}
