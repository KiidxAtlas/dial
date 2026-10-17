use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

#[test]
fn flow_invariants() {
    assert_eq!(FLOW.id, "snowflake");
    assert_eq!(FLOW.name, "Snowflake Cortex");
    assert!(matches!(FLOW.protocol, ApiProtocol::Anthropic));
    const { assert!(FLOW.needs_terminal) };
    const { assert!(!FLOW.subscription) };
    assert!(FLOW.refresh.is_some());
    assert_eq!(
        FLOW.base_url,
        "https://snowflake-account.invalid/api/v2/cortex"
    );
}

#[test]
fn normalizes_account_identifiers_and_urls() {
    let cases = [
        (
            "myorg-myaccount",
            "https://myorg-myaccount.snowflakecomputing.com",
        ),
        (
            "  MyOrg_My_Account  ",
            "https://myorg-my-account.snowflakecomputing.com",
        ),
        (
            "https://Org_Acct.snowflakecomputing.com:443/some/path?x=1",
            "https://org-acct.snowflakecomputing.com",
        ),
        (
            "HTTPS://acct.snowflakecomputing.com",
            "https://acct.snowflakecomputing.com",
        ),
        (
            "https://app.snowflake.com/myorg/my_acct/worksheets",
            "https://myorg-my-acct.snowflakecomputing.com",
        ),
        (
            "https://app.snowflake.com/myorg/acct/#/data",
            "https://myorg-acct.snowflakecomputing.com",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            normalize_account_url(input).unwrap(),
            expected,
            "input {input}"
        );
    }
}

#[test]
fn rejects_unsafe_or_invalid_accounts() {
    let cases = [
        "",
        "   ",
        "http://acct.snowflakecomputing.com",
        "https://evil.example.com",
        "https://acct.snowflakecomputing.com.evil.example",
        "https://.snowflakecomputing.com",
        "myorg/acct",
        "myorg-acct?x=1",
        "https:\\\\acct.snowflakecomputing.com",
        "https://",
        "myorg-acct.snowflakecomputing.cn",
        "https://app.snowflake.com/us-east-1/acct",
        "https://app.snowflake.com/myorg",
        "bad_label..x",
    ];
    for input in cases {
        assert!(
            normalize_account_url(input).is_err(),
            "should reject {input:?}"
        );
    }
    let china = normalize_account_url("acct.snowflakecomputing.cn").unwrap_err();
    assert!(china.to_string().contains("China-region"));
    let legacy = normalize_account_url("https://app.snowflake.com/us-gov-west-1/acct").unwrap_err();
    assert!(legacy.to_string().contains("Legacy Snowsight"));
}

#[test]
fn authorize_url_carries_pkce_state_redirect_and_scope() {
    // RFC 7636 appendix B vector.
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = util::pkce_challenge(verifier);
    assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");

    let url = authorize_url(
        "https://acct.snowflakecomputing.com",
        "http://127.0.0.1:54551/",
        "state-123",
        &challenge,
    );
    let (base, query) = url.split_once('?').unwrap();
    assert_eq!(base, "https://acct.snowflakecomputing.com/oauth/authorize");
    let fields = util::parse_query(query).unwrap();
    assert_eq!(fields["client_id"], "LOCAL_APPLICATION");
    assert_eq!(fields["response_type"], "code");
    assert_eq!(fields["redirect_uri"], "http://127.0.0.1:54551/");
    assert_eq!(fields["scope"], "refresh_token");
    assert_eq!(fields["state"], "state-123");
    assert_eq!(fields["code_challenge"], challenge);
    assert_eq!(fields["code_challenge_method"], "S256");
    assert_eq!(fields.len(), 7);
}

#[test]
fn parses_token_responses() {
    let body = serde_json::json!({
        "access_token": "acc",
        "refresh_token": "ref",
        "expires_in": 3600,
    });
    let tokens = parse_tokens(&body, 1_000).unwrap();
    assert_eq!(tokens.access, "acc");
    assert_eq!(tokens.refresh.as_deref(), Some("ref"));
    assert_eq!(tokens.expires_at_ms, 1_000 + 3_600_000);

    let no_refresh = serde_json::json!({ "access_token": "acc", "expires_in": 60 });
    let tokens = parse_tokens(&no_refresh, 0).unwrap();
    assert!(tokens.refresh.is_none());
    assert_eq!(tokens.expires_at_ms, 60_000);

    let invalid = [
        serde_json::json!({ "expires_in": 60 }),
        serde_json::json!({ "access_token": "  ", "expires_in": 60 }),
        serde_json::json!({ "access_token": "a" }),
        serde_json::json!({ "access_token": "a", "expires_in": 0 }),
        serde_json::json!({ "access_token": "a", "expires_in": -5 }),
        serde_json::json!({ "access_token": "a", "expires_in": 60, "refresh_token": null }),
        serde_json::json!({ "access_token": "a", "expires_in": 60, "refresh_token": "" }),
        serde_json::json!("not an object"),
    ];
    for body in invalid {
        assert!(parse_tokens(&body, 0).is_err(), "should reject {body}");
    }
}

/// Read one HTTP request (headers plus Content-Length body) from a stream.
fn read_request(stream: &mut TcpStream) -> String {
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..end]).into_owned();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if data.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

/// Serve one canned response on loopback; the join handle yields the captured request.
fn serve_once(status: &'static str, body: &'static str) -> (u16, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        request
    });
    (port, handle)
}

#[test]
fn token_exchange_posts_pkce_form_to_loopback() {
    let (port, server) = serve_once(
        "200 OK",
        r#"{"access_token":"acc-token","refresh_token":"ref-token","expires_in":3600}"#,
    );
    let base = format!("http://127.0.0.1:{port}");
    let cancel = Arc::new(AtomicBool::new(false));
    let before = util::now_ms();
    let tokens = fetch_tokens(
        &base,
        &[
            ("grant_type", "authorization_code"),
            ("code", "auth-code"),
            ("redirect_uri", "http://127.0.0.1:54551/"),
            ("code_verifier", "the-verifier"),
            ("client_id", CLIENT_ID),
        ],
        "exchange",
        &cancel,
    )
    .unwrap();
    assert_eq!(tokens.access, "acc-token");
    assert_eq!(tokens.refresh.as_deref(), Some("ref-token"));
    assert!(tokens.expires_at_ms >= before + 3_600_000);

    let request = server.join().unwrap();
    assert!(
        request.starts_with("POST /oauth/token-request "),
        "{request}"
    );
    assert!(request.contains("grant_type=authorization_code"));
    assert!(request.contains("code=auth-code"));
    assert!(request.contains("code_verifier=the-verifier"));
    assert!(request.contains("client_id=LOCAL_APPLICATION"));
    assert!(request.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A54551%2F"));
}

#[test]
fn token_refresh_reports_http_failure_without_body() {
    let (port, server) = serve_once("401 Unauthorized", r#"{"error":"secret-detail"}"#);
    let base = format!("http://127.0.0.1:{port}");
    let cancel = Arc::new(AtomicBool::new(false));
    let err = fetch_tokens(
        &base,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", "old"),
            ("client_id", CLIENT_ID),
        ],
        "refresh",
        &cancel,
    )
    .err()
    .unwrap();
    let message = err.to_string();
    assert_eq!(message, "Snowflake token refresh failed (401)");
    assert!(!message.contains("secret-detail"));
    let request = server.join().unwrap();
    assert!(request.contains("grant_type=refresh_token"));
}

#[test]
fn callback_rejects_mismatched_state_and_accepts_matching_one() {
    let cancel = Arc::new(AtomicBool::new(false));
    let callback = util::Callback::bind("127.0.0.1", 0, CALLBACK_PATH).unwrap();
    let port = callback.port();
    let client = thread::spawn(move || {
        let send = |path: &str| {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        };
        let stale = send("/?code=stale-code&state=other-state");
        let fresh = send("/?code=fresh-code&state=expected-state");
        (stale, fresh)
    });
    let fields = callback
        .wait(Some("expected-state"), &cancel, Duration::from_secs(10))
        .unwrap();
    assert_eq!(fields["code"], "fresh-code");
    assert_eq!(fields["state"], "expected-state");
    let (stale, fresh) = client.join().unwrap();
    assert!(stale.starts_with("HTTP/1.1 400"), "{stale}");
    assert!(fresh.starts_with("HTTP/1.1 200"), "{fresh}");
}

#[test]
fn access_points_at_cortex_endpoint_with_bearer_token() {
    let credentials = Credentials {
        access: "tok-123".into(),
        refresh: "ref".into(),
        expires_at_ms: u64::MAX,
        enterprise_url: Some("https://myorg-acct.snowflakecomputing.com".into()),
        ..Credentials::default()
    };
    let shaped = access(&credentials).unwrap();
    assert_eq!(shaped.token.as_str(), "tok-123");
    assert_eq!(
        shaped.base_url.as_deref(),
        Some("https://myorg-acct.snowflakecomputing.com/api/v2/cortex")
    );
    assert!(shaped.headers.is_empty());

    let missing = Credentials {
        access: "tok".into(),
        ..Credentials::default()
    };
    assert!(access(&missing).is_err());
}

#[test]
fn refresh_without_refresh_token_keeps_valid_token_only() {
    let cancel = Arc::new(AtomicBool::new(false));
    let valid = Credentials {
        access: "still-good".into(),
        refresh: String::new(),
        expires_at_ms: u64::MAX,
        enterprise_url: Some("https://myorg-acct.snowflakecomputing.com".into()),
        ..Credentials::default()
    };
    let kept = refresh(&valid, &cancel).unwrap();
    assert!(kept == valid);
    assert_eq!(kept.access, "still-good");

    let expired = Credentials {
        expires_at_ms: 1,
        ..valid.clone()
    };
    let err = refresh(&expired, &cancel).err().unwrap();
    assert!(err.to_string().contains("did not issue a refresh token"));

    let bad_account = Credentials {
        refresh: "ref".into(),
        enterprise_url: Some("http://evil.example".into()),
        ..valid
    };
    let err = refresh(&bad_account, &cancel).err().unwrap();
    assert_eq!(
        err.to_string(),
        "Invalid Snowflake account; run /login snowflake again"
    );
}
