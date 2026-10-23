use super::*;

fn user(text: &str) -> Value {
    json!({"role":"user","content":[{"type":"input_text","text":text}]})
}

#[test]
fn delta_requires_strict_prefix_and_equal_options() {
    let previous = json!({"model":"m","input":[user("a")]});
    let output = vec![
        json!({"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[]}),
    ];
    let replayed = json!({"type":"message","role":"assistant","content":[]});
    let current = json!({"model":"m","input":[user("a"), replayed, user("b")]});
    assert_eq!(
        delta_input(&previous, &output, &current),
        Some(vec![user("b")])
    );
    let mutated = json!({"model":"m","input":[user("x"), json!({"type":"message","role":"assistant","content":[]}), user("b")]});
    assert_eq!(delta_input(&previous, &output, &mutated), None);
    let other_model = json!({"model":"n","input":[user("a"), json!({"type":"message","role":"assistant","content":[]}), user("b")]});
    assert_eq!(delta_input(&previous, &output, &other_model), None);
}

#[test]
fn steered_plan_attaches_creates_or_discards() {
    let steer = vec![user("stop and do b")];
    let output = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
    assert!(matches!(
        plan_steered(Some(&[user("stop and do b")]), &steer),
        Plan::Attach
    ));
    assert!(
        matches!(plan_steered(Some(&[output.clone(), user("stop and do b")]), &steer), Plan::Create(rest) if rest == vec![output])
    );
    assert!(matches!(
        plan_steered(Some(&[user("stop and do b"), user("unrelated")]), &steer),
        Plan::Discard
    ));
    assert!(matches!(
        plan_steered(Some(&[user("other")]), &steer),
        Plan::Discard
    ));
    assert!(matches!(plan_steered(None, &steer), Plan::Discard));
}

#[test]
fn broken_pipe_write_is_retryable_but_protocol_errors_are_not() {
    let error = write_error(tungstenite::Error::Io(std::io::Error::from(
        ErrorKind::BrokenPipe,
    )));
    assert!(
        error
            .downcast_ref::<super::super::stream::IncompleteStream>()
            .is_some()
    );
    assert!(super::super::errors::classify(&error).is_retryable());
    let error = write_error(tungstenite::Error::Protocol(
        tungstenite::error::ProtocolError::InvalidOpcode(15),
    ));
    assert!(!super::super::errors::classify(&error).is_retryable());
}

#[test]
fn only_plain_user_messages_can_steer() {
    assert!(steer_items(&json!([user("a")])).is_some());
    assert!(
        steer_items(&json!([{"type":"function_call_output","call_id":"c","output":""}])).is_none()
    );
    assert!(steer_items(&json!([])).is_none());
}

#[test]
fn failed_websocket_usage_is_retained_for_created_and_attached_responses() {
    use super::super::usage::{Billing, Scope, Transport};
    for attached in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut socket = tungstenite::accept(listener.accept().unwrap().0).unwrap();
            if !attached {
                let request = socket.read().unwrap();
                assert!(request.into_text().unwrap().contains("response.create"));
            }
            socket.send(Message::Text(json!({
                "type":"response.failed",
                "response":{"usage":{"input_tokens":19,"output_tokens":2},"error":{"code":"server_error"}}
            }).to_string().into())).unwrap();
        });
        let tcp = TcpStream::connect(address).unwrap();
        let (mut socket, _) = tungstenite::client(
            format!("ws://{address}/responses"),
            MaybeTlsStream::Plain(setup::Transport::new(tcp, None)),
        )
        .unwrap();
        let scope = Scope::default().next_turn(0).unwrap();
        scope.begin(Billing::Subscription).unwrap();
        let transport = Transport::enter(Some(scope.clone()));
        let (_tx, rx) = std::sync::mpsc::sync_channel(STEER_QUEUE);
        let body = json!({"model":"m","input":[user("hello")]});
        let mut run = Run {
            pending: VecDeque::new(),
            accepted: Vec::new(),
            tainted: false,
            response_id: None,
        };
        assert!(
            stream(
                &mut socket,
                (!attached).then_some(&body),
                &body,
                &HashSet::new(),
                &rx,
                &mut run,
                5,
                &Arc::new(AtomicBool::new(false)),
                &mut None,
                &mut |_| true,
            )
            .is_err()
        );
        transport.settle(false).unwrap();
        scope
            .finish(None, None, Billing::Subscription, transport.attempts())
            .unwrap();
        let counts = scope.snapshot().1.all;
        assert_eq!(counts.transport_attempts, 1);
        assert_eq!(counts.partial_reported_attempts, 1);
        assert_eq!((counts.input, counts.output), (19, 2));
        assert_eq!(counts.reported_responses, 0);
        assert_eq!(counts.observed_cost_usd, None);
        server.join().unwrap();
    }
}

#[test]
fn silent_upgrade_cancellation_is_typed_and_does_not_select_sse() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/responses");
    let key = format!("cancel-setup-{address}");
    let interrupt = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupt);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut buf = [0; 4096];
        assert!(tcp.read(&mut buf).unwrap() > 0);
        flag.store(true, Ordering::Relaxed);
        // Keep the upgrade silent until the client has returned, not until the peer closes.
        done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    });
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "test",
        account_id: None,
        model: "m",
        headers: &[],
    };
    let started = Instant::now();
    let error = exchange(
        endpoint,
        &Chat {
            messages: vec![json!({"role":"user","content":"hello"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        },
        0,
        &mut None,
        interrupt,
        None,
        None,
        &mut |_| true,
        &key,
    )
    .unwrap_err();
    assert!(error.downcast_ref::<Interrupted>().is_some(), "{error:#}");
    let elapsed = started.elapsed();
    done_tx.send(()).unwrap();
    server.join().unwrap();
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert!(!SSE_ONLY.lock().contains(&format!("{key}\0\0m")));
}

#[test]
fn trickled_upgrade_cannot_extend_the_setup_deadline() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/responses");
    let server = std::thread::spawn(move || {
        let (mut tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut buf = [0; 4096];
        assert!(tcp.read(&mut buf).unwrap() > 0);
        for byte in b"HTTP/1.1 101 Switching Protocols\r\n" {
            if tcp.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(70));
        }
    });
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "test",
        account_id: None,
        model: "m",
        headers: &[],
    };
    let started = Instant::now();
    let error = connect(endpoint, "trickle", 1, &Arc::new(AtomicBool::new(false))).unwrap_err();
    let elapsed = started.elapsed();
    server.join().unwrap();
    assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert!(
        format!("{error:#}").contains("setup timed out"),
        "{error:#}"
    );
}

#[test]
fn silent_tls_handshake_can_be_cancelled() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("https://{address}/responses");
    let interrupt = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupt);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut tcp, _) = listener.accept().unwrap();
        // Native certificate loading happens before ClientHello and can be slow
        // on a cold macOS keychain; the cancellation bound is measured separately.
        tcp.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut buf = [0; 4096];
        assert!(tcp.read(&mut buf).unwrap() > 0); // TLS ClientHello, no server TLS reply.
        cancelled_tx.send(Instant::now()).unwrap();
        flag.store(true, Ordering::Relaxed);
        done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    });
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "test",
        account_id: None,
        model: "m",
        headers: &[],
    };
    let error = connect(endpoint, "tls-cancel", 0, &interrupt).unwrap_err();
    // Measure from cancellation; native certificate loading precedes ClientHello.
    let elapsed = cancelled_rx.recv().unwrap().elapsed();
    done_tx.send(()).unwrap();
    server.join().unwrap();
    assert!(error.downcast_ref::<Interrupted>().is_some(), "{error:#}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

fn streaming_loopback(
    server_body: impl FnOnce(WebSocket<TcpStream>, Arc<AtomicBool>) + Send + 'static,
    cancel: bool,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let server_flag = Arc::clone(&flag);
    let server = std::thread::spawn(move || {
        let mut socket = tungstenite::accept(listener.accept().unwrap().0).unwrap();
        assert!(socket.read().unwrap().is_text());
        socket
            .send(Message::Text(
                json!({
                    "type":"response.output_text.delta", "delta":"started"
                })
                .to_string()
                .into(),
            ))
            .unwrap();
        server_body(socket, server_flag);
    });
    let tcp = TcpStream::connect(address).unwrap();
    let (mut socket, _) = tungstenite::client(
        format!("ws://{address}/responses"),
        MaybeTlsStream::Plain(setup::Transport::new(tcp, None)),
    )
    .unwrap();
    // Cancellation and idle expiry are separate contracts. A loaded runner can
    // delay the server's cancellation past the short idle-test deadline.
    let idle_limit = (!cancel).then_some(Duration::from_millis(250));
    let _limit = super::super::cancel::TestIdleLimitScope::enter(idle_limit);
    let body = json!({"model":"m","input":[user("hello")]});
    let (_tx, rx) = std::sync::mpsc::sync_channel(STEER_QUEUE);
    let mut run = Run {
        pending: VecDeque::new(),
        accepted: Vec::new(),
        tainted: false,
        response_id: None,
    };
    let started = Instant::now();
    let error = stream(
        &mut socket,
        Some(&body),
        &body,
        &HashSet::new(),
        &rx,
        &mut run,
        1,
        &flag,
        &mut None,
        &mut |_| true,
    )
    .unwrap_err();
    let elapsed = started.elapsed();
    if cancel {
        assert!(error.downcast_ref::<Interrupted>().is_some(), "{error:#}");
    } else {
        assert!(
            error
                .downcast_ref::<super::super::stream::StalledStream>()
                .is_some(),
            "{error:#}"
        );
    }
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    drop(socket);
    server.join().unwrap();
    assert_eq!(
        Arc::strong_count(&flag),
        1,
        "transport retained request cancellation"
    );
}

fn trickle_frame(mut socket: WebSocket<TcpStream>, flag: Arc<AtomicBool>, cancel: bool) {
    use std::io::Write;
    // A text frame declaring a long payload: every TCP read succeeds, but no complete
    // WebSocket message is available. Cancellation must run below frame assembly.
    socket
        .get_mut()
        .write_all(&[0x81, 126, 0x10, 0x00])
        .unwrap();
    for index in 0..100 {
        if socket.get_mut().write_all(b" ").is_err() {
            break;
        }
        if cancel && index == 3 {
            flag.store(true, Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn trickled_stream_frame_is_cancelled_below_websocket_parser() {
    streaming_loopback(|socket, flag| trickle_frame(socket, flag, true), true);
}

#[test]
fn trickled_stream_frame_cannot_extend_progress_deadline() {
    streaming_loopback(|socket, flag| trickle_frame(socket, flag, false), false);
}

#[test]
fn ping_only_stream_expires_without_reply_progress() {
    streaming_loopback(
        |mut socket, _| {
            for _ in 0..100 {
                if socket.send(Message::Ping(vec![1].into())).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        },
        false,
    );
}

#[test]
fn cached_stream_detaches_old_flag_and_rebinds_active_cancellation() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/responses");
    let key = format!("stream-rebind-{address}");
    let socket_key = format!("{key}\0\0m");
    let old = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicBool::new(false));
    let server_flag = Arc::clone(&active);
    let server = std::thread::spawn(move || {
        let mut socket = tungstenite::accept(listener.accept().unwrap().0).unwrap();
        for id in ["one", "two"] {
            assert!(socket.read().unwrap().is_text());
            socket.send(Message::Text(json!({"type":"response.completed","response":{
                "id":id,"status":"completed","output":[{"type":"message","id":id,
                "role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]}]
            }}).to_string().into())).unwrap();
        }
        assert!(socket.read().unwrap().is_text());
        trickle_frame(socket, server_flag, true);
    });
    let endpoint = super::super::responses::Endpoint {
        url: &url,
        token: "test",
        account_id: None,
        model: "m",
        headers: &[],
    };
    let chat = Chat {
        messages: vec![json!({"role":"user","content":"hello"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let call = |flag| {
        exchange(
            endpoint,
            &chat,
            5,
            &mut None,
            flag,
            None,
            None,
            &mut |_| true,
            &key,
        )
    };
    assert_eq!(call(Arc::clone(&old)).unwrap().unwrap().text, "ok");
    assert_eq!(Arc::strong_count(&old), 1);
    old.store(true, Ordering::Relaxed);
    // Same cached connection still succeeds despite its previous flag now being cancelled.
    assert_eq!(
        call(Arc::new(AtomicBool::new(false)))
            .unwrap()
            .unwrap()
            .text,
        "ok"
    );
    assert!(SESSIONS.lock().contains_key(&socket_key));
    let error = call(Arc::clone(&active)).unwrap_err();
    assert!(error.downcast_ref::<Interrupted>().is_some(), "{error:#}");
    server.join().unwrap();
    assert_eq!(Arc::strong_count(&active), 1);
    assert!(!SESSIONS.lock().contains_key(&socket_key));
    assert!(!SSE_ONLY.lock().contains(&socket_key));
}
