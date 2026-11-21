use super::*;

fn valid_tool() -> Value {
    json!({
        "name": "lookup_ticket",
        "description": "Look up a ticket",
        "parameters": {"type": "object", "properties": {"id": {"type": "string"}}}
    })
}

#[test]
fn valid_declaration_gets_the_default_timeout() {
    let specs = parse_specs(&json!([valid_tool()])).unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "lookup_ticket");
    assert_eq!(specs[0].timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
}

#[test]
fn invalid_declarations_are_refused() {
    let mut bad_name = valid_tool();
    bad_name["name"] = json!("Lookup-Ticket");
    let mut not_object = valid_tool();
    not_object["parameters"] = json!({"type": "string"});
    let mut zero_timeout = valid_tool();
    zero_timeout["timeout_ms"] = json!(0);
    for tools in [
        json!([]),
        json!([bad_name]),
        json!([valid_tool(), valid_tool()]),
        json!([not_object]),
        json!([zero_timeout]),
    ] {
        assert!(parse_specs(&tools).is_err(), "accepted {tools}");
    }
}

#[test]
fn host_answer_completes_the_call() {
    let bridge = HostBridge::default();
    let interrupt = AtomicBool::new(false);
    let out = run_call(
        &bridge,
        &interrupt,
        "lookup_ticket",
        Duration::from_secs(5),
        |call_id| {
            bridge.complete(call_id, "ticket 7".into(), false).unwrap();
            true
        },
        || false,
    );
    assert!(!out.is_error);
    assert_eq!(out.text, "ticket 7");
}

#[test]
fn host_error_answer_is_an_error_output() {
    let bridge = HostBridge::default();
    let interrupt = AtomicBool::new(false);
    let mut delivered = String::new();
    let out = run_call(
        &bridge,
        &interrupt,
        "lookup_ticket",
        Duration::from_secs(5),
        |call_id| {
            delivered = call_id.to_owned();
            bridge.complete(call_id, "not found".into(), true).unwrap();
            true
        },
        || false,
    );
    assert!(out.is_error);
    assert!(out.text.starts_with("not found"), "{}", out.text);
    assert_eq!(
        out.execution.effects,
        crate::runtime::execution::Effects::Unknown
    );
    assert!(out.text.contains(&delivered), "{}", out.text);
}

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=";

fn answer_with(params: Value) -> ToolOutput {
    let bridge = HostBridge::default();
    let interrupt = AtomicBool::new(false);
    run_call(
        &bridge,
        &interrupt,
        "host tool browser_screenshot",
        Duration::from_secs(5),
        |call_id| {
            let mut params = params.clone();
            params["call_id"] = json!(call_id);
            let (id, result) = parse_result(&params).unwrap();
            bridge.answer(id, result).unwrap();
            true
        },
        || false,
    )
}

#[test]
fn an_image_answer_travels_as_the_shared_image_envelope() {
    let out = answer_with(json!({"content": [
        {"type": "text", "text": "Screenshot of https://example.com"},
        {"type": "image", "data": PNG, "mimeType": "image/png"}
    ]}));
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.starts_with("Screenshot of https://example.com"),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("attached for models that accept images"),
        "{}",
        out.text
    );
    assert!(!out.text.contains(PNG), "pixels stay out of the text");
    let artifact = &out.data.as_ref().unwrap()["artifact"];
    assert_eq!(
        artifact,
        &json!({"data": PNG, "encoding": "base64", "media_type": "image/png"})
    );
}

#[test]
fn a_mislabelled_image_is_noted_not_attached_and_content_errors_name_the_call() {
    let out =
        answer_with(json!({"content": [{"type": "image", "data": PNG, "mimeType": "image/jpeg"}]}));
    assert!(
        out.data
            .as_ref()
            .is_none_or(|data| data.get("artifact").is_none())
    );
    let out = answer_with(
        json!({"content": [{"type": "text", "text": "page closed"}], "is_error": true}),
    );
    assert!(out.is_error);
    assert!(
        out.text
            .starts_with("page closed (host tool browser_screenshot, request host-1)"),
        "{}",
        out.text
    );
    assert_eq!(
        out.execution.effects,
        crate::runtime::execution::Effects::Unknown
    );
}

#[test]
fn a_result_takes_exactly_one_of_output_or_content() {
    let (_, text) = parse_result(&json!({"call_id": "host-1", "output": "ok"})).unwrap();
    assert!(matches!(text.body, HostBody::Text(ref t) if t == "ok"));
    for (params, message) in [
        (json!({"call_id": "host-1"}), "output or content required"),
        (
            json!({"call_id": "host-1", "output": "a", "content": [{"type": "text", "text": "b"}]}),
            "give output or content, not both",
        ),
        (
            json!({"call_id": "host-1", "output": 7}),
            "output must be a string",
        ),
        (
            json!({"call_id": "host-1", "content": []}),
            "content must be a non-empty array of content blocks",
        ),
        (
            json!({"call_id": "host-1", "content": [{"text": "untyped"}]}),
            "each content block must be an object with a string type",
        ),
        (json!({"output": "a"}), "call_id required"),
    ] {
        let error = parse_result(&params)
            .err()
            .unwrap_or_else(|| panic!("accepted {params}"));
        assert_eq!(error.to_string(), message, "{params}");
    }
}

#[test]
fn silent_host_times_out() {
    let bridge = HostBridge::default();
    let interrupt = AtomicBool::new(false);
    let out = run_call(
        &bridge,
        &interrupt,
        "lookup_ticket",
        Duration::from_millis(60),
        |_| true,
        || false,
    );
    assert!(out.is_error);
    assert!(out.text.contains("timed out"), "{}", out.text);
}

#[test]
fn abort_cancels_a_waiting_call() {
    let bridge = HostBridge::default();
    let interrupt = AtomicBool::new(true);
    let out = run_call(
        &bridge,
        &interrupt,
        "lookup_ticket",
        Duration::from_secs(5),
        |_| true,
        || false,
    );
    assert!(out.is_error);
    assert!(out.text.contains("cancelled"), "{}", out.text);
}

#[test]
fn undelivered_call_fails_without_waiting() {
    let bridge = HostBridge::default();
    let interrupt = AtomicBool::new(false);
    let out = run_call(
        &bridge,
        &interrupt,
        "lookup_ticket",
        Duration::from_secs(5),
        |_| false,
        || false,
    );
    assert!(out.is_error);
    assert!(out.text.contains("could not be delivered"), "{}", out.text);
}

#[test]
fn answers_for_finished_or_unknown_calls_are_refused() {
    let bridge = HostBridge::default();
    let interrupt = AtomicBool::new(false);
    let mut seen = String::new();
    let out = run_call(
        &bridge,
        &interrupt,
        "lookup_ticket",
        Duration::from_secs(5),
        |call_id| {
            seen = call_id.to_owned();
            bridge.complete(call_id, "first".into(), false).unwrap();
            true
        },
        || false,
    );
    assert_eq!(out.text, "first");
    assert!(bridge.complete(&seen, "again".into(), false).is_err());
    assert!(bridge.complete("host-unknown", "x".into(), false).is_err());
}

#[test]
fn host_tools_register_only_once_per_process() {
    let bridge = HostBridge::default();
    bridge.ensure_unregistered().unwrap();
    bridge.mark_registered(vec!["lookup_ticket".into()]);
    assert!(bridge.ensure_unregistered().is_err());
}

#[test]
fn reset_returns_the_declared_names_and_frees_the_registration() {
    let bridge = HostBridge::default();
    bridge.mark_registered(vec!["lookup_ticket".into()]);
    assert_eq!(bridge.reset(), vec!["lookup_ticket".to_owned()]);
    bridge.ensure_unregistered().unwrap();
    assert!(bridge.reset().is_empty());
}

#[test]
fn reset_cancels_a_waiting_call() {
    let bridge = HostBridge::default();
    bridge.mark_registered(vec!["lookup_ticket".into()]);
    let interrupt = AtomicBool::new(false);
    let reset_done = std::cell::Cell::new(false);
    let out = run_call(
        &bridge,
        &interrupt,
        "lookup_ticket",
        Duration::from_secs(5),
        |_| true,
        || {
            if !reset_done.replace(true) {
                bridge.reset();
            }
            false
        },
    );
    assert!(out.is_error);
    assert!(out.text.contains("was cancelled"), "{}", out.text);
}

#[test]
fn delivered_host_timeout_retains_identity_and_unknown_effects() {
    let bridge = HostBridge::default();
    let interrupt = AtomicBool::new(false);
    let mut delivered = String::new();
    let out = run_call(
        &bridge,
        &interrupt,
        "write buffer",
        Duration::ZERO,
        |call_id| {
            delivered = call_id.to_owned();
            true
        },
        || false,
    );
    assert_eq!(
        out.execution.effects,
        crate::runtime::execution::Effects::Unknown
    );
    assert!(out.text.contains(&delivered), "{}", out.text);
}
