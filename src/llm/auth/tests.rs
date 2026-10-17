use super::*;
use std::{
    io::{BufRead, BufReader},
    net::TcpListener,
    thread,
};

fn token_server(status: u16, body: &'static str) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let job = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        let mut body_len = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                body_len = value.trim().parse::<usize>().unwrap();
            }
            request.push_str(&line);
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let mut payload = vec![0; body_len];
        reader.read_exact(&mut payload).unwrap();
        request.push_str(std::str::from_utf8(&payload).unwrap());
        write!(stream, "HTTP/1.1 {status} result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        request
    });
    (format!("http://{address}/api/v1/auth/keys"), job)
}

#[test]
fn callback_listener_accepts_only_the_local_callback_and_times_out() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        receive_callback(
            &listener,
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
        )
    });
    let mut browser = TcpStream::connect(address).unwrap();
    browser
        .write_all(b"GET /callback?code=callback-code HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    browser.read_to_string(&mut response).unwrap();
    assert!(response.contains("OpenRouter sign-in complete"));
    assert_eq!(server.join().unwrap().unwrap(), "callback-code");

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(
        receive_callback(&listener, Instant::now(), &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
}

#[test]
fn sign_in_notice_always_carries_the_url_for_manual_completion() {
    let url = "https://openrouter.ai/auth?code_challenge=x&code_challenge_method=S256";
    assert!(sign_in_notice(true, url).ends_with(url));
    let manual = sign_in_notice(false, url);
    assert!(manual.contains("Could not open a browser"));
    assert!(manual.ends_with(url));
}

#[test]
fn manual_callback_is_received_after_the_browser_could_not_open() {
    // The listener is the one `openrouter_login_with` keeps alive after an opener failure.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        receive_callback(
            &listener,
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
        )
    });
    let mut manual = TcpStream::connect(address).unwrap();
    manual
        .write_all(b"GET /callback?code=manual-code HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    manual.read_to_string(&mut response).unwrap();
    assert_eq!(server.join().unwrap().unwrap(), "manual-code");
}

#[test]
fn callback_waits_for_a_request_that_arrives_after_the_connection() {
    // A browser can open the loopback connection before it sends the request. Accepted sockets
    // inherit non-blocking mode from the polling listener on macOS, which made the read fail instantly.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        receive_callback(
            &listener,
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
        )
    });
    let mut browser = TcpStream::connect(address).unwrap();
    thread::sleep(Duration::from_millis(300));
    browser
        .write_all(b"GET /callback?code=late-code HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    browser.read_to_string(&mut response).unwrap();
    assert!(
        response.contains("OpenRouter sign-in complete"),
        "{response}"
    );
    assert_eq!(server.join().unwrap().unwrap(), "late-code");
}

#[test]
fn code_exchange_posts_pkce_and_never_echoes_provider_errors() {
    let (url, job) = token_server(200, r#"{"key":"sk-or-created"}"#);
    let token = exchange_code_at(
        &url,
        "auth-code",
        "pkce-verifier",
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(token.as_str(), "sk-or-created");
    assert_eq!(format!("{token:?}"), "SecretToken(<redacted>)");
    let request = job.join().unwrap();
    assert!(request.starts_with("POST /api/v1/auth/keys "));
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["code"], "auth-code");
    assert_eq!(body["code_verifier"], "pkce-verifier");
    assert_eq!(body["code_challenge_method"], "S256");

    let (url, job) = token_server(403, r#"{"error":"do not expose this body"}"#);
    let error = exchange_code_at(
        &url,
        "private-code",
        "private-verifier",
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("rejected"));
    assert!(
        !error.contains("private-code")
            && !error.contains("private-verifier")
            && !error.contains("do not expose")
    );
    job.join().unwrap();
}

#[test]
fn authorization_url_contains_documented_pkce_parameters() {
    let url = authorization_url("challenge-value");
    assert_eq!(
        url,
        "https://openrouter.ai/auth?callback_url=http%3A%2F%2Flocalhost%3A54549%2Fcallback&code_challenge=challenge-value&code_challenge_method=S256&key_label=dial"
    );
    assert!(!url.contains("state="));
    assert!(!url.contains("client_id="));
}

#[test]
fn callback_accepts_exact_path_and_decodes_code() {
    assert_eq!(
        parse_callback_target("/callback?other=x&code=a%2Bb%2Fc").unwrap(),
        "a+b/c"
    );
    assert!(parse_callback_target("/elsewhere?code=abc").is_err());
    assert!(parse_callback_target("/callback?code=x&code=y").is_err());
    assert!(parse_callback_target("/callback?error=access_denied").is_err());
}

#[test]
fn token_response_requires_nonempty_openrouter_key() {
    assert_eq!(
        validate_key_response(&serde_json::json!({"key":"sk-or-test"})).unwrap(),
        "sk-or-test"
    );
    for value in [
        serde_json::json!({}),
        serde_json::json!({"key": 1}),
        serde_json::json!({"key":""}),
        serde_json::json!({"key":"sk-test"}),
    ] {
        assert!(validate_key_response(&value).is_err());
    }
}

#[test]
fn secret_debug_redacts_and_drop_zeroes_contents() {
    let mut token = SecretToken::new("sk-or-private".into());
    assert_eq!(format!("{token:?}"), "SecretToken(<redacted>)");
    assert_eq!(token.as_mut_string(), "sk-or-private");
    token.wipe();
    assert!(token.0.bytes().all(|byte| byte == 0));
    std::mem::forget(token);
}
