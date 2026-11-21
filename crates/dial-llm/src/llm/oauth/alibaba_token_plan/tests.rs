use super::super::tests::{Ui, endpoint, idle, request};
use super::*;
use serde_json::json;

fn unused_endpoints() -> Endpoints<'static> {
    Endpoints {
        international: "http://127.0.0.1:1/v1",
        china: "http://127.0.0.1:1/v1",
    }
}

#[test]
fn flow_invariants() {
    assert_eq!(FLOW.id, "alibaba-token-plan");
    assert_eq!(FLOW.base_url, INTERNATIONAL_API_BASE_URL);
    assert!(matches!(FLOW.protocol, ApiProtocol::ChatCompletions));
    const { assert!(FLOW.needs_terminal && FLOW.subscription) };
    assert!(FLOW.refresh.is_none());
    assert_eq!(
        crate::llm::oauth::flow("alibaba-token-plan").map(|f| f.name),
        Some("QwenCloud Token Plan")
    );
}

#[test]
fn international_login_stores_key_without_endpoint_or_cookie() {
    let (base, server) = endpoint(200, json!({"data": []}));
    // Empty cookie answer skips quota reporting.
    let ui = Ui::new(&["1", " sk-sp-test ", ""]);
    let endpoints = Endpoints {
        international: &base,
        china: "http://127.0.0.1:1/v1",
    };
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();

    let request = request(&server).to_ascii_lowercase();
    assert!(request.starts_with("get /v1/models http/1.1"));
    assert!(request.contains("authorization: bearer sk-sp-test"));

    assert_eq!(credentials.access, "sk-sp-test");
    assert!(credentials.refresh.is_empty());
    assert_eq!(credentials.expires_at_ms, util::NEVER_EXPIRES);
    assert_eq!(credentials.api_endpoint, None);
    assert!(!credentials.extra.contains_key("cookie"));
    assert_eq!(
        *ui.auths.lock(),
        vec![(
            INTERNATIONAL_AUTH_URL.to_owned(),
            Some(
                "Subscribe to Token Plan Individual and copy its dedicated API key. Keep this page open; the next prompt explains how to enable optional quota reporting."
                    .to_owned()
            ),
        )]
    );
}

#[test]
fn china_login_stores_region_endpoint_and_normalized_cookie() {
    let (china, server) = endpoint(200, json!({}));
    let ui = Ui::new(&["2", "sk-sp-cn", "  cookie: a=1; b=2  "]);
    let endpoints = Endpoints {
        international: "http://127.0.0.1:1/v1",
        china: &china,
    };
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();

    assert!(request(&server).starts_with("GET /v1/models"));
    assert_eq!(credentials.api_endpoint.as_deref(), Some(china.as_str()));
    assert_eq!(
        credentials.extra.get("cookie").and_then(|v| v.as_str()),
        Some("a=1; b=2")
    );
    assert_eq!(ui.auths.lock()[0].0, CHINA_AUTH_URL);
}

#[test]
fn custom_region_normalizes_url_and_stores_it_as_endpoint() {
    let (base, server) = endpoint(200, json!({}));
    let custom = format!("{base}//");
    let ui = Ui::new(&["3", custom.as_str(), "sk-sp-custom", ""]);
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints()).unwrap();

    assert!(request(&server).starts_with("GET /v1/models"));
    assert_eq!(credentials.api_endpoint.as_deref(), Some(base.as_str()));
    assert_eq!(credentials.access, "sk-sp-custom");
}

#[test]
fn lone_cookie_value_is_rejected() {
    let (base, _server) = endpoint(200, json!({}));
    let ui = Ui::new(&["1", "sk-sp-x", "abc123"]);
    let endpoints = Endpoints {
        international: &base,
        china: "",
    };
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints)
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Invalid QwenCloud Cookie header."));
    assert!(error.contains(INTERNATIONAL_COOKIE_HOST));
}

#[test]
fn rejected_key_fails_without_echoing_the_key_or_body() {
    let (base, _server) = endpoint(403, json!({"error": "sk-sp-leaked"}));
    let ui = Ui::new(&["1", "sk-sp-leaked"]);
    let endpoints = Endpoints {
        international: &base,
        china: "",
    };
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "QwenCloud Token Plan API key validation failed (HTTP 403)"
    );
}

#[test]
fn empty_custom_url_is_rejected() {
    let ui = Ui::new(&["3", "  "]);
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints())
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Custom URL is required for option 3");
}

#[test]
fn cookie_normalization_and_shape_check() {
    assert_eq!(normalize_cookie("  Cookie:  a=1 "), "a=1");
    assert_eq!(normalize_cookie("COOKIE:a=1"), "a=1");
    assert_eq!(normalize_cookie("   "), "");
    assert!(is_cookie_header("a=1"));
    assert!(is_cookie_header("junk; b=2"));
    assert!(!is_cookie_header("abc"));
    assert!(!is_cookie_header("=x"));
    assert!(!is_cookie_header("a="));
}

#[test]
fn access_uses_the_stored_region_endpoint_when_present() {
    let regional = Credentials {
        access: "sk-a".into(),
        api_endpoint: Some("https://example.test/v1".into()),
        expires_at_ms: util::NEVER_EXPIRES,
        ..Credentials::default()
    };
    assert_eq!(
        access(&regional).unwrap().base_url.as_deref(),
        Some("https://example.test/v1")
    );

    let international = Credentials {
        access: "sk-b".into(),
        ..Credentials::default()
    };
    let shaped = access(&international).unwrap();
    assert_eq!(shaped.token.as_str(), "sk-b");
    assert_eq!(shaped.base_url, None);
}
