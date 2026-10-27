use super::*;
use crate::llm::fake;

#[test]
fn authorization_url_uses_pkce_scopes_and_state() {
    let url = authorization_url(
        "http://localhost:54545/callback",
        "state-123",
        "verifier-123",
    );
    assert!(url.starts_with(AUTHORIZE_URL));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("code=true"));
    assert!(url.contains("state=state-123"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains(&format!("code_challenge={}", challenge("verifier-123"))));
    assert!(url.contains("user%3Ainference"));
}

#[test]
fn callback_rejects_state_mismatch_and_returns_authorization_code() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let request = std::thread::spawn(move || {
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(b"GET /callback?code=auth-code&state=expected HTTP/1.1\\r\\nHost: localhost\\r\\n\\r\\n").unwrap();
    });
    let (mut stream, _) = listener.accept().unwrap();
    assert_eq!(
        callback_code(&mut stream, "expected", &AtomicBool::new(false)).unwrap(),
        "auth-code"
    );
    request.join().unwrap();

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let request = std::thread::spawn(move || {
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(b"GET /callback?code=auth-code&state=wrong HTTP/1.1\\r\\nHost: localhost\\r\\n\\r\\n").unwrap();
    });
    let (mut stream, _) = listener.accept().unwrap();
    assert!(callback_code(&mut stream, "expected", &AtomicBool::new(false)).is_err());
    request.join().unwrap();
}
#[test]
fn browser_preconnections_do_not_consume_anthropic_oauth_attempt() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let job =
        std::thread::spawn(move || read_callback(&listener, "state-good", &AtomicBool::new(false)));
    for target in ["/favicon.ico", "/callback?state=wrong&code=no"] {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(stream, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
    }
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(
            b"GET /callback?code=valid-code&state=state-good HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert_eq!(job.join().unwrap().unwrap(), "valid-code");
}

#[test]
fn expired_access_refreshes_and_preserves_rotating_refresh_token() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path();
    save(
        dir,
        &State {
            tokens: Some(Tokens {
                access_token: "sk-ant-oat01-old".into(),
                refresh_token: "refresh-old".into(),
                expires_at: now().saturating_sub(1),
            }),
        },
    )
    .unwrap();
    let fake = fake::serve(vec![fake::json(
        200,
        &json!({"access_token":"sk-ant-oat01-new","expires_in":3600}),
    )]);
    let token = access_token_at(dir, &fake.url, Arc::new(AtomicBool::new(false))).unwrap();
    assert_eq!(token.as_str(), "sk-ant-oat01-new");
    let requests = fake.requests();
    assert_eq!(
        fake::body(&requests[0]),
        json!({"grant_type":"refresh_token","client_id":CLIENT_ID,"refresh_token":"refresh-old"})
    );
    assert_eq!(
        fake::header(&requests[0], "anthropic-beta").as_deref(),
        Some("oauth-2025-04-20")
    );
    assert_eq!(
        load(dir).unwrap().tokens.unwrap().refresh_token,
        "refresh-old"
    );
}

#[test]
fn fresh_access_token_skips_refresh_request() {
    let home = tempfile::tempdir().unwrap();
    save(
        home.path(),
        &State {
            tokens: Some(Tokens {
                access_token: "sk-ant-oat01-fresh".into(),
                refresh_token: "refresh".into(),
                expires_at: now() + 3600,
            }),
        },
    )
    .unwrap();
    let token = access_token_at(
        home.path(),
        "http://127.0.0.1:1",
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(token.as_str(), "sk-ant-oat01-fresh");
}

#[test]
fn malformed_token_responses_are_rejected_and_keep_previous_refresh() {
    let previous = Tokens {
        access_token: "old".into(),
        refresh_token: "refresh-old".into(),
        expires_at: 0,
    };
    assert_eq!(
        tokens(
            &json!({"access_token":"new","expires_in":30}),
            Some(&previous)
        )
        .unwrap()
        .refresh_token,
        "refresh-old"
    );
    assert!(tokens(&json!({"access_token":"new"}), None).is_err());
    assert!(tokens(&json!({"refresh_token":"refresh"}), None).is_err());
}

#[test]
fn signed_in_distinguishes_missing_from_corrupt_credentials() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!signed_in(dir.path()));
    assert!(!signed_in_checked(dir.path()).unwrap());
    write_private(&dir.path().join(FILE), b"{not json").unwrap();
    assert!(!signed_in(dir.path()));
    let error = signed_in_checked(dir.path()).unwrap_err();
    assert!(
        format!("{error:#}").contains("credentials are invalid"),
        "{error:#}"
    );
}

#[test]
fn rejected_refresh_preserves_subscription_and_saved_api_key() {
    let home = tempfile::tempdir().unwrap();
    let previous = State {
        tokens: Some(Tokens {
            access_token: "sk-ant-oat01-expired-test".into(),
            refresh_token: "refresh-test".into(),
            expires_at: 0,
        }),
    };
    save(home.path(), &previous).unwrap();
    super::super::catalog::save_credential(
        home.path(),
        super::super::anthropic::BASE_URL,
        "sk-ant-api03-key-test",
    )
    .unwrap();
    let before = std::fs::read(credential_path(home.path())).unwrap();
    let fake = fake::serve(vec![fake::json(
        401,
        &json!({"error": "refresh-secret-test-rejected"}),
    )]);
    let error =
        access_token_at(home.path(), &fake.url, Arc::new(AtomicBool::new(false))).unwrap_err();
    assert!(!format!("{error:#}").contains("refresh-secret-test-rejected"));
    assert_eq!(std::fs::read(credential_path(home.path())).unwrap(), before);
    assert!(signed_in_checked(home.path()).unwrap());
    assert_eq!(
        super::super::catalog::credential_token(home.path(), super::super::anthropic::BASE_URL)
            .unwrap()
            .as_deref(),
        Some("sk-ant-api03-key-test")
    );
    assert_eq!(fake.requests().len(), 1);
}
