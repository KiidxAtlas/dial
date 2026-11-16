use super::*;
use std::io::Cursor;

fn frames(input: &str, limit: usize) -> Vec<Line> {
    let (tx, rx) = mpsc::sync_channel(64);
    read_frames(Cursor::new(input.as_bytes().to_vec()), limit, &tx);
    drop(tx);
    rx.into_iter().map(|LineSlot(line)| line).collect()
}

#[test]
fn reader_decodes_lines_skips_blanks_and_always_ends() {
    let lines = frames("{\"a\":1}\r\n\n  \n{\"b\":2}", 1024);
    assert_eq!(lines.len(), 3);
    assert!(matches!(&lines[0], Line::Frame(v) if v["a"] == 1));
    assert!(matches!(&lines[1], Line::Frame(v) if v["b"] == 2));
    assert!(matches!(lines[2], Line::End));
}

#[test]
fn reader_reports_malformed_json_and_continues() {
    let lines = frames("not json\n{\"ok\":true}\n", 1024);
    assert!(matches!(&lines[0], Line::Malformed(m) if m.contains("non-JSON")));
    assert!(matches!(&lines[1], Line::Frame(v) if v["ok"] == true));
    assert!(matches!(lines[2], Line::End));
}

#[test]
fn reader_closes_on_an_oversized_frame() {
    let lines = frames("{\"x\":\"0123456789\"}\n{\"y\":1}\n", 8);
    assert_eq!(lines.len(), 2);
    assert!(matches!(&lines[0], Line::Malformed(m) if m.contains("exceeds 8 bytes")));
    assert!(matches!(lines[1], Line::End));
}

#[test]
fn replies_settle_only_their_own_request() {
    let mut pending = BTreeMap::from([(1, "state".to_owned()), (2, "send".to_owned())]);
    let ok = classify_frame(json!({"id":1,"result":{"busy":false}}), &mut pending);
    assert_eq!(
        ok,
        Message::Response {
            id: RequestId(1),
            method: "state".into(),
            result: Ok(json!({"busy":false})),
        }
    );
    let failed = classify_frame(
        json!({"id":2,"error":{"code":"busy","message":"agent busy"}}),
        &mut pending,
    );
    let Message::Response { result, .. } = failed else {
        panic!("expected a response, got {failed:?}");
    };
    assert_eq!(
        result,
        Err(RpcError {
            code: Some("busy".into()),
            message: "agent busy".into()
        })
    );
    assert!(pending.is_empty());
    assert!(matches!(
        classify_frame(json!({"id":1,"result":{}}), &mut pending),
        Message::ProtocolError(e) if e.message.contains("unknown request id 1")
    ));
}

#[test]
fn events_and_uncorrelated_errors_are_distinguished() {
    let mut pending = BTreeMap::new();
    let event = classify_frame(
        json!({"event":"approval","approval_id":"a-1","tool":"write","summary":"x.txt"}),
        &mut pending,
    );
    let Message::Event(event) = event else {
        panic!("expected an event");
    };
    assert_eq!(
        event.approval(),
        Some(ApprovalRequest {
            approval_id: "a-1".into(),
            tool: "write".into(),
            summary: "x.txt".into()
        })
    );
    assert_eq!(event.text_delta(), None);
    assert!(matches!(
        classify_frame(json!({"id":null,"error":{"message":"bad line"}}), &mut pending),
        Message::ProtocolError(e) if e.message == "bad line" && e.code.is_none()
    ));
}

#[test]
fn negotiation_picks_the_highest_common_version() {
    assert_eq!(common_version(&[1, 2]), Some(2));
    assert_eq!(common_version(&[1]), Some(1));
    assert_eq!(common_version(&[2, 3]), Some(2));
    assert_eq!(common_version(&[7]), None);
    let legacy = Ready::parse(json!({"event":"ready","protocol_version":1,"methods":["state"]}))
        .expect("legacy ready parses");
    assert_eq!(legacy.supported_protocol_versions, vec![1]);
    assert!(Ready::parse(json!({"event":"text"})).is_err());
}

#[test]
fn only_queries_are_safe_to_repeat() {
    for method in ["state", "messages_page", "list_sessions", "hello"] {
        assert_eq!(effect(method), Effect::ReadOnly, "{method}");
    }
    for method in [
        "send",
        "approve",
        "abort",
        "resume",
        "delete_session",
        "unknown",
    ] {
        assert_eq!(effect(method), Effect::MayHaveApplied, "{method}");
    }
}

#[test]
fn clip_respects_character_boundaries() {
    let text = "é".repeat(150);
    let clipped = clip(&text);
    assert!(clipped.ends_with('…'));
    assert!(clipped.len() <= 200 + '…'.len_utf8());
}

#[test]
fn error_kinds_and_retry_rules_come_from_the_shared_vocabulary() {
    let coded = RpcError::parse(&json!({"code":"not_found","message":"no such session"}));
    assert_eq!(coded.kind(), ErrorCode::NotFound);
    let newer = RpcError::parse(&json!({"code":"rate_limited","message":"later"}));
    assert_eq!(newer.kind(), ErrorCode::Failed);
    let v1 = RpcError::parse(&json!({"message":"plain"}));
    assert_eq!(v1.kind(), ErrorCode::Failed);
    for method in protocol::METHODS {
        assert_eq!(
            is_read_only(method.name),
            method.effect == protocol::Effect::ReadOnly
        );
    }
    assert!(!is_read_only("a_method_from_a_newer_engine"));
    // The client never claims a version the shared vocabulary does not define.
    assert!(
        CLIENT_PROTOCOL_VERSIONS
            .iter()
            .all(|v| protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(v))
    );
}

#[test]
fn sequence_gaps_count_lost_events_and_replies_are_unnumbered() {
    let mut last = Some(1);
    let event = |seq: u64| json!({"event": "text", "seq": seq});
    assert_eq!(skipped_events(&mut last, &event(2)), 0);
    assert_eq!(
        skipped_events(&mut last, &json!({"id": 4, "result": {}})),
        0
    );
    assert_eq!(skipped_events(&mut last, &event(5)), 2);
    assert_eq!(
        skipped_events(&mut last, &json!({"event": "text"})),
        0,
        "pre-envelope engine"
    );
    assert_eq!(last, Some(5));
    assert_eq!(skipped_events(&mut None, &event(9)), 0);
}

#[cfg(unix)]
fn engine_script(script: &str) -> (tempfile::TempDir, Client) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let client = Client::connect(&Launch::new(path), Duration::from_secs(5)).unwrap();
    (dir, client)
}

#[cfg(unix)]
#[test]
fn handshake_failure_does_not_wait_for_drop_grace() {
    use std::os::unix::fs::PermissionsExt;
    for output in [
        "",
        "printf '%s\\n' 'not-json'",
        "printf '%s\\n' '{\"event\":\"ready\",\"protocol_version\":99}'",
        "printf '%s\\n' '{\"event\":\"ready\",\"protocol_version\":2,\"methods\":[\"hello\"]}'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine");
        std::fs::write(&path, format!("#!/bin/sh\n{output}\nexec sleep 10\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let start = Instant::now();
        assert!(Client::connect(&Launch::new(path), Duration::from_millis(20)).is_err());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
    }
}

#[cfg(unix)]
#[test]
fn mcp_login_delivers_interaction_before_completion() {
    let (_dir, mut client) = engine_script(
        "printf '%s\\n' '{\"event\":\"ready\",\"protocol_version\":2,\"methods\":[\"hello\",\"mcp_login\",\"auth_code\"]}'\nIFS= read -r hello\nprintf '%s\\n' '{\"id\":1,\"result\":{\"protocol_version\":2}}'\nIFS= read -r login\nprintf '%s\\n' '{\"event\":\"auth_prompt\",\"prompt_id\":\"p1\"}'\nIFS= read -r answer\nprintf '%s\\n' '{\"id\":3,\"result\":{\"received\":true}}' '{\"id\":2,\"result\":{\"server\":\"audit\"}}'",
    );
    let login = client.mcp_login("audit").unwrap();
    let Some(Message::Event(event)) = client.poll(Duration::from_secs(2)) else {
        panic!("login prompt must be available before completion");
    };
    assert_eq!(event.kind, "auth_prompt");
    let result = client
        .call(
            "auth_code",
            json!({"prompt_id":"p1","code":"test-code"}),
            Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(result["received"], true);
    assert!(
        matches!(client.poll(Duration::from_secs(2)), Some(Message::Response { id, result: Ok(_), .. }) if id == login)
    );
}

#[cfg(unix)]
fn nonreading_engine() -> (tempfile::TempDir, Client) {
    engine_script(
        "printf '%s\\n' '{\"event\":\"ready\",\"protocol_version\":1,\"methods\":[\"send\",\"shutdown\"]}'\nexec sleep 10",
    )
}

#[cfg(unix)]
#[test]
fn call_timeout_includes_backpressured_request_write() {
    let (_dir, mut client) = nonreading_engine();
    let start = Instant::now();
    let result = client.call(
        "send",
        json!({"text":"x".repeat(2 * 1024 * 1024)}),
        Duration::from_millis(20),
    );
    let elapsed = start.elapsed();
    client.kill().unwrap();
    assert!(
        matches!(result, Err(ClientError::Timeout { .. })),
        "{result:?}"
    );
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert_eq!(client.outstanding().len(), 1);
    assert_eq!(client.outstanding()[0].effect, Effect::MayHaveApplied);
    let mut disconnected = false;
    while let Some(message) = client.poll(Duration::from_secs(2)) {
        if let Message::Disconnected { outstanding } = message {
            assert_eq!(outstanding.len(), 1);
            assert_eq!(outstanding[0].effect, Effect::MayHaveApplied);
            disconnected = true;
            break;
        }
    }
    assert!(disconnected);
}

#[cfg(unix)]
#[test]
fn request_backpressure_is_bounded_and_shutdown_can_kill_a_blocked_writer() {
    let (_dir, mut client) = nonreading_engine();
    let mut admitted = 0;
    let mut refused = false;
    for _ in 0..QUEUED_REQUESTS + 2 {
        match client.request("send", json!({"text":"x".repeat(2 * 1024 * 1024)})) {
            Ok(_) => admitted += 1,
            Err(ClientError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                refused = true;
                break;
            }
            other => panic!("unexpected admission: {other:?}"),
        }
    }
    assert!(refused, "a non-reading engine must fill the bounded queue");
    assert_eq!(client.outstanding().len(), admitted);
    assert!(admitted <= QUEUED_REQUESTS + 1);
    let start = Instant::now();
    let status = client.shutdown(Duration::from_millis(20)).unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(
        !status.success(),
        "shutdown must kill the non-reading engine"
    );
}

#[cfg(unix)]
#[test]
fn queued_requests_keep_fifo_order_and_call_preserves_other_responses() {
    let (_dir, mut client) = engine_script(
        "printf '%s\\n' '{\"event\":\"ready\",\"protocol_version\":1,\"methods\":[\"send\",\"state\"]}'\nIFS= read -r first\nIFS= read -r second\nprintf '{\"id\":1,\"result\":%s}\\n' \"$first\"\nprintf '{\"id\":2,\"result\":%s}\\n' \"$second\"",
    );
    let first = client.request("send", json!({"text":"first"})).unwrap();
    let second = client
        .call("state", json!({}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(second["method"], "state");
    let Some(Message::Response {
        id,
        result: Ok(result),
        ..
    }) = client.poll(Duration::ZERO)
    else {
        panic!("first response was lost");
    };
    assert_eq!(id, first);
    assert_eq!(result["params"]["text"], "first");
    assert!(client.outstanding().is_empty());
}

#[cfg(unix)]
#[test]
fn write_failure_reports_disconnect_even_when_stdout_stays_open() {
    let (_dir, mut client) = engine_script(
        "exec 0<&-\nprintf '%s\\n' '{\"event\":\"ready\",\"protocol_version\":1,\"methods\":[\"send\"]}'\nexec sleep 10",
    );
    let id = client.request("send", json!({"text":"hello"})).unwrap();
    let message = client.poll(Duration::from_secs(2));
    client.kill().unwrap();
    let Some(Message::Disconnected { outstanding }) = message else {
        panic!("write failure did not signal disconnect: {message:?}");
    };
    assert_eq!(outstanding.len(), 1);
    assert_eq!(outstanding[0].id, id);
    assert_eq!(outstanding[0].effect, Effect::MayHaveApplied);
    assert!(client.outstanding().is_empty());
    assert!(client.poll(Duration::ZERO).is_none());
}
