//! Loopback fixtures for the native image transport: origin and download rules, credential
//! placement, bounded bodies, no redirects and total deadlines. No live service is contacted.
use super::*;
use crate::llm::model_config::{HeaderValue, RequestHeader};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

const KEY: &str = "sk-test-key";

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    location: Option<String>,
    silent: bool,
}

fn json_reply(status: u16, value: &Value) -> Reply {
    Reply {
        status,
        content_type: "application/json",
        body: value.to_string().into_bytes(),
        location: None,
        silent: false,
    }
}

fn bytes_reply(status: u16, body: Vec<u8>) -> Reply {
    Reply {
        status,
        content_type: "application/octet-stream",
        body,
        location: None,
        silent: false,
    }
}

fn redirect(location: &str) -> Reply {
    Reply {
        status: 302,
        content_type: "text/plain",
        body: Vec::new(),
        location: Some(location.into()),
        silent: false,
    }
}

/// Accepts the connection, reads the request, and never answers.
fn silent() -> Reply {
    Reply {
        status: 200,
        content_type: "text/plain",
        body: Vec::new(),
        location: None,
        silent: true,
    }
}

fn read_request(stream: &TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut raw = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        let end = line == "\r\n" || line.is_empty();
        raw.push_str(&line);
        if end {
            break;
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    raw.push_str(&String::from_utf8_lossy(&body));
    raw
}

fn write_reply(mut stream: TcpStream, reply: Reply) {
    if reply.silent {
        std::thread::sleep(Duration::from_secs(20));
        return;
    }
    let location = reply
        .location
        .map(|value| format!("Location: {value}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {} X\r\nContent-Type: {}\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    // A client may stop reading early (size limits); a closed socket is not a server failure.
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&reply.body);
}

/// Answers the given replies in order on loopback. Returns every request received, including any
/// connection beyond the scripted ones, so a retry shows up as an extra entry.
fn serve(replies: Vec<Reply>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let total = replies.len();
    let handle = std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        let mut seen = Vec::new();
        let mut answered = 0usize;
        let mut last = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    seen.push(read_request(&stream));
                    if let Some(reply) = replies.next() {
                        answered += 1;
                        write_reply(stream, reply);
                    }
                    last = Instant::now();
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    let idle = last.elapsed();
                    if idle > Duration::from_secs(3)
                        || (answered == total && idle > Duration::from_millis(300))
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        seen
    });
    (base, handle)
}

/// A loopback address with no server behind it; any connection is recorded by `assert_no_connection`.
fn silent_listener() -> (String, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    (
        format!("http://{}", listener.local_addr().unwrap()),
        listener,
    )
}

fn assert_no_connection(listener: &TcpListener) {
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        other => panic!("unexpected connection: {:?}", other.map(|(_, peer)| peer)),
    }
}

fn target(base: &str, wire: Wire, key: &str) -> Target {
    Target {
        base_url: base.into(),
        model: "test-model".into(),
        wire,
        key: key.into(),
        account_id: None,
        headers: Vec::new(),
        settings: Default::default(),
    }
}

fn custom_header() -> RequestHeader {
    RequestHeader {
        name: "x-custom-route".into(),
        value: HeaderValue::Literal("route-1".into()),
    }
}

fn config(timeout_secs: u64) -> Config {
    let mut config = Config::default();
    config.image.timeout_secs = timeout_secs;
    config
}

fn client_for<'a>(target: &'a Target, config: &'a Config) -> Client<'a> {
    Client::new(target, config, Arc::new(AtomicBool::new(false)))
}

/// Whether the request head contains exactly this header line (compared lowercase).
fn has_header(raw: &str, line: &str) -> bool {
    raw.split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .lines()
        .any(|found| found == line)
}

fn body_json(raw: &str) -> Value {
    let (_, payload) = raw.split_once("\r\n\r\n").expect("request body");
    serde_json::from_str(payload).expect("JSON request body")
}

#[test]
fn same_origin_service_urls_are_accepted_for_every_wire() {
    for wire in [
        Wire::OpenAi,
        Wire::Gemini,
        Wire::BlackForestLabs,
        Wire::Fal,
        Wire::Replicate,
        Wire::ComfyUi,
    ] {
        validate_service_url(
            "https://api.example.com/v1",
            "https://api.example.com/v1/prompt",
            wire,
        )
        .unwrap_or_else(|error| panic!("{wire:?}: {error:#}"));
    }
}

#[test]
fn cross_origin_polling_urls_are_refused_except_bfl_regional_hosts() {
    for wire in [
        Wire::OpenAi,
        Wire::Xai,
        Wire::Gemini,
        Wire::Fal,
        Wire::Replicate,
        Wire::ComfyUi,
    ] {
        for next in [
            "https://evil.example.com/v1/x",
            "https://api.example.com:8443/v1/x",
            "http://api.example.com/v1/x",
        ] {
            assert!(
                validate_service_url("https://api.example.com/v1", next, wire).is_err(),
                "{wire:?} must refuse {next}"
            );
        }
    }
}

#[test]
fn bfl_regional_polling_is_allowed_only_between_bfl_https_origins() {
    validate_service_url(
        "https://api.bfl.ai/v1",
        "https://api.eu.bfl.ai/v1/get_result?id=1",
        Wire::BlackForestLabs,
    )
    .unwrap();
    // The default port is the same origin as an omitted port.
    validate_service_url(
        "https://api.bfl.ai/v1",
        "https://api.bfl.ai:443/v1/x",
        Wire::BlackForestLabs,
    )
    .unwrap();
    for next in [
        "https://evil.example.com/v1/get_result",
        "http://api.eu.bfl.ai/v1/x",
        "https://api.eu.bfl.ai:8443/v1/x",
        "https://bfl.ai.evil.com/x",
    ] {
        assert!(
            validate_service_url("https://api.bfl.ai/v1", next, Wire::BlackForestLabs).is_err(),
            "BFL must refuse {next}"
        );
    }
    // The regional exception belongs to BFL alone, and needs a BFL base.
    assert!(
        validate_service_url(
            "https://queue.fal.run",
            "https://api.eu.bfl.ai/x",
            Wire::Fal
        )
        .is_err()
    );
    assert!(
        validate_service_url(
            "https://api.example.com/v1",
            "https://api.bfl.ai/x",
            Wire::BlackForestLabs
        )
        .is_err()
    );
}

#[test]
fn service_urls_reject_user_information_and_fragments() {
    assert!(
        validate_service_url(
            "https://key:pw@api.example.com/v1",
            "https://api.example.com/v1/x",
            Wire::OpenAi
        )
        .is_err()
    );
    assert!(
        validate_service_url(
            "https://api.example.com/v1",
            "https://user@api.example.com/v1/x",
            Wire::OpenAi
        )
        .is_err()
    );
    assert!(
        validate_service_url(
            "https://api.example.com/v1",
            "https://api.example.com/v1/x#frag",
            Wire::OpenAi
        )
        .is_err()
    );
}

#[test]
fn plain_http_service_urls_are_allowed_only_for_loopback_services() {
    validate_service_url(
        "http://127.0.0.1:9/v1",
        "http://127.0.0.1:9/v1/prompt",
        Wire::ComfyUi,
    )
    .unwrap();
    assert!(
        validate_service_url(
            "http://api.example.com/v1",
            "http://api.example.com/v1/x",
            Wire::OpenAi
        )
        .is_err()
    );
    assert!(
        validate_service_url("not a url", "https://api.example.com/v1/x", Wire::OpenAi).is_err()
    );
}

#[test]
fn downloads_from_the_service_origin_are_allowed() {
    validate_download_url(
        "https://api.example.com/v1",
        "https://api.example.com/v1/files/a.png",
        Wire::Fal,
    )
    .unwrap();
}

#[test]
fn downloads_follow_each_wire_delivery_allow_list() {
    let allowed = [
        (Wire::Fal, "https://fal.media/a.png"),
        (Wire::Fal, "https://v3.fal.media/files/a.png"),
        (Wire::Replicate, "https://replicate.delivery/a.png"),
        (Wire::Replicate, "https://pbxt.replicate.delivery/a.png"),
        (Wire::BlackForestLabs, "https://delivery.bfl.ai/a.png"),
    ];
    for (wire, url) in allowed {
        validate_download_url("https://api.example.com/v1", url, wire)
            .unwrap_or_else(|error| panic!("{wire:?} {url}: {error:#}"));
    }
    let refused = [
        (Wire::Fal, "https://evilfal.media/a.png"),
        (Wire::Fal, "https://fal.media.evil.example/a.png"),
        (
            Wire::Replicate,
            "https://replicate.delivery.evil.example/a.png",
        ),
        (Wire::Replicate, "https://evil.example/a.png"),
        (Wire::BlackForestLabs, "https://api.bfl.ai/a.png"),
        (
            Wire::BlackForestLabs,
            "https://delivery.bfl.ai.evil.example/a.png",
        ),
        // A host allowed for one provider is not allowed for another.
        (Wire::OpenAi, "https://fal.media/a.png"),
        (Wire::Fal, "https://replicate.delivery/a.png"),
    ];
    for (wire, url) in refused {
        assert!(
            validate_download_url("https://api.example.com/v1", url, wire).is_err(),
            "{wire:?} must refuse {url}"
        );
    }
}

#[test]
fn downloads_require_https_on_port_443_without_credentials() {
    for url in [
        "http://fal.media/a.png",
        "https://fal.media:8443/a.png",
        "https://user:pw@fal.media/a.png",
    ] {
        assert!(
            validate_download_url("https://api.example.com/v1", url, Wire::Fal).is_err(),
            "must refuse {url}"
        );
    }
    assert!(
        validate_download_url(
            "https://api.example.com/v1",
            "http://127.0.0.1:9/a.png",
            Wire::Fal
        )
        .is_err()
    );
}

#[test]
fn rejected_origins_never_open_a_connection() {
    let (base, listener) = silent_listener();
    let target = target(&base, Wire::Fal, KEY);
    let config = config(5);
    let client = client_for(&target, &config);
    assert!(client.get("https://evil.example.com/v1/status").is_err());
    assert!(
        client
            .post("https://evil.example.com/v1/prompt", &json!({}))
            .is_err()
    );
    assert!(client.download("https://evil.example.com/a.png").is_err());
    assert_no_connection(&listener);
}

#[test]
fn json_requests_send_only_the_wire_credential_and_configured_headers() {
    let cases = [
        (Wire::Gemini, "x-goog-api-key: sk-test-key"),
        (Wire::BlackForestLabs, "x-key: sk-test-key"),
        (Wire::Fal, "authorization: key sk-test-key"),
        (Wire::OpenAi, "authorization: bearer sk-test-key"),
    ];
    for (wire, expected) in cases {
        let (base, server) = serve(vec![json_reply(200, &json!({"ok": true}))]);
        let mut target = target(&base, wire, KEY);
        target.headers = vec![custom_header()];
        let config = config(5);
        let value = client_for(&target, &config)
            .get(&format!("{base}/status"))
            .unwrap_or_else(|error| panic!("{wire:?}: {error:#}"));
        assert_eq!(value["ok"], true, "{wire:?}");
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 1, "{wire:?}");
        let raw = &seen[0];
        assert!(has_header(raw, expected), "{wire:?}: {raw}");
        assert!(
            has_header(raw, "x-custom-route: route-1"),
            "{wire:?}: {raw}"
        );
        // The key is sent once, under the wire's own header, never duplicated.
        assert_eq!(
            raw.to_ascii_lowercase().matches("sk-test-key").count(),
            1,
            "{wire:?}: {raw}"
        );
    }
}

#[test]
fn keyless_targets_send_no_credential_header() {
    let (base, server) = serve(vec![json_reply(200, &json!({}))]);
    let target = target(&base, Wire::Gemini, "");
    let config = config(5);
    client_for(&target, &config)
        .get(&format!("{base}/status"))
        .unwrap();
    let raw = server.join().unwrap().remove(0).to_ascii_lowercase();
    assert!(!raw.contains("x-goog-api-key:"), "{raw}");
    assert!(!raw.contains("authorization:"), "{raw}");
}

#[test]
fn post_sends_one_json_body_and_returns_the_parsed_reply() {
    let (base, server) = serve(vec![json_reply(200, &json!({"prompt_id": "run-1"}))]);
    let target = target(&base, Wire::ComfyUi, "");
    let config = config(5);
    let value = client_for(&target, &config)
        .post(&format!("{base}/prompt"), &json!({"prompt": {"1": 2}}))
        .unwrap();
    assert_eq!(value["prompt_id"], "run-1");
    let seen = server.join().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].starts_with("POST /prompt HTTP/1.1"), "{}", seen[0]);
    assert_eq!(body_json(&seen[0]), json!({"prompt": {"1": 2}}));
}

#[test]
fn service_failures_are_reported_once_and_never_retried() {
    let (base, server) = serve(vec![json_reply(500, &json!({"error": "boom"}))]);
    let target = target(&base, Wire::ComfyUi, "");
    let config = config(5);
    let error = client_for(&target, &config)
        .post(&format!("{base}/prompt"), &json!({}))
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("HTTP 500") && text.contains("not retried"),
        "{text}"
    );
    // The service's own error body is never echoed into the message.
    assert!(!text.contains("boom"), "{text}");
    assert_eq!(
        server.join().unwrap().len(),
        1,
        "submission must not be retried"
    );
}

#[test]
fn invalid_json_from_a_successful_service_is_refused() {
    let (base, server) = serve(vec![bytes_reply(200, b"not json".to_vec())]);
    let target = target(&base, Wire::ComfyUi, "");
    let config = config(5);
    let error = client_for(&target, &config)
        .get(&format!("{base}/status"))
        .unwrap_err();
    assert!(format!("{error:#}").contains("invalid JSON"), "{error:#}");
    server.join().unwrap();
}

#[test]
fn downloads_send_no_service_credentials_or_custom_headers() {
    let png = b"\x89PNG\r\n\x1a\nfixture".to_vec();
    let (base, server) = serve(vec![bytes_reply(200, png.clone())]);
    let mut target = target(&base, Wire::Fal, KEY);
    target.headers = vec![custom_header()];
    let config = config(5);
    let bytes = client_for(&target, &config)
        .download(&format!("{base}/files/a.png"))
        .unwrap();
    assert_eq!(bytes, png);
    let seen = server.join().unwrap();
    assert_eq!(seen.len(), 1);
    let raw = seen[0].to_ascii_lowercase();
    for absent in [
        "authorization:",
        "x-key:",
        "x-goog-api-key:",
        "x-custom-route:",
        "sk-test-key",
    ] {
        assert!(
            !raw.contains(absent),
            "download must not send {absent}: {raw}"
        );
    }
}

#[test]
fn download_failures_are_not_returned_as_images() {
    let (base, server) = serve(vec![bytes_reply(404, b"missing".to_vec())]);
    let target = target(&base, Wire::Fal, KEY);
    let config = config(5);
    let error = client_for(&target, &config)
        .download(&format!("{base}/files/a.png"))
        .unwrap_err();
    assert!(format!("{error:#}").contains("HTTP 404"), "{error:#}");
    server.join().unwrap();
}

#[test]
fn redirects_are_never_followed() {
    let (secret, secret_listener) = silent_listener();
    let (base, server) = serve(vec![
        redirect(&format!("{secret}/leak.png")),
        redirect(&format!("{secret}/status")),
    ]);
    let target = target(&base, Wire::Fal, KEY);
    let config = config(5);
    let client = client_for(&target, &config);
    assert!(client.download(&format!("{base}/files/a.png")).is_err());
    assert!(client.get(&format!("{base}/status")).is_err());
    server.join().unwrap();
    assert_no_connection(&secret_listener);
}

#[test]
fn oversized_service_bodies_are_refused_for_json_and_downloads() {
    let limit = MAX_RESPONSE_BYTES as usize;
    let target_for = |base: &str| target(base, Wire::Fal, KEY);
    let config = config(0);

    let (base, _server) = serve(vec![bytes_reply(200, vec![0u8; limit + 1])]);
    let target = target_for(&base);
    assert!(
        client_for(&target, &config)
            .download(&format!("{base}/a.png"))
            .is_err()
    );

    let (base, _server) = serve(vec![bytes_reply(200, vec![b' '; limit + 1])]);
    let target = target_for(&base);
    assert!(
        client_for(&target, &config)
            .get(&format!("{base}/status"))
            .is_err()
    );
}

#[test]
fn an_expired_deadline_stops_the_next_operation_before_connecting() {
    let (base, listener) = silent_listener();
    let target = target(&base, Wire::OpenAi, KEY);
    let config = config(1);
    let mut client = client_for(&target, &config);
    client.started = Instant::now() - Duration::from_secs(2);
    let url = client.endpoint("status");
    let error = client.get(&url).unwrap_err();
    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    assert!(client.pause().is_err());
    assert_no_connection(&listener);
}

#[test]
fn a_silent_service_is_abandoned_at_the_deadline() {
    let (base, _server) = serve(vec![silent()]);
    let target = target(&base, Wire::OpenAi, "");
    let config = config(1);
    let started = Instant::now();
    let result = client_for(&target, &config).get(&format!("{base}/status"));
    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_cancelled_request_is_abandoned_while_the_service_is_silent() {
    let (base, _server) = serve(vec![silent()]);
    let target = target(&base, Wire::OpenAi, "");
    let config = config(0);
    let cancel = Arc::new(AtomicBool::new(false));
    let setter = cancel.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        setter.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let result = with_cancel(&cancel, |shared| {
        Client::new(&target, &config, shared).get(&format!("{base}/status"))
    });
    trigger.join().unwrap();
    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_cancelled_operation_sends_nothing() {
    let (base, listener) = silent_listener();
    let target = target(&base, Wire::Fal, KEY);
    let config = config(0);
    let client = Client::new(&target, &config, Arc::new(AtomicBool::new(true)));
    let url = client.endpoint("status");
    let error = client.get(&url).unwrap_err();
    assert!(error.to_string().contains("interrupted"), "{error:#}");
    assert!(client.download(&format!("{base}/a.png")).is_err());
    assert!(client.pause().is_err());
    assert_no_connection(&listener);
}

#[test]
fn borrowed_cancellation_reaches_the_owned_transport_flag() {
    let cancel = AtomicBool::new(false);
    let observed = with_cancel(&cancel, |shared| {
        cancel.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !shared.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        shared.load(Ordering::SeqCst)
    });
    assert!(
        observed,
        "the owned flag must observe the borrowed cancellation"
    );
}

#[test]
fn an_already_cancelled_borrow_starts_the_owned_flag_set() {
    let cancel = AtomicBool::new(true);
    assert!(with_cancel(&cancel, |shared| shared.load(Ordering::SeqCst)));
}
