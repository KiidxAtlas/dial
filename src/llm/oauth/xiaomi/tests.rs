use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use super::*;

struct FakeUi(String);

impl Interaction for FakeUi {
    fn auth(&self, _url: &str, _instructions: Option<&str>) {}
    fn progress(&self, _message: &str) {}
    fn prompt(
        &self,
        _message: &str,
        _placeholder: Option<&str>,
        _allow_empty: bool,
    ) -> Result<String> {
        Ok(self.0.clone())
    }
}

fn read_request(stream: &mut TcpStream) -> (String, String) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).unwrap();
    (head, String::from_utf8_lossy(&body).into_owned())
}

/// Serve one canned response per connection; returns each request's (head, body).
fn serve(
    responses: Vec<(u16, &'static str)>,
) -> (String, thread::JoinHandle<Vec<(String, String)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            seen.push(read_request(&mut stream));
            let reply = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
        }
        seen
    });
    (addr, handle)
}

fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn flow_invariants() {
    assert_eq!(FLOW.id, "xiaomi");
    assert_eq!(FLOW.name, "Xiaomi MiMo");
    assert_eq!(FLOW.base_url, "https://api.xiaomimimo.com/v1");
    assert!(matches!(FLOW.protocol, ApiProtocol::ChatCompletions));
    const { assert!(FLOW.needs_terminal) };
    const { assert!(!FLOW.subscription) };
    assert!(FLOW.refresh.is_none());
}

#[test]
fn candidate_bases_follow_omp_order() {
    assert_eq!(candidate_bases("sk-1"), vec![STANDARD_BASE]);
    assert_eq!(
        candidate_bases("tp-1"),
        vec![
            "https://token-plan-sgp.xiaomimimo.com/v1",
            "https://token-plan-ams.xiaomimimo.com/v1",
            "https://token-plan-cn.xiaomimimo.com/v1",
        ]
    );
}

#[test]
fn validation_posts_chat_completion_with_bearer_key() {
    let (addr, server) = serve(vec![(200, "{}")]);
    let base = format!("http://{addr}/v1");
    let validated = validate("sk-test", std::slice::from_ref(&base), &cancel()).unwrap();
    assert_eq!(validated, base);

    let seen = server.join().unwrap();
    let (head, body) = &seen[0];
    assert!(head.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: bearer sk-test")
    );
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["model"], "mimo-v2.5");
    assert_eq!(body["max_tokens"], 1);
    assert_eq!(body["messages"][0]["content"], "ping");
}

#[test]
fn token_plan_key_falls_through_to_accepting_region() {
    let (addr, server) = serve(vec![(401, "{}"), (401, "{}"), (200, "{}")]);
    let bases: Vec<String> = ["sgp", "ams", "cn"]
        .iter()
        .map(|region| format!("http://{addr}/{region}/v1"))
        .collect();
    let validated = validate("tp-test", &bases, &cancel()).unwrap();
    assert_eq!(validated, format!("http://{addr}/cn/v1"));

    let paths: Vec<String> = server
        .join()
        .unwrap()
        .into_iter()
        .map(|(head, _)| head.split_whitespace().nth(1).unwrap().to_owned())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/sgp/v1/chat/completions",
            "/ams/v1/chat/completions",
            "/cn/v1/chat/completions"
        ]
    );
}

#[test]
fn non_auth_failure_stops_probing() {
    let (addr, server) = serve(vec![(500, "boom")]);
    let base = format!("http://{addr}/v1");
    let bases = vec![base.clone(), base];
    let error = validate("tp-test", &bases, &cancel()).unwrap_err();
    assert!(format!("{error:#}").contains("(500)"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn all_unauthorized_reports_401() {
    let (addr, server) = serve(vec![(401, "{}"), (401, "{}")]);
    let bases = vec![format!("http://{addr}/a/v1"), format!("http://{addr}/b/v1")];
    let error = validate("tp-test", &bases, &cancel()).unwrap_err();
    assert!(format!("{error:#}").contains("(401)"));
    server.join().unwrap();
}

#[test]
fn sign_in_stores_validated_regional_base() {
    let (addr, server) = serve(vec![(401, "{}"), (200, "{}")]);
    let bases_for = |_key: &str| {
        vec![
            format!("http://{addr}/sgp/v1"),
            format!("http://{addr}/ams/v1"),
        ]
    };
    let ui = FakeUi("  tp-abc  ".to_owned());
    let credentials = sign_in(&ui, &cancel(), &bases_for).unwrap();
    server.join().unwrap();

    assert_eq!(credentials.access, "tp-abc");
    assert!(credentials.refresh.is_empty());
    assert_eq!(credentials.expires_at_ms, util::NEVER_EXPIRES);
    assert_eq!(
        credentials.extra_str("base_url"),
        Some(format!("http://{addr}/ams/v1").as_str())
    );
}

#[test]
fn empty_key_is_rejected_before_validation() {
    let ui = FakeUi("   ".to_owned());
    let never = |_key: &str| -> Vec<String> { panic!("no validation for an empty key") };
    let error = sign_in(&ui, &cancel(), &never).unwrap_err();
    assert!(format!("{error:#}").contains("API key is required"));
}

#[test]
fn access_uses_stored_base_and_bearer_token() {
    let credentials = credential("sk-1", "https://token-plan-cn.xiaomimimo.com/v1");
    let access = access(&credentials).unwrap();
    assert_eq!(access.token.as_str(), "sk-1");
    assert_eq!(
        access.base_url.as_deref(),
        Some("https://token-plan-cn.xiaomimimo.com/v1")
    );
    assert!(access.headers.is_empty());
}
