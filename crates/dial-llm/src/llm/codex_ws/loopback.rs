use super::*;
use std::net::TcpListener;

fn text(socket: &mut WebSocket<TcpStream>) -> Value {
    loop {
        if let Message::Text(text) = socket.read().unwrap() {
            return serde_json::from_str(text.as_ref()).unwrap();
        }
    }
}
fn send(socket: &mut WebSocket<TcpStream>, event: Value) {
    socket
        .send(Message::Text(event.to_string().into()))
        .unwrap();
}
fn completed(id: &str, text: &str) -> Value {
    json!({"type":"response.completed","response":{"id":id,"status":"completed","usage":{"input_tokens":1,"output_tokens":1},
        "output":[{"type":"message","id":format!("msg_{id}"),"status":"completed","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}]}})
}

fn chat() -> Chat {
    Chat {
        messages: vec![json!({"role":"user","content":"hello"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    }
}

/// Serves one stale connection (which must receive no request) and then one fresh connection
/// that answers the full replay. Returns the replayed request.
fn stale_then_fresh(listener: TcpListener) -> std::thread::JoinHandle<Value> {
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut stale = tungstenite::accept(stream).unwrap();
        loop {
            match stale.read() {
                Ok(Message::Text(text)) => panic!("stale socket received a request: {text}"),
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let (stream, _) = listener.accept().unwrap();
        let mut fresh = tungstenite::accept(stream).unwrap();
        let request = text(&mut fresh);
        send(&mut fresh, completed("r1", "ok"));
        request
    })
}

fn run_exchange(endpoint: super::super::responses::Endpoint<'_>, key: &str) -> Option<Reply> {
    exchange(
        endpoint,
        &chat(),
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        &mut |_| true,
        key,
    )
    .unwrap()
}

#[test]
fn cached_socket_broken_pipe_replays_on_a_fresh_socket_without_pinning_sse() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/codex/responses");
    let key = format!("loopback-write-{addr}");
    let socket_key = format!("{key}\0acct\0gpt-test");
    let server = stale_then_fresh(listener);
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "test-token",
        account_id: Some("acct"),
        model: "gpt-test",
        headers: &[],
    };
    let socket = connect(endpoint, &key, 5, &Arc::new(AtomicBool::new(false))).unwrap();
    let MaybeTlsStream::Plain(tcp) = socket.get_ref() else {
        panic!("loopback connection should not use TLS");
    };
    tcp.shutdown(std::net::Shutdown::Write).unwrap();
    SESSIONS
        .lock()
        .insert(socket_key.clone(), Session::new(socket));
    let reply = run_exchange(endpoint, &key).expect("replayed over WebSocket, not SSE");
    assert_eq!(reply.text, "ok");
    let request = server.join().unwrap();
    assert!(request["previous_response_id"].is_null());
    assert_eq!(request["input"][0]["role"], "user");
    assert!(!SSE_ONLY.lock().contains(&socket_key));
    assert!(SESSIONS.lock().remove(&socket_key).is_some());
}

#[test]
fn idle_cached_socket_is_replaced_before_sending() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/codex/responses");
    let key = format!("loopback-idle-{addr}");
    let socket_key = format!("{key}\0acct\0gpt-test");
    let server = stale_then_fresh(listener);
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "test-token",
        account_id: Some("acct"),
        model: "gpt-test",
        headers: &[],
    };
    let mut idle =
        Session::new(connect(endpoint, &key, 5, &Arc::new(AtomicBool::new(false))).unwrap());
    idle.idle_since = Instant::now() - MAX_IDLE_REUSE - Duration::from_secs(1);
    SESSIONS.lock().insert(socket_key.clone(), idle);
    assert_eq!(run_exchange(endpoint, &key).unwrap().text, "ok");
    assert!(server.join().unwrap()["previous_response_id"].is_null());
    assert!(!SSE_ONLY.lock().contains(&socket_key));
    assert!(SESSIONS.lock().remove(&socket_key).is_some());
}

#[test]
fn websocket_history_survives_http_fallback_and_token_refresh() {
    use std::io::{BufRead, BufReader, Read, Write};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/codex/responses");
    let key = format!("loopback-history-{addr}");
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        assert_eq!(text(&mut socket)["type"], "response.create");
        send(&mut socket, completed("ws", "first"));
        drop(socket);

        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("POST /codex/responses "));
        let mut length = None;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut body = vec![0; length.unwrap()];
        reader.read_exact(&mut body).unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["input"].as_array().unwrap().iter().any(|item| {
                item["role"] == "assistant" && item["content"][0]["text"] == "first"
            })
        );
        let events = format!("data: {}\n\n", completed("http", "second"));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            events.len(),
            events
        )
        .unwrap();
    });
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "initial-token",
        account_id: Some("acct"),
        model: "gpt-test",
        headers: &[],
    };
    let mut chat = chat();
    let reply = exchange(
        endpoint,
        &chat,
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        &mut |_| true,
        &key,
    )
    .unwrap()
    .unwrap();
    assert_eq!(reply.text, "first");
    chat.messages
        .push(json!({"role":"assistant","content":reply.text,
        "dial_provider_content":reply.provider_content}));
    chat.messages
        .push(json!({"role":"user","content":"continue"}));

    // Ownership failures must be caught before connecting, on either transport.
    let other_url = format!("{url}/other");
    let mut unverified = chat.clone();
    unverified.messages[1]["dial_provider_content"][0]
        .as_object_mut()
        .unwrap()
        .remove("scope");
    for (route, history) in [
        (
            super::super::responses::Endpoint {
                account_id: Some("other"),
                ..endpoint
            },
            &chat,
        ),
        (
            super::super::responses::Endpoint {
                url: &other_url,
                ..endpoint
            },
            &chat,
        ),
        (endpoint, &unverified),
    ] {
        let error = exchange(
            route,
            history,
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &mut |_| panic!("invalid history must not emit callbacks"),
            &key,
        )
        .unwrap_err();
        assert!(error.to_string().contains("native history belongs"));
        let error = super::super::responses::exchange_codex(
            route,
            history,
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &mut |_| panic!("invalid history must not emit callbacks"),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("native history belongs"));
    }
    forget(&key);
    let reply = super::super::responses::exchange_codex(
        super::super::responses::Endpoint {
            token: "refreshed-token",
            ..endpoint
        },
        &chat,
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        &mut |_| true,
        None,
    )
    .unwrap();
    assert_eq!(reply.text, "second");
    assert_eq!(
        reply.provider_content[0]["scope"],
        chat.messages[1]["dial_provider_content"][0]["scope"]
    );
    server.join().unwrap();
}

fn premature_close_uses_sse_on_next_attempt(graceful: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/codex/responses");
    let key = format!("loopback-close-{addr}");
    let socket_key = format!("{key}\0acct\0gpt-test");
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let (check_tx, check_rx) = std::sync::mpsc::channel();
    let (checked_tx, checked_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        assert_eq!(text(&mut socket)["type"], "response.create");
        send(
            &mut socket,
            json!({"type":"response.created","response":{"id":"partial"}}),
        );
        send(
            &mut socket,
            json!({"type":"response.output_text.delta","delta":"unfinished"}),
        );
        send(
            &mut socket,
            json!({"type":"response.output_item.added","output_index":0,
            "item":{"type":"function_call","id":"tool","call_id":"call","name":"read","arguments":""}}),
        );
        send(
            &mut socket,
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"}),
        );
        if graceful {
            socket.close(None).unwrap();
        }
        drop(socket);
        closed_tx.send(()).unwrap();
        // The failed key must select SSE without opening another TCP connection.
        check_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock));
        listener.set_nonblocking(false).unwrap();
        checked_tx.send(()).unwrap();
        // Changing just the session, account, or model must still permit WebSockets.
        for _ in 0..3 {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            assert_eq!(text(&mut socket)["type"], "response.create");
            send(&mut socket, completed("ok", "complete"));
        }
    });
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "t",
        account_id: Some("acct"),
        model: "gpt-test",
        headers: &[],
    };
    let mut seen = String::new();
    let mut on_text = |text: &str| {
        seen.push_str(text);
        true
    };
    let result = exchange(
        endpoint,
        &chat(),
        5,
        &mut Some(&mut on_text),
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        &mut |_| true,
        &key,
    );
    let error = result.expect_err("partial text and tool data must not become a Reply");
    assert!(
        error
            .downcast_ref::<super::super::stream::IncompleteStream>()
            .is_some()
    );
    assert!(error.to_string().contains(if graceful {
        "closed before"
    } else {
        "dropped before"
    }));
    assert_eq!(seen, "unfinished");
    assert!(SSE_ONLY.lock().contains(&socket_key));
    assert!(!SESSIONS.lock().contains_key(&socket_key));
    assert!(!LIVE.lock().contains_key(&key));
    closed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        exchange(
            endpoint,
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &mut |_| panic!("SSE selection must not emit callbacks"),
            &key
        )
        .unwrap()
        .is_none()
    );
    check_tx.send(()).unwrap();
    checked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let other_session = format!("{key}-other");
    for (session, account, model) in [
        (other_session.as_str(), "acct", "gpt-test"),
        (key.as_str(), "other-acct", "gpt-test"),
        (key.as_str(), "acct", "other-model"),
    ] {
        let isolated = super::super::responses::Endpoint {
            account_id: Some(account),
            model,
            ..endpoint
        };
        assert_eq!(
            exchange(
                isolated,
                &chat(),
                5,
                &mut None,
                Arc::new(AtomicBool::new(false)),
                None,
                None,
                &mut |_| true,
                session
            )
            .unwrap()
            .unwrap()
            .text,
            "complete"
        );
        assert!(
            !SSE_ONLY
                .lock()
                .contains(&format!("{session}\0{account}\0{model}"))
        );
        forget(session);
    }
    server.join().unwrap();
    SSE_ONLY.lock().remove(&socket_key);
}

#[test]
fn graceful_early_close_discards_partial_reply_and_disables_only_its_key() {
    premature_close_uses_sse_on_next_attempt(true);
}

#[test]
fn abrupt_drop_discards_partial_reply_and_disables_only_its_key() {
    premature_close_uses_sse_on_next_attempt(false);
}

#[test]
fn interrupt_and_fatal_event_do_not_disable_websockets() {
    for interrupted in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/codex/responses");
        let key = format!("loopback-nontransport-{addr}");
        let server = std::thread::spawn(move || {
            if interrupted {
                listener.set_nonblocking(true).unwrap();
                assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
                return;
            }
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            assert_eq!(text(&mut socket)["type"], "response.create");
            if !interrupted {
                send(
                    &mut socket,
                    json!({"type":"response.failed","response":{"error":{"code":"model_not_found"}}}),
                );
            }
        });
        let endpoint = super::super::responses::Endpoint {
            url: &url,
            token: "t",
            account_id: Some("acct"),
            model: "gpt-test",
            headers: &[],
        };
        let error = exchange(
            endpoint,
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(interrupted)),
            None,
            None,
            &mut |_| true,
            &key,
        )
        .err()
        .unwrap();
        assert_eq!(error.downcast_ref::<Interrupted>().is_some(), interrupted);
        assert!(
            error
                .downcast_ref::<super::super::stream::IncompleteStream>()
                .is_none()
        );
        assert!(!SSE_ONLY.lock().contains(&format!("{key}\0acct\0gpt-test")));
        assert!(!LIVE.lock().contains_key(&key));
        server.join().unwrap();
    }
}

#[test]
fn failed_steering_commit_replays_on_fresh_websocket_without_disabling_it() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/codex/responses");
    let key = format!("loopback-steer-failed-{addr}");
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        // The seeded session attaches to an accepted automatic successor, but the
        // server reports that this steering was never committed to that successor.
        send(
            &mut socket,
            json!({"type":"response.steer.failed","steer":{"id":"s1","previous_response_id":"r1"}}),
        );
        drop(socket);
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        let replay = text(&mut socket);
        assert_eq!(replay["type"], "response.create");
        assert!(replay["previous_response_id"].is_null());
        assert_eq!(replay["input"].as_array().unwrap().len(), 2);
        send(&mut socket, completed("replayed", "complete"));
    });
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "t",
        account_id: Some("acct"),
        model: "gpt-test",
        headers: &[],
    };
    let mut chat = chat();
    let mut session =
        Session::new(connect(endpoint, &key, 5, &Arc::new(AtomicBool::new(false))).unwrap());
    session.last_request =
        Some(super::super::responses::request_codex(endpoint.model, &chat, None).unwrap());
    session.last_response_id = Some("r1".into());
    session.can_append = true;
    let steering = json!({"role":"user","content":"redirect"});
    session.accepted = vec![(
        "s1".into(),
        super::super::responses::input_items(std::slice::from_ref(&steering))
            .unwrap()
            .as_array()
            .unwrap()
            .clone(),
    )];
    chat.messages.push(steering);
    let socket_key = format!("{key}\0acct\0gpt-test");
    SESSIONS.lock().insert(socket_key.clone(), session);
    let reply = exchange(
        endpoint,
        &chat,
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        &mut |_| true,
        &key,
    )
    .unwrap()
    .unwrap();
    assert_eq!(reply.text, "complete");
    assert!(!SSE_ONLY.lock().contains(&socket_key));
    server.join().unwrap();
    forget(&key);
}

/// Accepted steering governs the next request: the client reads the server's automatic
/// successor without sending, then chains the following turn on the same socket.
#[test]
fn accepted_steering_attaches_to_successor_then_chains() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        let first = text(&mut socket);
        assert_eq!(first["type"], "response.create");
        assert!(first["previous_response_id"].is_null());
        send(
            &mut socket,
            json!({"type":"response.created","response":{"id":"r1"}}),
        );
        send(
            &mut socket,
            json!({"type":"response.output_text.delta","delta":"o"}),
        );
        let steer = text(&mut socket);
        assert_eq!(steer["type"], "response.steer");
        assert_eq!(steer["previous_response_id"], "r1");
        send(
            &mut socket,
            json!({"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}),
        );
        send(&mut socket, completed("r1", "one"));
        // Automatic successor carrying the accepted steering; the client must not send for it.
        send(
            &mut socket,
            json!({"type":"response.created","response":{"id":"r2"}}),
        );
        send(&mut socket, completed("r2", "two"));
        let third = text(&mut socket);
        assert_eq!(third["type"], "response.create");
        assert_eq!(third["previous_response_id"], "r2");
        assert_eq!(third["input"], json!([{"role":"user","content":"c"}]));
        send(
            &mut socket,
            json!({"type":"response.created","response":{"id":"r3"}}),
        );
        send(&mut socket, completed("r3", "three"));
    });
    let key = format!("loopback-{}", std::process::id());
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "t",
        account_id: Some("acct"),
        model: "gpt-test",
        headers: &[],
    };
    let mut on_event = |_: &StreamEvent| true;
    let run = |chat: &Chat,
               on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
               on_event: &mut dyn FnMut(&StreamEvent) -> bool| {
        exchange(
            endpoint,
            chat,
            10,
            on_text,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            on_event,
            &key,
        )
    };
    let user = |text: &str| json!({"role":"user","content":text});
    let mut chat = Chat {
        messages: vec![user("a")],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let mut ticket = None;
    let mut steer = |_: &str| {
        if ticket.is_none() {
            ticket = submit(
                &key,
                super::super::responses::input_items(&[user("b")]).unwrap(),
            )
            .unwrap();
        }
        true
    };
    let first = run(&chat, &mut Some(&mut steer), &mut on_event)
        .unwrap()
        .unwrap();
    assert_eq!(first.text, "one");
    assert_eq!(
        ticket.unwrap().try_outcome(),
        Some(SteerOutcome::Accepted { id: "s1".into() })
    );
    chat.messages.push(
        json!({"role":"assistant","content":"one","dial_provider_content":first.provider_content}),
    );
    chat.messages.push(user("b"));
    let second = run(&chat, &mut None, &mut on_event).unwrap().unwrap();
    assert_eq!(second.text, "two");
    chat.messages.push(
        json!({"role":"assistant","content":"two","dial_provider_content":second.provider_content}),
    );
    chat.messages.push(user("c"));
    assert_eq!(
        run(&chat, &mut None, &mut on_event).unwrap().unwrap().text,
        "three"
    );
    server.join().unwrap();
    assert!(!SSE_ONLY.lock().contains(&format!("{key}\0acct\0gpt-test")));
    forget(&key);
}
