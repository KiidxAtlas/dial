use crate::config::tools::mcp::validate_url;

use super::*;
use serde_json::json;

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
