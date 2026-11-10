use super::*;

#[test]
fn only_opencode_hosts_are_endpoints() {
    for url in [
        "https://opencode.ai/zen/v1",
        "https://opencode.ai/zen/go/v1/chat/completions",
        "https://OpenCode.ai/inference/openai/v1",
    ] {
        assert!(is_endpoint(url), "{url}");
    }
    for url in [
        "https://api.opencode.ai.example.com/v1",
        "https://example.com/opencode.ai/v1",
        "not a url",
    ] {
        assert!(!is_endpoint(url), "{url}");
    }
}

#[test]
fn conversation_id_is_sent_and_a_sessionless_call_gets_a_stable_process_id() {
    assert_eq!(session(Some("abc")), "abc");
    let fallback = session(None);
    assert!(fallback.starts_with("dial-") && fallback.len() > 20);
    assert_eq!(session(Some("")), fallback);
    assert_eq!(session(None), fallback);
}

#[test]
fn headers_identify_dial_and_never_duplicate_caller_headers() {
    assert_eq!(
        headers(Some("s1"), &[]),
        [(SESSION_HEADER, "s1"), ("User-Agent", USER_AGENT)]
    );
    assert!(USER_AGENT.starts_with("dial/"));
    assert_eq!(
        headers(Some("s1"), &[("user-agent", "mine/1"), ("X-Other", "1")]),
        [(SESSION_HEADER, "s1")]
    );
    assert!(
        headers(
            Some("s1"),
            &[("X-OpenCode-Session", "own"), ("User-Agent", "x")]
        )
        .is_empty()
    );
}
