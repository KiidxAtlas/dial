use crate::config::tools::mcp::validate_url;

use super::*;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn sse_events_join_data_lines_and_skip_noise() {
    let body = ": keep-alive\nevent: message\ndata: {\"id\":1,\ndata: \"result\":{}}\n\ndata: not json\n\ndata:{\"id\":2}\n";
    assert_eq!(
        parse_sse(body),
        vec![json!({"id":1,"result":{}}), json!({"id":2})]
    );
}

#[test]
fn sse_event_names_reach_the_callback_and_crlf_is_accepted() {
    let body = "event: endpoint\r\ndata: /messages?session=1\r\n\r\ndata: {\"a\":1}\r\n\r\n";
    let mut seen = Vec::new();
    read_events(body.as_bytes(), &|| true, &mut |event, data| {
        seen.push((event.to_owned(), data.to_owned()));
        true
    })
    .unwrap();
    assert_eq!(
        seen,
        vec![
            ("endpoint".to_owned(), "/messages?session=1".to_owned()),
            ("message".to_owned(), "{\"a\":1}".to_owned()),
        ]
    );
}

#[test]
fn urls_must_be_https_or_loopback() {
    for ok in [
        "https://mcp.example.com/mcp",
        "http://localhost:8080/mcp",
        "http://127.0.0.1:1/x",
    ] {
        assert!(validate_url(ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://example.com/mcp",
        "ftp://x/y",
        "mcp.example.com",
        "file:///etc/passwd",
        "https://",
    ] {
        assert!(validate_url(bad).is_err(), "{bad}");
    }
}

/// An endless stream of `data:` lines with no blank line: the generator for budget tests.
struct EndlessData {
    pattern: Vec<u8>,
    served: usize,
}

impl EndlessData {
    fn new(line: &str) -> Self {
        Self {
            pattern: line.as_bytes().to_vec(),
            served: 0,
        }
    }
}

impl std::io::Read for EndlessData {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        for byte in buf.iter_mut() {
            *byte = self.pattern[self.served % self.pattern.len()];
            self.served += 1;
        }
        Ok(buf.len())
    }
}

#[test]
fn event_data_over_budget_fails_without_a_blank_line() {
    let mut delivered = 0;
    let error = read_events(
        BufReader::new(EndlessData::new(&format!("data: {}\n", "a".repeat(1000)))),
        &|| true,
        &mut |_, _| {
            delivered += 1;
            true
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(delivered, 0);
}

#[test]
fn cancellation_is_checked_between_lines_of_one_event() {
    let lines = std::cell::Cell::new(0usize);
    let keep_going = || {
        lines.set(lines.get() + 1);
        lines.get() <= 3
    };
    let mut delivered = 0;
    read_events(
        BufReader::new(EndlessData::new("data: x\n")),
        &keep_going,
        &mut |_, _| {
            delivered += 1;
            true
        },
    )
    .unwrap();
    assert_eq!(lines.get(), 4);
    assert_eq!(delivered, 0);
}

#[test]
fn stalled_legacy_stream_is_closed_at_its_lifetime() {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut socket, _)) = listener.accept() {
            let _ = socket.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: endpoint\r\ndata: /messages\r\n\r\n",
            );
            std::thread::sleep(Duration::from_secs(10));
        }
    });
    let config = McpServerConfig {
        transport: Some(McpTransport::Sse),
        ..McpServerConfig::default()
    };
    let mut http = Http::new("fake", &format!("http://{addr}/sse"), &config, None).unwrap();
    http.stream_lifetime = Duration::from_millis(300);
    let http = Arc::new(http);
    let shared = Arc::new(Shared::new(Vec::new(), None));
    let (done, finished) = mpsc::channel();
    let listener_shared = shared.clone();
    std::thread::spawn(move || {
        http.listen(listener_shared, None);
        let _ = done.send(());
    });
    finished
        .recv_timeout(Duration::from_secs(5))
        .expect("the listener releases a stalled stream at its lifetime");
    assert!(shared.is_closed());
}

#[test]
fn redirects_never_forward_credentials_or_session_ids() {
    use std::io::Write;
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let location = format!("http://{}/redirected", target.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        drain_request(&mut socket);
        write!(socket, "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let http = Http::new("redirect", &url, &McpServerConfig::default(), None).unwrap();
    *http.session.lock().unwrap() = Some("private-session".into());
    let error = http
        .post_once(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            Duration::from_secs(2),
            &mut |_| panic!("redirect is not an MCP response"),
            &[("X-Api-Key".into(), "private-token".into())],
            &Operation::new(Duration::from_secs(2)),
        )
        .unwrap_err();
    server.join().unwrap();
    assert!(error.to_string().contains("307"), "{error}");
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn authorization_errors_preserve_the_http_status() {
    let auth = Auth::new("test", &McpServerConfig::default(), None).unwrap();
    for status in [401, 403] {
        let error = auth.refused(status, Some("Bearer"));
        assert_eq!(error.downcast_ref::<AuthRequired>().unwrap().1, status);
        assert!(error.to_string().contains(&format!("HTTP {status}")));
    }
}

#[test]
fn only_a_401_bearer_challenge_offers_an_oauth_sign_in() {
    let auth = Auth::new("test", &McpServerConfig::default(), None).unwrap();
    for (status, challenge, oauth) in [
        (
            401,
            Some("Bearer resource_metadata=\"https://x/.well-known\""),
            true,
        ),
        (401, Some("Basic realm=\"x\", bearer"), true),
        (401, Some("Basic realm=\"x\""), false),
        (401, None, false),
        (403, Some("Bearer error=\"insufficient_scope\""), false),
    ] {
        let error = auth.refused(status, challenge);
        let required = error.downcast_ref::<AuthRequired>().unwrap();
        assert_eq!(required.oauth_sign_in(), oauth, "{status} {challenge:?}");
    }
    // Static credentials that are refused are wrong, not missing: no sign-in is offered.
    let fixed = McpServerConfig {
        headers: BTreeMap::from([("Authorization".into(), "Bearer static".into())]),
        ..Default::default()
    };
    let auth = Auth::new("test", &fixed, None).unwrap();
    let error = auth.refused(401, Some("Bearer"));
    assert!(error.downcast_ref::<AuthRequired>().is_none());
}

#[test]
fn streamable_get_session_termination_closes_the_shared_connection() {
    use std::io::Write;
    for known_session in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let http = Arc::new(Http::new("expired", &url, &McpServerConfig::default(), None).unwrap());
        if known_session {
            *http.session.lock().unwrap() = Some("expired-session".into());
        }
        let (signals, received) = mpsc::channel();
        let shared = Arc::new(Shared::new(Vec::new(), Some(signals)));
        if known_session {
            http.listen(shared.clone(), None);
        } else {
            let worker = http.clone();
            let state = shared.clone();
            let handle = std::thread::spawn(move || worker.listen(state, None));
            assert!(matches!(
                received.recv_timeout(Duration::from_secs(2)),
                Ok(Signal::NotificationDegraded)
            ));
            http.stop.store(true, Ordering::SeqCst);
            handle.join().unwrap();
        }
        server.join().unwrap();
        assert_eq!(shared.is_closed(), known_session);
        if known_session {
            assert!(matches!(
                received.try_recv(),
                Ok(super::super::Signal::Closed)
            ));
        } else {
            assert!(received.try_recv().is_err());
        }
    }
}

/// Consume the whole request, so the next read observes cancellation rather than request bytes.
fn drain_request(socket: &mut std::net::TcpStream) {
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(socket);
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
}

#[test]
fn notification_drop_recovers_and_shutdown_interrupts_a_silent_stream() {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        drain_request(&mut first);
        first.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: first\n\n").unwrap();
        drop(first);
        let (mut recovered, _) = listener.accept().unwrap();
        drain_request(&mut recovered);
        recovered.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: healthy\n\n").unwrap();
        let mut byte = [0];
        assert_eq!(
            recovered.read(&mut byte).unwrap(),
            0,
            "shutdown closes a silent stream's socket"
        );
    });
    let http = Arc::new(Http::new("recover", &url, &McpServerConfig::default(), None).unwrap());
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared::new(Vec::new(), Some(tx)));
    http.clone().after_initialize("2025-06-18", shared.clone());
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)),
        Ok(Signal::NotificationHealthy)
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)),
        Ok(Signal::NotificationDegraded)
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)),
        Ok(Signal::NotificationHealthy)
    ));
    let began = Instant::now();
    http.shutdown();
    assert!(began.elapsed() < Duration::from_secs(1));
    assert!(
        !shared.is_closed(),
        "optional listener loss must not close POST health"
    );
    server.join().unwrap();
}

#[test]
fn notification_405_is_unsupported_not_a_connection_failure_or_retry_loop() {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        drain_request(&mut socket);
        socket.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        listener
    });
    let http = Arc::new(Http::new("unsupported", &url, &McpServerConfig::default(), None).unwrap());
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared::new(Vec::new(), Some(tx)));
    http.clone().after_initialize("2025-06-18", shared.clone());
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)),
        Ok(Signal::NotificationUnsupported)
    ));
    http.shutdown();
    let listener = server.join().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!shared.is_closed());
}

#[test]
fn cancelled_http_request_closes_socket_and_settles_owned_worker() {
    use super::super::client::{Client, RequestFailure};
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let (entered, rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut request, _) = listener.accept().unwrap();
        drain_request(&mut request);
        entered.send(()).unwrap();
        let stalled = std::thread::spawn(move || {
            let mut byte = [0];
            assert_eq!(
                request.read(&mut byte).unwrap(),
                0,
                "cancel must close active request socket"
            );
        });
        let (mut cancellation, _) = listener.accept().unwrap();
        drain_request(&mut cancellation);
        cancellation
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        stalled.join().unwrap();
    });
    let http = Arc::new(Http::new("cancel", &url, &McpServerConfig::default(), None).unwrap());
    let client = Client::remote("cancel", http, Arc::new(Shared::new(Vec::new(), None)));
    let cancelled = AtomicBool::new(false);
    let began = Instant::now();
    let error = client
        .request_with(
            "tools/call",
            json!({}),
            Duration::from_secs(20),
            &|| {
                if rx.try_recv().is_ok() {
                    cancelled.store(true, Ordering::SeqCst);
                }
                cancelled.load(Ordering::SeqCst)
            },
            None,
        )
        .unwrap_err();
    assert!(began.elapsed() < Duration::from_secs(2));
    assert!(error.downcast_ref::<RequestFailure>().unwrap().dispatched);
    server.join().unwrap();
}
