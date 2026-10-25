use super::*;
use crate::llm::errors::{self, ErrorClass};
use crate::llm::fake;
use serde_json::json;

fn request(base_url: &str) -> Result<Reply> {
    exchange(
        &Route {
            protocol: ApiProtocol::Copilot,
            base_url,
            model: "claude-opus-5.5",
            auth: Some("fixture-token"),
            account_id: None,
            headers: &[("X-Fixture", "operator-header")],
            project_id: None,
            allow_insecure: true,
            session_id: None,
            request_id: None,
        },
        &Chat {
            messages: vec![json!({"role": "user", "content": "hello"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 16,
            seed: None,
        },
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        &mut |_| true,
    )
}

#[test]
fn unsupported_model_adds_account_recovery_without_losing_typed_http_failure() {
    let server = fake::serve(vec![fake::json(
        400,
        &json!({"error": {"message": "The requested model is not supported."}}),
    )]);
    let error = request(&server.url).unwrap_err();
    let diagnostic = error.to_string();
    for detail in ["Copilot", "claude-opus-5.5", "policy", "plan", "/models"] {
        assert!(diagnostic.contains(detail), "missing {detail:?}: {error:#}");
    }
    assert!(diagnostic.to_lowercase().contains("enable"), "{error:#}");
    let http = error.downcast_ref::<crate::llm::HttpError>().unwrap();
    assert_eq!(http.status, 400);
    assert_eq!(http.class, ErrorClass::Fatal);
    assert!(
        http.to_string()
            .contains("The requested model is not supported.")
    );
    assert_eq!(errors::classify(&error), ErrorClass::Fatal);
    assert!(!errors::classify(&error).is_retryable());
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn other_bad_requests_and_auth_failures_do_not_get_model_policy_advice() {
    for (status, message, class) in [
        (400, "Invalid request parameter", ErrorClass::Fatal),
        (
            401,
            "The requested model is not supported.",
            ErrorClass::Auth,
        ),
    ] {
        let server = fake::serve(vec![fake::json(
            status,
            &json!({"error": {"message": message}}),
        )]);
        let error = request(&server.url).unwrap_err();
        let http = error.downcast_ref::<crate::llm::HttpError>().unwrap();
        assert_eq!(http.status, status);
        assert_eq!(http.class, class);
        assert_eq!(errors::classify(&error), class);
        assert!(http.to_string().contains(message));
        assert_eq!(error.to_string(), http.to_string());
        assert_eq!(server.requests().len(), 1);
    }
}

#[test]
fn successful_chat_keeps_the_selected_model_route_and_identity_headers() {
    let server = fake::serve(vec![fake::json(
        200,
        &json!({"choices": [{"finish_reason": "stop", "message": {"content": "hello back"}}]}),
    )]);
    assert_eq!(request(&server.url).unwrap().text, "hello back");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let raw = &requests[0];
    assert_eq!(fake::request_line(raw), "POST /chat/completions HTTP/1.1");
    assert_eq!(fake::body(raw)["model"], "claude-opus-5.5");
    assert_eq!(
        fake::header(raw, "Authorization").as_deref(),
        Some("Bearer fixture-token")
    );
    assert_eq!(
        fake::header(raw, "X-Fixture").as_deref(),
        Some("operator-header")
    );
    for (name, value) in COPILOT_HEADERS {
        assert_eq!(fake::header(raw, name).as_deref(), Some(value), "{name}");
    }
}

#[test]
fn request_fixture_uses_chat_endpoint_and_identity_headers() {
    assert_eq!(
        copilot_chat_url("https://api.githubcopilot.com/"),
        "https://api.githubcopilot.com/chat/completions"
    );
    assert_eq!(
        copilot_chat_url("https://enterprise.example/chat/completions"),
        "https://enterprise.example/chat/completions"
    );
    assert!(COPILOT_HEADERS.contains(&("Copilot-Integration-Id", "copilot-chat")));
    assert!(COPILOT_HEADERS.contains(&("X-GitHub-Api-Version", "2026-08-01")));
}
