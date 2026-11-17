use super::*;
use std::io::Cursor;

fn frames(input: &str, limit: usize) -> Vec<Line> {
    let (tx, rx) = transport_queue::channel(64, TRANSPORT_BYTES, line_bytes);
    read_frames(Cursor::new(input.as_bytes().to_vec()), limit, &tx);
    drop(tx);
    std::iter::from_fn(|| rx.recv_timeout(Duration::ZERO).ok())
        .map(|LineSlot(line, _)| line)
        .collect()
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

#[cfg(unix)]
#[test]
fn refused_restart_returns_the_running_engine() {
    let (_dir, mut client) = nonreading_engine();
    client
        .request("send", json!({"text":"x"}))
        .expect("request admitted");
    let refusal = client
        .restart_and_resume(&Launch::new("/nonexistent/dial"), Duration::from_millis(20))
        .expect_err("a turn in flight refuses the restart");
    assert!(matches!(refusal.error, ClientError::Unsupported(_)));
    let mut running = refusal
        .client
        .expect("a refused restart hands the running engine back");
    assert!(!running.is_closed());
    assert_eq!(running.outstanding().len(), 1);
    running.kill().unwrap();
}

#[cfg(unix)]
#[test]
fn call_stops_reading_at_the_backlog_bound_and_keeps_every_message() {
    let events = QUEUED_FRAMES + 76;
    let script = format!(
        "printf '%s\\n' '{{\"event\":\"ready\",\"protocol_version\":1,\"methods\":[\"state\"]}}'\nIFS= read -r request\ni=0\nwhile [ $i -lt {events} ]; do printf '%s\\n' '{{\"event\":\"text\",\"delta\":\"x\"}}'; i=$((i+1)); done\nprintf '%s\\n' '{{\"id\":1,\"result\":{{\"ok\":true}}}}'"
    );
    let (_dir, mut client) = engine_script(&script);
    let result = client.call("state", json!({}), Duration::from_secs(5));
    assert!(
        matches!(result, Err(ClientError::Backlog { .. })),
        "{result:?}"
    );
    let mut texts = 0;
    let mut reply = None;
    while let Some(message) = client.poll(Duration::from_secs(5)) {
        match message {
            Message::Event(event) if event.kind == "text" => texts += 1,
            Message::Response { result, .. } => {
                reply = Some(result);
                break;
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert_eq!(texts, events, "no deferred event may be dropped");
    assert_eq!(reply, Some(Ok(json!({"ok":true}))));
}

#[cfg(unix)]
#[test]
fn call_bounds_deferred_bytes_not_only_message_count() {
    // 40 MiB of unrelated events is far below the message-count bound, so only the byte bound can
    // stop this call from buffering the whole stream.
    let script = "printf '%s\\n' '{\"event\":\"ready\",\"protocol_version\":1,\"methods\":[\"state\"]}'\nIFS= read -r request\nbig=$(head -c 1048576 /dev/zero | tr '\\0' 'x')\ni=0\nwhile [ $i -lt 40 ]; do printf '{\"event\":\"text\",\"delta\":\"%s\"}\\n' \"$big\"; i=$((i+1)); done\nprintf '%s\\n' '{\"id\":1,\"result\":{\"ok\":true}}'";
    let (_dir, mut client) = engine_script(script);
    let result = client.call("state", json!({}), Duration::from_secs(20));
    assert!(
        matches!(result, Err(ClientError::Backlog { .. })),
        "a 40 MiB backlog must hit the byte bound: {result:?}"
    );
    let mut texts = 0;
    let mut reply = None;
    while let Some(message) = client.poll(Duration::from_secs(10)) {
        match message {
            Message::Event(event) if event.kind == "text" => texts += 1,
            Message::Response { result, .. } => {
                reply = Some(result);
                break;
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert_eq!(texts, 40, "no deferred event may be dropped");
    assert_eq!(reply, Some(Ok(json!({"ok":true}))));
}

/// Writes an executable engine script named `name` in `dir` and returns its launch.
#[cfg(unix)]
fn write_engine(dir: &tempfile::TempDir, name: &str, script: &str) -> Launch {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.path().join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    Launch::new(path)
}

/// An old engine that defers events inside its own `state` and `shutdown` calls and then exits.
/// `seq` skips 4 and 5 before the deferred `c`, so the old connection reports two lost events.
#[cfg(unix)]
const RESTARTABLE_ENGINE: &str = r##"printf '%s\n' '{"event":"ready","seq":1,"session_id":"s1","protocol_version":1,"methods":["state","shutdown"]}'
IFS= read -r state
printf '%s\n' '{"event":"text","seq":2,"session_id":"s1","delta":"a"}' '{"event":"text","seq":3,"session_id":"s1","delta":"b"}' '{"id":1,"result":{"session_id":"s1"}}'
IFS= read -r shutdown
printf '%s\n' '{"event":"text","seq":6,"session_id":"s1","delta":"c"}' '{"id":2,"result":{}}' '{"event":"text","seq":7,"session_id":"s1","delta":"d"}'"##;

/// A replacement engine that announces `session` and then serves one more event before waiting.
#[cfg(unix)]
fn replacement_engine(session: &str, methods: &str) -> String {
    format!(
        "printf '%s\\n' '{{\"event\":\"ready\",\"seq\":1,\"session_id\":\"{session}\",\"protocol_version\":1,\"methods\":[{methods}]}}' '{{\"event\":\"text\",\"seq\":2,\"session_id\":\"{session}\",\"delta\":\"e\"}}'\nexec cat >/dev/null"
    )
}

/// Text deltas of the event messages, in order; other message kinds are skipped.
#[cfg(unix)]
fn deltas(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Event(event) => event.text_delta(),
            _ => None,
        })
        .collect()
}

/// Polls a handoff to its end. A timeout here would mean the old connection never ended, so it
/// fails the test instead of ending the drain early.
#[cfg(unix)]
fn drain(handoff: &mut Handoff) -> Vec<Message> {
    let mut messages = Vec::new();
    loop {
        match handoff.poll(Duration::from_secs(5)) {
            HandoffPoll::Message(message) => messages.push(message),
            HandoffPoll::Finished => return messages,
            HandoffPoll::TimedOut => panic!("the old connection never ended: {messages:?}"),
        }
    }
}

#[cfg(unix)]
#[test]
fn a_successful_restart_hands_over_every_unpolled_message_in_order() {
    let (dir, client) = engine_script(RESTARTABLE_ENGINE);
    let next = write_engine(
        &dir,
        "next",
        &replacement_engine("s1", "\"state\",\"shutdown\""),
    );
    let mut restarted = client
        .restart_and_resume(&next, Duration::from_secs(5))
        .expect("a restart whose replacement starts succeeds");
    let messages = drain(&mut restarted.handoff);
    // a and b (deferred by `state`), c (deferred by `shutdown`), d (after its reply), then the end.
    assert_eq!(messages.len(), 5, "{messages:?}");
    assert_eq!(deltas(&messages), ["a", "b", "c", "d"]);
    assert!(matches!(
        messages.last(),
        Some(Message::Disconnected { outstanding }) if outstanding.is_empty()
    ));
    assert!(restarted.handoff.is_finished());
    assert_eq!(
        restarted.handoff.lost_events(),
        2,
        "the old connection skipped seq 4 and 5"
    );
    assert_eq!(restarted.client.ready().session_id, "s1");
    assert_eq!(
        restarted.client.lost_events(),
        0,
        "a sequence space starts fresh"
    );
    // The replacement's own messages carry its own sequence numbers, separate from the handoff.
    let Some(Message::Event(event)) = restarted.client.poll(Duration::from_secs(2)) else {
        panic!("the replacement's event must be polled after the handoff");
    };
    assert_eq!((event.seq(), event.text_delta()), (Some(2), Some("e")));
    restarted.client.kill().unwrap();
}

#[cfg(unix)]
#[test]
fn a_failed_restart_after_shutdown_still_hands_over_the_unpolled_messages() {
    let (_dir, client) = engine_script(RESTARTABLE_ENGINE);
    let mut error = client
        .restart_and_resume(&Launch::new("/nonexistent/dial"), Duration::from_secs(1))
        .expect_err("no replacement can start");
    assert!(error.client.is_none(), "the old engine is gone");
    let mut handoff = error.handoff.take().expect("the old connection's handoff");
    assert_eq!(deltas(&drain(&mut handoff)), ["a", "b", "c", "d"]);
    assert_eq!(handoff.lost_events(), 2);
}

#[cfg(unix)]
#[test]
fn a_failed_resume_hands_back_the_replacement_connection() {
    let (dir, client) = engine_script(RESTARTABLE_ENGINE);
    // The replacement serves no `resume`, so it cannot take over session s1.
    let next = write_engine(
        &dir,
        "next",
        &replacement_engine("s2", "\"state\",\"shutdown\""),
    );
    let mut error = client
        .restart_and_resume(&next, Duration::from_secs(5))
        .expect_err("the replacement cannot resume s1");
    assert!(
        matches!(error.error, ClientError::Unsupported(_)),
        "{:?}",
        error.error
    );
    let mut handoff = error.handoff.take().expect("the old connection's handoff");
    assert_eq!(deltas(&drain(&mut handoff)), ["a", "b", "c", "d"]);
    let mut running = error
        .client
        .expect("the replacement keeps running for a retry");
    assert_eq!(running.ready().session_id, "s2");
    assert!(!running.is_closed());
    running.kill().unwrap();
}

#[cfg(unix)]
#[test]
fn unread_shutdown_output_refuses_the_restart_and_keeps_every_message() {
    // 3000 lines of about 1 KB each: far more than the bounded queue and the pipe can hold, so the
    // engine cannot exit until its output is read, and the restart must refuse rather than kill it.
    let ready = r#"{"event":"ready","seq":1,"session_id":"s1","protocol_version":1,"methods":["state","shutdown"]}"#;
    let line = format!("{{\"event\":\"text\",\"delta\":\"{}\"}}", "x".repeat(1024));
    let script = format!(
        "printf '%s\\n' '{ready}'\nIFS= read -r state\nprintf '%s\\n' '{{\"id\":1,\"result\":{{\"session_id\":\"s1\"}}}}'\nIFS= read -r shutdown\nprintf '%s\\n' '{{\"id\":2,\"result\":{{}}}}'\nyes '{line}' | head -n 3000\nexit 0"
    );
    let (dir, client) = engine_script(&script);
    let next = write_engine(
        &dir,
        "next",
        &replacement_engine("s1", "\"state\",\"shutdown\""),
    );
    let refusal = client
        .restart_and_resume(&next, Duration::from_secs(10))
        .expect_err("unread output must refuse the restart");
    assert!(
        matches!(refusal.error, ClientError::Backlog { .. }),
        "{:?}",
        refusal.error
    );
    assert!(
        refusal.handoff.is_none(),
        "the old connection still owns its buffer"
    );
    let mut running = *refusal.client.expect("the old engine keeps running");
    assert!(
        matches!(
            running.request("state", json!({})),
            Err(ClientError::Disconnected { .. })
        ),
        "a winding-down engine takes no requests"
    );
    assert!(!running.is_closed());
    let (mut events, mut ended) = (0, false);
    while let Some(message) = running.poll(Duration::from_secs(5)) {
        match message {
            Message::Event(event) if event.text_delta().is_some() => events += 1,
            Message::Disconnected { outstanding } => {
                assert!(outstanding.is_empty(), "{outstanding:?}");
                ended = true;
                break;
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert_eq!(events, 3000, "no unread line may be dropped");
    assert!(ended, "the engine's end must follow its output");
    assert!(
        running
            .shutdown(Duration::from_secs(5))
            .expect("the drained engine exits")
            .success()
    );
}

#[cfg(unix)]
#[test]
fn output_from_a_grandchild_holding_the_pipe_is_handed_over_in_steps() {
    // The engine exits at once, but a background child keeps stdout open and writes one event
    // later. The restart must not wait for that end of stream, and the handoff reports the wait
    // as TimedOut rather than as finished.
    let ready = r#"{"event":"ready","seq":1,"session_id":"s1","protocol_version":1,"methods":["state","shutdown"]}"#;
    let state_reply = r#"{"id":1,"result":{"session_id":"s1"}}"#;
    let shutdown_reply = r#"{"id":2,"result":{}}"#;
    let late = r#"{"event":"text","seq":2,"session_id":"s1","delta":"late"}"#;
    let script = format!(
        "printf '%s\\n' '{ready}'\nIFS= read -r state\nprintf '%s\\n' '{state_reply}'\nIFS= read -r shutdown\nprintf '%s\\n' '{shutdown_reply}'\n(sleep 2; printf '%s\\n' '{late}') &\nexit 0"
    );
    let (dir, client) = engine_script(&script);
    let next = write_engine(
        &dir,
        "next",
        &replacement_engine("s1", "\"state\",\"shutdown\""),
    );
    let started = Instant::now();
    let mut restarted = client
        .restart_and_resume(&next, Duration::from_secs(5))
        .expect("the restart completes while the pipe is still open");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "restart waited for the grandchild: {:?}",
        started.elapsed()
    );
    assert!(
        matches!(
            restarted.handoff.poll(Duration::ZERO),
            HandoffPoll::TimedOut
        ),
        "the late line has not arrived, and the handoff is not finished"
    );
    assert!(!restarted.handoff.is_finished());
    let messages = drain(&mut restarted.handoff);
    assert_eq!(deltas(&messages), ["late"], "{messages:?}");
    assert!(matches!(
        messages.last(),
        Some(Message::Disconnected { .. })
    ));
    assert_eq!(messages.len(), 2);
    restarted.client.kill().unwrap();
}

#[cfg(unix)]
#[test]
fn an_exited_engine_with_a_full_buffer_hands_over_its_messages() {
    // No `shutdown` method is advertised, so the wind-down sends nothing and only waits for the
    // exit. The exit must win over the full queue: a refusal here would tell the caller to drain
    // a process that is already gone.
    let (_dir, mut client) = engine_script(
        "printf '%s\\n' '{\"event\":\"ready\",\"protocol_version\":1,\"methods\":[\"state\"]}'",
    );
    client.child.as_mut().unwrap().wait().unwrap();
    let total = DEFERRED_MESSAGES + 76;
    for n in 0..total {
        let body = json!({"event":"text","delta":n.to_string()});
        client.buffered.push_back(sized(Message::Event(Event {
            kind: "text".into(),
            body,
        })));
    }
    assert!(
        client.buffer_full(),
        "the queue must already be at its bound"
    );
    client
        .wind_down_for_restart(Duration::from_secs(5))
        .expect("an exited engine hands its buffer over instead of refusing");
    let mut handoff = Handoff {
        connection: Box::new(client),
        ended: false,
    };
    let messages = drain(&mut handoff);
    let expected: Vec<String> = (0..total).map(|n| n.to_string()).collect();
    let got: Vec<String> = deltas(&messages).into_iter().map(str::to_owned).collect();
    assert_eq!(got, expected, "every buffered message, in order");
    assert!(matches!(
        messages.last(),
        Some(Message::Disconnected { outstanding }) if outstanding.is_empty()
    ));
    assert_eq!(messages.len(), total + 1);
}

#[cfg(unix)]
#[test]
fn dropping_an_unresponsive_engine_does_not_block_the_owner() {
    let (_dir, mut client) = nonreading_engine();
    client.drop_grace = Duration::from_millis(400);
    let start = Instant::now();
    drop(client);
    assert!(start.elapsed() < Duration::from_millis(200));
}

#[cfg(unix)]
#[test]
fn recovery_response_reconciles_a_gap_using_the_server_wire_contract() {
    let snapshot: Value =
        serde_json::from_str(include_str!("../tests/fixtures/recovery_snapshot.json")).unwrap();
    let reply = json!({"id":1,"result":snapshot});
    let script = format!(
        r#"printf '%s\n' '{{"event":"ready","session_id":"s1","seq":1,"protocol_version":1,"methods":["recovery_snapshot"]}}' '{{"event":"approval","seq":2,"approval_id":"a","tool":"read","summary":"read","call_id":"c"}}' '{{"event":"todo","seq":4,"tasks":[{{"content":"stale task"}}]}}'
IFS= read -r request
printf '%s\n' '{reply}' '{{"event":"notice","seq":7,"message":"after snapshot"}}'
exec cat >/dev/null"#
    );
    let (_dir, mut client) = engine_script(&script);
    let mut view = Conversation::new();
    view.new_connection(client.ready());
    for _ in 0..2 {
        view.apply(&client.poll(Duration::from_secs(2)).unwrap());
    }
    assert!(view.needs_reload());
    assert_eq!(view.pending_approvals()[0].request.approval_id, "a");
    view.load_history(Some("s1"), &[]);
    assert!(view.needs_reload());
    let id = client.request("recovery_snapshot", json!({})).unwrap();
    let message = client.poll(Duration::from_secs(2)).unwrap();
    let Message::Response {
        id: response_id,
        result: Ok(result),
        ..
    } = message
    else {
        panic!("expected recovery response")
    };
    assert_eq!(id, response_id);
    assert!(view.load_snapshot(&result));
    assert!(!view.needs_reload());
    assert!(!view.turn_running());
    assert!(view.pending_approvals().is_empty());
    assert!(view.question().is_none());
    assert_eq!(view.todo()[0]["content"], "current task");
    assert_eq!(view.entries().len(), 2);
    let Entry::Tool(tool) = &view.entries()[1] else {
        panic!("expected recovered tool")
    };
    assert_eq!(tool.result.as_ref().unwrap().status, None);
    assert_eq!(tool.result.as_ref().unwrap().succeeded(), None);
    view.apply(&client.poll(Duration::from_secs(2)).unwrap());
    assert!(
        !view.needs_reload(),
        "snapshot sequence becomes the new baseline"
    );
    assert!(
        matches!(view.entries().last(), Some(Entry::Notice(notice)) if notice.message == "after snapshot")
    );
}
