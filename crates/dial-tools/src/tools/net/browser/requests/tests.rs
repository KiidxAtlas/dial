use super::*;

#[test]
fn sensitive_headers_are_redacted_everywhere_in_output() {
    let record = json!({
        "id": 1,
        "method": "GET",
        "url": "https://example.test/api",
        "request_headers": {
            "Authorization": "Bearer abc",
            "Cookie": "session=1",
            "X-Session-Token": "t",
            "Accept": "application/json"
        },
        "response_headers": {"Set-Cookie": "sid=2", "Content-Type": "text/plain"}
    });
    let shown = shape_requests(&json!({"records": [record]}));
    let text = shown.to_string();
    assert!(!text.contains("Bearer abc"));
    assert!(!text.contains("session=1"));
    assert!(!text.contains("sid=2"));
    assert!(!text.contains("X-Session-Token\":\"t"));
    assert!(text.contains("application/json"));
    assert_eq!(
        shown["requests"][0]["request_headers"]["Authorization"],
        REDACTED
    );
}

#[test]
fn bound_text_strips_controls_and_cuts_on_a_character_boundary() {
    let (text, truncated) = bound_text("a\u{7}b\tc", 64);
    assert_eq!(text, "ab\tc");
    assert!(!truncated);

    let (text, truncated) = bound_text("ééé", 3);
    assert_eq!(text, "é");
    assert!(truncated);
}

#[test]
fn request_body_is_returned_only_when_textual_and_bounded() {
    let big = "x".repeat(REQUEST_BODY_MAX_BYTES + 10);
    let shown = shape_request(&json!({
        "record": {"id": 2, "url": "https://example.test/", "request_headers": {}},
        "content_type": "text/plain; charset=utf-8",
        "body": big,
        "body_bytes": big.len()
    }));
    assert_eq!(
        shown["body"].as_str().map(str::len),
        Some(REQUEST_BODY_MAX_BYTES)
    );
    assert_eq!(shown["body_truncated"], true);

    let binary = shape_request(&json!({
        "record": {"id": 3, "url": "https://example.test/i.png", "request_headers": {}},
        "content_type": "image/png",
        "body": "\u{0}\u{1}",
        "body_bytes": 2
    }));
    assert!(binary.get("body").is_none());
    assert_eq!(binary["body_omitted"], "binary");
}

#[test]
fn request_log_arguments_are_bounded() {
    assert_eq!(
        parse("requests", &json!({})).unwrap()["limit"],
        REQUESTS_DEFAULT_LIMIT
    );
    assert!(parse("requests", &json!({"limit": 0})).is_err());
    assert!(parse("requests", &json!({"limit": REQUESTS_MAX_LIMIT + 1})).is_err());
    assert!(parse("request", &json!({})).is_err());
    assert!(parse("request", &json!({"request_id": 0})).is_err());
    assert_eq!(
        parse("request", &json!({"request_id": 7})).unwrap()["request_id"],
        7
    );
    assert_eq!(parse("har_start", &json!({})).unwrap()["content"], "text");
    assert!(parse("har_start", &json!({"content": "base64"})).is_err());
}

#[test]
fn har_document_is_written_redacted_with_query_pairs_and_body_budget() {
    let dir = tempfile::tempdir().unwrap();
    let body = "y".repeat(HAR_BODY_MAX_BYTES);
    let result = json!({
        "dropped": 0,
        "content": "text",
        "entries": [
            {
                "id": 1,
                "method": "GET",
                "url": "https://example.test/search?q=rust&page=2",
                "status": 200,
                "duration_ms": 12.5,
                "started": "2024-01-01T00:00:00.000Z",
                "request_headers": {"Authorization": "Bearer secret-value"},
                "response_headers": {"Content-Type": "text/html", "Set-Cookie": "sid=9"},
                "response_bytes": 5,
                "body": body
            },
            {
                "id": 2,
                "method": "GET",
                "url": "https://example.test/second",
                "status": 200,
                "request_headers": {},
                "response_headers": {"Content-Type": "text/html"},
                "response_bytes": 5,
                "body": "hello"
            }
        ]
    });
    let summary = write_har(&result, dir.path()).unwrap();
    assert_eq!(summary["entries"], 2);
    let path = summary["path"].as_str().unwrap();
    let written = std::fs::read_to_string(path).unwrap();
    assert!(!written.contains("secret-value"));
    assert!(!written.contains("sid=9"));
    let document: Value = serde_json::from_str(&written).unwrap();
    let first = &document["log"]["entries"][0];
    assert_eq!(first["request"]["queryString"][0]["name"], "q");
    assert_eq!(first["request"]["queryString"][1]["value"], "2");
    assert_eq!(
        first["response"]["content"]["text"].as_str().map(str::len),
        Some(HAR_BODY_MAX_BYTES)
    );
    assert_eq!(document["log"]["version"], "1.2");
}

#[test]
fn har_binary_body_is_base64_encoded_and_driver_omissions_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let result = json!({
        "entries": [{
            "id": 3,
            "method": "GET",
            "url": "https://example.test/image.png",
            "status": 200,
            "response_headers": {"content-type": "image/png"},
            "body_base64": "AAEC"
        }],
        "bodies_omitted": 2,
        "content": "all"
    });
    let summary = write_har(&result, dir.path()).unwrap();
    let written = std::fs::read_to_string(summary["path"].as_str().unwrap()).unwrap();
    let document: Value = serde_json::from_str(&written).unwrap();
    let content = &document["log"]["entries"][0]["response"]["content"];
    assert_eq!(content["text"], "AAEC");
    assert_eq!(content["encoding"], "base64");
    assert_eq!(summary["bodies_omitted"], 2);
}

#[test]
fn sensitive_query_names_match_words_and_camel_case_only() {
    for name in [
        "token",
        "API_KEY",
        "x-api-key",
        "accessToken",
        "access_token",
        "id_token",
        "sig",
        "X-Amz-Signature",
        "code",
        "session",
        "auth",
        "credential",
        "client_secret",
        "password",
    ] {
        assert!(is_sensitive_query_name(name), "{name} should be redacted");
    }
    for name in [
        "q",
        "page",
        "design",
        "signup",
        "barcode-scan",
        "state",
        "id",
    ] {
        assert!(!is_sensitive_query_name(name), "{name} should be kept");
    }
}

#[test]
fn redact_url_query_keeps_names_order_and_fragment() {
    assert_eq!(
        redact_url_query("https://example.test/cb?code=abc&state=ok&api%5Fkey=k1&q=a%20b#frag"),
        "https://example.test/cb?code=[redacted]&state=ok&api%5Fkey=[redacted]&q=a%20b#frag"
    );
    assert_eq!(
        redact_url_query("https://example.test/a?token"),
        "https://example.test/a?token=[redacted]"
    );
    assert_eq!(
        redact_url_query("https://example.test/a"),
        "https://example.test/a"
    );
}

#[test]
fn request_list_and_detail_never_show_sensitive_query_values() {
    let record = json!({
        "id": 3,
        "method": "GET",
        "url": "https://example.test/callback?access_token=tok-123&page=4",
        "request_headers": {"Referer": "https://example.test/login?session=sess-9"},
        "response_headers": {"Location": "https://example.test/next?code=code-7"}
    });
    let listed = shape_requests(&json!({"records": [record.clone()]})).to_string();
    let detailed = shape_request(&json!({"record": record, "body_bytes": 0})).to_string();
    for text in [listed, detailed] {
        assert!(!text.contains("tok-123"), "{text}");
        assert!(!text.contains("sess-9"), "{text}");
        assert!(!text.contains("code-7"), "{text}");
        assert!(text.contains("access_token=[redacted]"), "{text}");
        assert!(text.contains("page=4"), "{text}");
    }
}

#[test]
fn har_written_file_redacts_query_values_in_url_and_query_string() {
    let dir = tempfile::tempdir().unwrap();
    let result = json!({
        "dropped": 0,
        "content": "text",
        "entries": [{
            "id": 1,
            "method": "GET",
            "url": "https://example.test/cb?api_key=hunter2&q=rust",
            "status": 302,
            "duration_ms": 1.0,
            "started": "2024-01-01T00:00:00.000Z",
            "request_headers": {},
            "response_headers": {"Location": "https://example.test/next?sig=sig-value"},
            "response_bytes": 0
        }]
    });
    let summary = write_har(&result, dir.path()).unwrap();
    let written = std::fs::read_to_string(summary["path"].as_str().unwrap()).unwrap();
    assert!(!written.contains("hunter2"));
    assert!(!written.contains("sig-value"));
    let document: Value = serde_json::from_str(&written).unwrap();
    let request = &document["log"]["entries"][0]["request"];
    assert_eq!(request["queryString"][0]["name"], "api_key");
    assert_eq!(request["queryString"][0]["value"], "[redacted]");
    assert_eq!(request["queryString"][1]["value"], "rust");
    let location = &document["log"]["entries"][0]["response"]["redirectURL"];
    assert_eq!(location, "https://example.test/next?sig=[redacted]");
}
