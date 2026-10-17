use super::*;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
};

#[derive(Default)]
struct Ui {
    answers: Mutex<VecDeque<String>>,
    auths: Mutex<Vec<(String, Option<String>)>>,
}

impl Ui {
    fn new(answers: &[&str]) -> Self {
        Self {
            answers: Mutex::new(answers.iter().map(|a| (*a).to_owned()).collect()),
            auths: Mutex::new(Vec::new()),
        }
    }
}

impl Interaction for Ui {
    fn auth(&self, url: &str, instructions: Option<&str>) {
        self.auths
            .lock()
            .push((url.to_owned(), instructions.map(str::to_owned)));
    }
    fn progress(&self, _message: &str) {}
    fn prompt(
        &self,
        _message: &str,
        _placeholder: Option<&str>,
        _allow_empty: bool,
    ) -> Result<String> {
        Ok(self.answers.lock().pop_front().unwrap_or_default())
    }
}

fn idle() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn unused_endpoints() -> Endpoints<'static> {
    Endpoints {
        international: "http://127.0.0.1:1/v1",
        china: "http://127.0.0.1:1/v1",
    }
}

/// Loopback HTTP server answering one canned response per expected request; each raw request
/// is sent back on the channel. Returns the base URL (`http://127.0.0.1:port/v1`).
fn serve(responses: Vec<(u16, &'static str)>) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            tx.send(read_request(&mut stream)).unwrap();
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (base, rx)
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "client closed before sending headers");
        bytes.extend_from_slice(&buf[..n]);
        if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "client closed before sending body");
        bytes.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn flow_invariants() {
    assert_eq!(FLOW.id, "alibaba-token-plan");
    assert_eq!(FLOW.base_url, INTERNATIONAL_API_BASE_URL);
    assert!(matches!(FLOW.protocol, ApiProtocol::ChatCompletions));
    const { assert!(FLOW.needs_terminal && FLOW.subscription) };
    assert!(FLOW.refresh.is_none());
    assert_eq!(
        crate::llm::oauth::flow("alibaba-token-plan").map(|f| f.name),
        Some("QwenCloud Token Plan")
    );
}

#[test]
fn international_login_stores_key_without_endpoint_or_cookie() {
    let (base, requests) = serve(vec![(200, r#"{"data":[]}"#)]);
    // Empty cookie answer skips quota reporting.
    let ui = Ui::new(&["1", " sk-sp-test ", ""]);
    let endpoints = Endpoints {
        international: &base,
        china: "http://127.0.0.1:1/v1",
    };
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();

    let request = requests.recv().unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /v1/models http/1.1"));
    assert!(request.contains("authorization: bearer sk-sp-test"));

    assert_eq!(credentials.access, "sk-sp-test");
    assert!(credentials.refresh.is_empty());
    assert_eq!(credentials.expires_at_ms, util::NEVER_EXPIRES);
    assert_eq!(credentials.api_endpoint, None);
    assert!(!credentials.extra.contains_key("cookie"));
    assert_eq!(
        *ui.auths.lock(),
        vec![(
            INTERNATIONAL_AUTH_URL.to_owned(),
            Some(
                "Subscribe to Token Plan Individual and copy its dedicated API key. Keep this page open; the next prompt explains how to enable optional quota reporting."
                    .to_owned()
            ),
        )]
    );
}

#[test]
fn china_login_stores_region_endpoint_and_normalized_cookie() {
    let (china, requests) = serve(vec![(200, "{}")]);
    let ui = Ui::new(&["2", "sk-sp-cn", "  cookie: a=1; b=2  "]);
    let endpoints = Endpoints {
        international: "http://127.0.0.1:1/v1",
        china: &china,
    };
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();

    assert!(requests.recv().unwrap().starts_with("GET /v1/models"));
    assert_eq!(credentials.api_endpoint.as_deref(), Some(china.as_str()));
    assert_eq!(
        credentials.extra.get("cookie").and_then(|v| v.as_str()),
        Some("a=1; b=2")
    );
    assert_eq!(ui.auths.lock()[0].0, CHINA_AUTH_URL);
}

#[test]
fn custom_region_normalizes_url_and_stores_it_as_endpoint() {
    let (base, requests) = serve(vec![(200, "{}")]);
    let custom = format!("{base}//");
    let ui = Ui::new(&["3", custom.as_str(), "sk-sp-custom", ""]);
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints()).unwrap();

    assert!(requests.recv().unwrap().starts_with("GET /v1/models"));
    assert_eq!(credentials.api_endpoint.as_deref(), Some(base.as_str()));
    assert_eq!(credentials.access, "sk-sp-custom");
}

#[test]
fn lone_cookie_value_is_rejected() {
    let (base, _requests) = serve(vec![(200, "{}")]);
    let ui = Ui::new(&["1", "sk-sp-x", "abc123"]);
    let endpoints = Endpoints {
        international: &base,
        china: "",
    };
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints)
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Invalid QwenCloud Cookie header."));
    assert!(error.contains(INTERNATIONAL_COOKIE_HOST));
}

#[test]
fn rejected_key_fails_without_echoing_the_key_or_body() {
    let (base, _requests) = serve(vec![(403, r#"{"error":"sk-sp-leaked"}"#)]);
    let ui = Ui::new(&["1", "sk-sp-leaked"]);
    let endpoints = Endpoints {
        international: &base,
        china: "",
    };
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "QwenCloud Token Plan API key validation failed (HTTP 403)"
    );
}

#[test]
fn empty_custom_url_is_rejected() {
    let ui = Ui::new(&["3", "  "]);
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints())
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Custom URL is required for option 3");
}

#[test]
fn cookie_normalization_and_shape_check() {
    assert_eq!(normalize_cookie("  Cookie:  a=1 "), "a=1");
    assert_eq!(normalize_cookie("COOKIE:a=1"), "a=1");
    assert_eq!(normalize_cookie("   "), "");
    assert!(is_cookie_header("a=1"));
    assert!(is_cookie_header("junk; b=2"));
    assert!(!is_cookie_header("abc"));
    assert!(!is_cookie_header("=x"));
    assert!(!is_cookie_header("a="));
}

#[test]
fn access_uses_the_stored_region_endpoint_when_present() {
    let regional = Credentials {
        access: "sk-a".into(),
        api_endpoint: Some("https://example.test/v1".into()),
        expires_at_ms: util::NEVER_EXPIRES,
        ..Credentials::default()
    };
    assert_eq!(
        access(&regional).unwrap().base_url.as_deref(),
        Some("https://example.test/v1")
    );

    let international = Credentials {
        access: "sk-b".into(),
        ..Credentials::default()
    };
    let shaped = access(&international).unwrap();
    assert_eq!(shaped.token.as_str(), "sk-b");
    assert_eq!(shaped.base_url, None);
}
