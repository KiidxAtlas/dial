use super::*;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
};

/// Scripted interaction: answers prompts in order and records what the flow showed.
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
    assert_eq!(FLOW.id, "alibaba-coding-plan");
    assert_eq!(FLOW.base_url, DEFAULT_API_BASE_URL);
    assert!(matches!(FLOW.protocol, ApiProtocol::ChatCompletions));
    const { assert!(FLOW.needs_terminal && FLOW.subscription) };
    assert!(FLOW.refresh.is_none());
    assert_eq!(
        crate::llm::oauth::flow("alibaba-coding-plan").map(|f| f.name),
        Some("Alibaba Coding Plan")
    );
}

#[test]
fn international_login_validates_chat_probe_and_stores_endpoint() {
    let (base, requests) = serve(vec![(200, r#"{"choices":[]}"#)]);
    let ui = Ui::new(&["1", "  sk-test-123 "]);
    let endpoints = Endpoints {
        international: &base,
        china: "http://127.0.0.1:1/v1",
    };
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();

    let request = requests.recv().unwrap().to_ascii_lowercase();
    assert!(request.starts_with("post /v1/chat/completions http/1.1"));
    assert!(request.contains("authorization: bearer sk-test-123"));
    assert!(request.contains("\"max_tokens\":1"));
    assert!(request.contains("\"model\":\"qwen3.5-plus\""));

    assert_eq!(credentials.access, "sk-test-123");
    assert!(credentials.refresh.is_empty());
    assert_eq!(credentials.expires_at_ms, util::NEVER_EXPIRES);
    assert_eq!(credentials.enterprise_url.as_deref(), Some(base.as_str()));
    assert_eq!(
        *ui.auths.lock(),
        vec![(
            DEFAULT_AUTH_URL.to_owned(),
            Some(
                "Copy your API key from the Alibaba Cloud DashScope console (International)"
                    .to_owned()
            ),
        )]
    );
}

#[test]
fn china_login_opens_china_console_and_validates_its_endpoint() {
    let (china, requests) = serve(vec![(200, "{}")]);
    let ui = Ui::new(&["2", "sk-cn"]);
    let endpoints = Endpoints {
        international: "http://127.0.0.1:1/v1",
        china: &china,
    };
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();
    assert!(
        requests
            .recv()
            .unwrap()
            .starts_with("POST /v1/chat/completions")
    );
    assert_eq!(credentials.enterprise_url.as_deref(), Some(china.as_str()));
    assert_eq!(ui.auths.lock()[0].0, CHINA_AUTH_URL);
}

#[test]
fn custom_endpoint_normalizes_url_and_validates_models() {
    let (base, requests) = serve(vec![(200, r#"{"data":[]}"#)]);
    let custom = format!("{base}//");
    let ui = Ui::new(&["3", custom.as_str(), "sk-custom"]);
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints()).unwrap();

    let request = requests.recv().unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /v1/models http/1.1"));
    assert!(request.contains("authorization: bearer sk-custom"));
    assert_eq!(credentials.enterprise_url.as_deref(), Some(base.as_str()));
    assert_eq!(credentials.access, "sk-custom");
    assert_eq!(ui.auths.lock()[0].0, DEFAULT_AUTH_URL);
}

#[test]
fn rejected_key_fails_without_echoing_the_key_or_body() {
    let (base, _requests) = serve(vec![(401, r#"{"error":"bad sk-leaked"}"#)]);
    let ui = Ui::new(&["1", "sk-leaked"]);
    let endpoints = Endpoints {
        international: &base,
        china: "",
    };
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "Alibaba Coding Plan API key validation failed (HTTP 401)"
    );
}

#[test]
fn empty_key_is_rejected_before_any_request() {
    let ui = Ui::new(&["1", "   "]);
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints())
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Alibaba Coding Plan API key is required");
}

#[test]
fn empty_custom_url_is_rejected() {
    let ui = Ui::new(&["3", " / "]);
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints())
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Custom URL is required for option 3");
}

#[test]
fn cancellation_after_region_choice_stops_before_the_key_prompt() {
    let cancel = Arc::new(AtomicBool::new(true));
    let ui = Ui::new(&["1", "sk-never-used"]);
    let error = sign_in(&ui, &cancel, &|u, i| ui.auth(u, i), &unused_endpoints())
        .unwrap_err()
        .to_string();
    assert_eq!(error, "sign-in cancelled");
    assert!(ui.auths.lock().is_empty());
}

#[test]
fn access_uses_the_stored_key_and_endpoint_override() {
    let credentials = Credentials {
        access: "sk-a".into(),
        enterprise_url: Some("https://example.test/v1".into()),
        expires_at_ms: util::NEVER_EXPIRES,
        ..Credentials::default()
    };
    let shaped = access(&credentials).unwrap();
    assert_eq!(shaped.token.as_str(), "sk-a");
    assert_eq!(shaped.base_url.as_deref(), Some("https://example.test/v1"));

    let bare = Credentials {
        access: "sk-b".into(),
        ..Credentials::default()
    };
    assert_eq!(access(&bare).unwrap().base_url, None);
}
