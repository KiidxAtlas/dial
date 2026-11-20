use super::super::tests::{Ui, endpoint, idle, request};
use super::*;

fn unused_endpoints() -> Endpoints<'static> {
    Endpoints {
        international: "http://127.0.0.1:1/v1",
        china: "http://127.0.0.1:1/v1",
    }
}

#[test]
fn flow_invariants() {
    assert_eq!(FLOW.id, "alibaba-coding-plan");
    assert_eq!(FLOW.base_url, DEFAULT_API_BASE_URL);
    assert!(matches!(FLOW.protocol, ApiProtocol::ChatCompletions));
    const { assert!(FLOW.needs_terminal && FLOW.subscription) };
    assert!(FLOW.refresh.is_none());
    assert_eq!(
        crate::llm::oauth::flow("alibaba-coding-plan").map(|f| f.name),
        Some("Alibaba Coding Plan")
    );
}

#[test]
fn international_login_validates_chat_probe_and_stores_endpoint() {
    let (base, server) = endpoint(200, json!({"choices": []}));
    let ui = Ui::new(&["1", "  sk-test-123 "]);
    let endpoints = Endpoints {
        international: &base,
        china: "http://127.0.0.1:1/v1",
    };
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();

    let request = request(&server).to_ascii_lowercase();
    assert!(request.starts_with("post /v1/chat/completions http/1.1"));
    assert!(request.contains("authorization: bearer sk-test-123"));
    assert!(request.contains("\"max_tokens\":1"));
    assert!(request.contains("\"model\":\"qwen3.5-plus\""));

    assert_eq!(credentials.access, "sk-test-123");
    assert!(credentials.refresh.is_empty());
    assert_eq!(credentials.expires_at_ms, util::NEVER_EXPIRES);
    assert_eq!(credentials.enterprise_url.as_deref(), Some(base.as_str()));
    assert_eq!(
        *ui.auths.lock(),
        vec![(
            DEFAULT_AUTH_URL.to_owned(),
            Some(
                "Copy your API key from the Alibaba Cloud DashScope console (International)"
                    .to_owned()
            ),
        )]
    );
}

#[test]
fn china_login_opens_china_console_and_validates_its_endpoint() {
    let (china, server) = endpoint(200, json!({}));
    let ui = Ui::new(&["2", "sk-cn"]);
    let endpoints = Endpoints {
        international: "http://127.0.0.1:1/v1",
        china: &china,
    };
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();
    assert!(request(&server).starts_with("POST /v1/chat/completions"));
    assert_eq!(credentials.enterprise_url.as_deref(), Some(china.as_str()));
    assert_eq!(ui.auths.lock()[0].0, CHINA_AUTH_URL);
}

#[test]
fn custom_endpoint_normalizes_url_and_validates_models() {
    let (base, server) = endpoint(200, json!({"data": []}));
    let custom = format!("{base}//");
    let ui = Ui::new(&["3", custom.as_str(), "sk-custom"]);
    let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints()).unwrap();

    let request = request(&server).to_ascii_lowercase();
    assert!(request.starts_with("get /v1/models http/1.1"));
    assert!(request.contains("authorization: bearer sk-custom"));
    assert_eq!(credentials.enterprise_url.as_deref(), Some(base.as_str()));
    assert_eq!(credentials.access, "sk-custom");
    assert_eq!(ui.auths.lock()[0].0, DEFAULT_AUTH_URL);
}

#[test]
fn rejected_key_fails_without_echoing_the_key_or_body() {
    let (base, _server) = endpoint(401, json!({"error": "bad sk-leaked"}));
    let ui = Ui::new(&["1", "sk-leaked"]);
    let endpoints = Endpoints {
        international: &base,
        china: "",
    };
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "Alibaba Coding Plan API key validation failed (HTTP 401)"
    );
}

#[test]
fn empty_key_is_rejected_before_any_request() {
    let ui = Ui::new(&["1", "   "]);
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints())
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Alibaba Coding Plan API key is required");
}

#[test]
fn empty_custom_url_is_rejected() {
    let ui = Ui::new(&["3", " / "]);
    let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints())
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Custom URL is required for option 3");
}

#[test]
fn cancellation_after_region_choice_stops_before_the_key_prompt() {
    let cancel = Arc::new(AtomicBool::new(true));
    let ui = Ui::new(&["1", "sk-never-used"]);
    let error = sign_in(&ui, &cancel, &|u, i| ui.auth(u, i), &unused_endpoints())
        .unwrap_err()
        .to_string();
    assert_eq!(error, "sign-in cancelled");
    assert!(ui.auths.lock().is_empty());
}

#[test]
fn access_uses_the_stored_key_and_endpoint_override() {
    let credentials = Credentials {
        access: "sk-a".into(),
        enterprise_url: Some("https://example.test/v1".into()),
        expires_at_ms: util::NEVER_EXPIRES,
        ..Credentials::default()
    };
    let shaped = access(&credentials).unwrap();
    assert_eq!(shaped.token.as_str(), "sk-a");
    assert_eq!(shaped.base_url.as_deref(), Some("https://example.test/v1"));

    let bare = Credentials {
        access: "sk-b".into(),
        ..Credentials::default()
    };
    assert_eq!(access(&bare).unwrap().base_url, None);
}
