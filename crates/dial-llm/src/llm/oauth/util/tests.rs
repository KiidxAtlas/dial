use super::*;

#[test]
fn form_round_trips_reserved_characters() {
    let encoded = form(&[
        ("redirect_uri", "http://localhost:1/cb?x=1"),
        ("scope", "a b"),
    ]);
    assert_eq!(
        encoded,
        "redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcb%3Fx%3D1&scope=a%20b"
    );
    let parsed = parse_query(&encoded).unwrap();
    assert_eq!(parsed["redirect_uri"], "http://localhost:1/cb?x=1");
    assert_eq!(parsed["scope"], "a b");
}

#[test]
fn decode_rejects_truncated_escapes() {
    assert!(decode("abc%2").is_err());
    assert!(decode("abc%").is_err());
    assert_eq!(decode("a%20b+c").unwrap(), "a b c");
}

#[test]
fn pkce_challenge_matches_rfc7636_example() {
    // S256 of the RFC 7636 Appendix B verifier. The challenge printed in the RFC does not match
    // SHA-256 of that verifier (confirmed independently with openssl and Python hashlib); this
    // value is the real digest.
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r7wW1gFWFOEjXk"),
        "bwWFMyPfdG9qreDhH2lmftFx_dFeLDalzcT1gb_j68g"
    );
}

#[test]
fn pasted_codes_accept_urls_queries_and_bare_codes() {
    let (code, state) = parse_pasted_code("zcode://zai-auth/callback?code=abc&state=xyz").unwrap();
    assert_eq!((code.as_str(), state.as_deref()), ("abc", Some("xyz")));
    let (code, state) = parse_pasted_code("code=abc").unwrap();
    assert_eq!((code.as_str(), state), ("abc", None));
    let (code, state) = parse_pasted_code("abc#st").unwrap();
    assert_eq!((code.as_str(), state.as_deref()), ("abc", Some("st")));
    assert!(parse_pasted_code("https://x/cb?error=access_denied").is_err());
}

#[test]
fn jwt_claims_decode_unpadded_payloads() {
    let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":1700000000,"sub":"u1"}"#);
    let token = format!("h.{payload}.s");
    assert_eq!(jwt_claims(&token).unwrap()["sub"], "u1");
    assert_eq!(jwt_expiry_ms(&token, 1000), Some(1_699_999_999_000));
    assert!(jwt_claims("opaque").is_none());
}

#[test]
fn device_responses_classify_rfc8628_errors() {
    let r = |status, text: &str| Response {
        status,
        text: text.into(),
        headers: Vec::new(),
    };
    assert!(matches!(
        classify_device_response(&r(400, r#"{"error":"authorization_pending"}"#)),
        Poll::Pending
    ));
    assert!(matches!(
        classify_device_response(&r(400, r#"{"error":"slow_down"}"#)),
        Poll::SlowDown
    ));
    assert!(matches!(
        classify_device_response(&r(200, r#"{"access_token":"t"}"#)),
        Poll::Complete(_)
    ));
    assert!(matches!(
        classify_device_response(&r(400, r#"{"error":"access_denied"}"#)),
        Poll::Failed(_)
    ));
}

#[test]
fn device_poll_returns_after_pending() {
    let cancel = AtomicBool::new(false);
    let mut n = 0;
    let value = poll_device(Some(0), Some(30), &cancel, || {
        n += 1;
        Ok(if n < 2 {
            Poll::Pending
        } else {
            Poll::Complete(n)
        })
    })
    .unwrap();
    assert_eq!(value, 2);
}

#[test]
fn callback_accepts_only_matching_state() {
    let callback = Callback::bind("127.0.0.1", 0, "/cb").unwrap();
    let port = callback.port();
    let client = std::thread::spawn(move || {
        for target in [
            "/other?code=x&state=s1",
            "/cb?code=x&state=wrong",
            "/cb?code=good&state=s1",
        ] {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(stream, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
            let mut reply = String::new();
            let _ = stream.read_to_string(&mut reply);
        }
    });
    let fields = callback
        .wait(Some("s1"), &AtomicBool::new(false), Duration::from_secs(10))
        .unwrap();
    client.join().unwrap();
    assert_eq!(fields["code"], "good");
}

#[test]
fn read_redirect_gives_up_on_a_client_that_dribbles_bytes() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let client = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        // One byte every 50ms never completes the head, and every individual read succeeds.
        for byte in b"GET /cb?code=x HTTP/1.1".iter().cycle().take(400) {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let (mut server, _) = listener.accept().unwrap();
    let started = Instant::now();
    let error = read_redirect_within(&mut server, "/cb", Duration::from_millis(300)).unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(server);
    client.join().unwrap();
}
