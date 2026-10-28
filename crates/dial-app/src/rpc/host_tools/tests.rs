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
    let out = run_call(
        &bridge,
        &interrupt,
        "lookup_ticket",
        Duration::from_secs(5),
        |call_id| {
            bridge.complete(call_id, "not found".into(), true).unwrap();
            true
        },
        || false,
    );
    assert!(out.is_error);
    assert_eq!(out.text, "not found");
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
