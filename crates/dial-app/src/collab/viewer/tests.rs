use super::*;

fn text(request: Request) -> String {
    match request {
        Request::Http(bytes) => String::from_utf8(bytes).unwrap(),
        other => panic!("expected an HTTP response, got {other:?}"),
    }
}

#[test]
fn plain_get_serves_the_viewer_with_a_pinned_policy() {
    let page = text(classify(b"GET /?x=1 HTTP/1.1\r\nHost: relay\r\n\r\n"));
    assert!(page.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(page.ends_with(PAGE));
    assert!(page.contains(&format!("Content-Length: {}\r\n", PAGE.len())));
    let policy = content_security_policy();
    assert!(policy.starts_with("default-src 'none'; script-src 'sha256-"));
    assert!(policy.contains("style-src 'sha256-"));
    assert!(policy.contains("connect-src 'self'"));
    assert!(page.contains("Referrer-Policy: no-referrer"));
    let head = text(classify(b"HEAD /index.html HTTP/1.1\r\n\r\n"));
    assert!(head.starts_with("HTTP/1.1 200 OK") && head.ends_with("\r\n\r\n"));
}

#[test]
fn served_page_carries_the_policy_that_pins_its_inline_blocks() {
    let served = text(classify(b"GET / HTTP/1.1\r\n\r\n"));
    let (head, body) = served.split_once("\r\n\r\n").unwrap();
    assert!(head.contains(&format!(
        "Content-Security-Policy: {}\r\n",
        content_security_policy()
    )));
    assert_eq!(body, PAGE);
}

#[test]
fn upgrades_partial_heads_and_other_paths_are_routed_correctly() {
    let upgrade = b"GET /r/abcdefghijk?role=guest HTTP/1.1\r\nUpgrade: WebSocket\r\nConnection: Upgrade\r\n\r\n";
    assert_eq!(classify(upgrade), Request::Relay);
    assert_eq!(classify(b"GET / HTTP/1.1\r\nHost: x"), Request::Incomplete);
    assert_eq!(classify(&[b'a'; MAX_HEAD_BYTES]), Request::Relay);
    assert!(text(classify(b"GET /r/abcdefghijk HTTP/1.1\r\n\r\n")).starts_with("HTTP/1.1 404"));
    assert!(text(classify(b"POST / HTTP/1.1\r\n\r\n")).starts_with("HTTP/1.1 405"));
}
