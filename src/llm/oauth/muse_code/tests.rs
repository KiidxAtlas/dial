use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use super::*;

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
    assert_eq!(FLOW.id, "muse-code");
    assert_eq!(FLOW.name, "Muse Code");
    assert_eq!(FLOW.base_url, "https://api.meta.ai/v1");
    assert!(matches!(FLOW.protocol, ApiProtocol::Responses));
    const { assert!(!FLOW.needs_terminal) };
    const { assert!(FLOW.subscription) };
    assert!(FLOW.refresh.is_none());
}

#[test]
fn device_flow_exchanges_token_and_mints_key() {
    let (addr, server) = serve(vec![
        (
            200,
            r#"{"device_code":"DEV-1","user_code":"ABCD","verification_uri":"https://auth.meta.com/verify","verification_uri_complete":"https://auth.meta.com/verify?code=ABCD","interval":1,"expires_in":60}"#,
        ),
        (400, r#"{"error":"authorization_pending"}"#),
        (
            200,
            r#"{"access_token":"acct-token","token_type":"bearer"}"#,
        ),
        (
            200,
            r#"{"api_key":"mk-1","user_id":"u-1","user_email":"A@Example.com","is_subs_active":true}"#,
        ),
    ]);
    let endpoints = Endpoints {
        device: &format!("http://{addr}/oidc/device/authorization/"),
        token: &format!("http://{addr}/oidc/device/token/"),
        key: &format!("http://{addr}/muse-code/key"),
    };
    let opened = RefCell::new(Vec::new());
    let open = |url: &str, instructions: Option<&str>| {
        opened
            .borrow_mut()
            .push((url.to_owned(), instructions.map(str::to_owned)));
    };
    let credentials = sign_in(&cancel(), &open, &endpoints).unwrap();
    let seen = server.join().unwrap();

    assert_eq!(
        *opened.borrow(),
        vec![(
            "https://auth.meta.com/verify?code=ABCD".to_owned(),
            Some("Enter code: ABCD".to_owned())
        )]
    );
    assert!(
        seen[0]
            .0
            .starts_with("POST /oidc/device/authorization/ HTTP/1.1")
    );
    assert!(seen[0].1.contains("client_id=1031625952748946"));
    assert!(seen[1].0.starts_with("POST /oidc/device/token/ HTTP/1.1"));
    assert!(seen[1].1.contains("device_code=DEV-1"));
    assert!(seen[1].1.contains("client_id=1031625952748946"));
    assert!(seen[1].1.contains("grant_type="));
    assert!(seen[3].0.starts_with("POST /muse-code/key HTTP/1.1"));
    assert!(
        seen[3]
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer acct-token")
    );
    assert!(
        seen[3]
            .0
            .to_ascii_lowercase()
            .contains("x-api-version: 1.0.0")
    );
    assert_eq!(seen[3].1, r#"{"onboard":true}"#);

    assert_eq!(credentials.access, "acct-token");
    assert!(credentials.refresh.is_empty());
    assert_eq!(credentials.extra_str("api_key"), Some("mk-1"));
    assert_eq!(credentials.account_id.as_deref(), Some("u-1"));
    assert_eq!(credentials.email.as_deref(), Some("a@example.com"));
    assert_eq!(credentials.expires_at_ms, util::NEVER_EXPIRES);
}

#[test]
fn key_exchange_failure_reports_status_only() {
    let (addr, server) = serve(vec![(403, "secret-body")]);
    let url = format!("http://{addr}/key");
    let error = request_key(&url, "acct", true, &cancel())
        .err()
        .expect("403 must fail");
    server.join().unwrap();
    let text = format!("{error:#}");
    assert!(text.contains("403"));
    assert!(!text.contains("secret-body"));
}

#[test]
fn inactive_subscription_is_invalid_grant() {
    let key = parse_key_response(json!({
        "api_key": "mk", "user_id": "u", "is_subs_active": false
    }))
    .unwrap();
    let error = credentials("acct".into(), String::new(), key).unwrap_err();
    assert!(format!("{error:#}").contains("invalid_grant: Muse Code subscription is inactive"));
}

#[test]
fn payment_required_reports_action_url() {
    let key = parse_key_response(json!({
        "require_payment": true,
        "require_payment_action_url": "https://meta.ai/subscribe"
    }))
    .unwrap();
    let error = credentials("acct".into(), String::new(), key).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("Muse Code subscription is required: https://meta.ai/subscribe")
    );
}

#[test]
fn missing_key_and_identity_are_distinct_errors() {
    let no_key = parse_key_response(json!({"user_id": "u"})).unwrap();
    let error = credentials("acct".into(), String::new(), no_key).unwrap_err();
    assert!(format!("{error:#}").contains("missing api_key"));

    let no_identity = parse_key_response(json!({"api_key": "mk"})).unwrap();
    let error = credentials("acct".into(), String::new(), no_identity).unwrap_err();
    assert!(format!("{error:#}").contains("missing a stable account identity"));
}

#[test]
fn email_is_lowercased_and_used_when_user_id_absent() {
    let key = parse_key_response(json!({
        "api_key": "mk", "user_email": "  Dev@Example.COM "
    }))
    .unwrap();
    let credentials = credentials("acct".into(), String::new(), key).unwrap();
    assert_eq!(credentials.account_id.as_deref(), Some("dev@example.com"));
    assert_eq!(credentials.email.as_deref(), Some("dev@example.com"));
}

#[test]
fn access_uses_minted_key_as_bearer_with_version_header() {
    let key = parse_key_response(json!({"api_key": "mk-1", "user_id": "u-1"})).unwrap();
    let credentials = credentials("acct".into(), String::new(), key).unwrap();
    let shaped = access(&credentials).unwrap();
    assert_eq!(shaped.token.as_str(), "mk-1");
    assert_eq!(shaped.base_url, None);
    assert_eq!(
        shaped.headers,
        vec![("x-api-version".to_owned(), "1.0.0".to_owned())]
    );
    assert_eq!(shaped.account_id.as_deref(), Some("u-1"));
}

#[test]
fn access_without_minted_key_asks_for_sign_in() {
    let credentials = Credentials {
        access: "acct".into(),
        expires_at_ms: util::NEVER_EXPIRES,
        ..Default::default()
    };
    let error = access(&credentials).expect_err("no key must fail");
    assert!(format!("{error:#}").contains("sign in again"));
}
