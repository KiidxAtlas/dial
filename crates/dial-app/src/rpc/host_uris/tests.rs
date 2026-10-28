use super::*;

#[test]
fn declared_schemes_are_lowercased_and_deduplicated() {
    let schemes = parse_schemes(&json!(["Jira", "notion+v2"])).unwrap();
    assert_eq!(schemes, vec!["jira".to_owned(), "notion+v2".to_owned()]);
    assert!(parse_schemes(&json!(["jira", "JIRA"])).is_err());
}

#[test]
fn invalid_or_built_in_schemes_are_refused() {
    for schemes in [
        json!("jira"),
        json!([42]),
        json!(["1jira"]),
        json!(["ji ra"]),
        json!(["file"]),
        json!(["ssh"]),
        json!(["https"]),
        json!(["a".repeat(33)]),
        json!(vec!["x"; MAX_SCHEMES + 1]),
    ] {
        assert!(parse_schemes(&schemes).is_err(), "accepted {schemes}");
    }
}

#[test]
fn only_declared_schemes_are_served_and_reset_clears_them() {
    let uris = HostUris::default();
    uris.declare(vec!["jira".into()]);
    assert!(uris.serves("jira"));
    assert!(!uris.serves("notion"));
    uris.reset();
    assert!(!uris.serves("jira"));
}

#[test]
fn host_answer_is_the_read_result() {
    let uris = HostUris::default();
    let cancel = AtomicBool::new(false);
    let out = run_call(
        &uris.bridge,
        &cancel,
        "host URI jira",
        Duration::from_secs(5),
        |call_id| {
            uris.complete(call_id, "# Ticket 7".into(), false).unwrap();
            true
        },
        || false,
    );
    assert!(!out.is_error);
    assert_eq!(out.text, "# Ticket 7");
}

#[test]
fn oversized_host_answer_becomes_a_bounded_error() {
    let uris = HostUris::default();
    let cancel = AtomicBool::new(false);
    let out = run_call(
        &uris.bridge,
        &cancel,
        "host URI jira",
        Duration::from_secs(5),
        |call_id| {
            let big = "x".repeat(MAX_RESULT_BYTES + 1);
            uris.complete(call_id, big, false).unwrap();
            true
        },
        || false,
    );
    assert!(out.is_error);
    assert!(out.text.contains("exceeds"), "{}", out.text);
}

#[test]
fn silent_host_times_out() {
    let uris = HostUris::default();
    let cancel = AtomicBool::new(false);
    let out = run_call(
        &uris.bridge,
        &cancel,
        "host URI jira",
        Duration::from_millis(5),
        |_| true,
        || false,
    );
    assert!(out.is_error);
    assert!(out.text.contains("timed out"), "{}", out.text);
}

#[test]
fn abort_cancels_a_waiting_read() {
    let uris = HostUris::default();
    let cancel = AtomicBool::new(true);
    let out = run_call(
        &uris.bridge,
        &cancel,
        "host URI jira",
        Duration::from_secs(5),
        |_| true,
        || false,
    );
    assert!(out.is_error);
    assert!(out.text.contains("cancelled"), "{}", out.text);
}

#[test]
fn answers_for_unknown_reads_are_refused() {
    let uris = HostUris::default();
    assert!(uris.complete("no-such-call", "x".into(), false).is_err());
}

#[test]
fn a_session_switch_unregisters_schemes_so_later_reads_are_refused() {
    let uris = HostUris::default();
    uris.declare(vec!["jira".into()]);
    assert!(uris.serves("jira"));
    // The same reset `drop_host_tools` runs on new_session, resume, fork and close_session.
    uris.reset();
    assert!(!uris.serves("jira"));
    // With the scheme unserved, `read` falls through to the built-in resource schemes, which
    // refuse it as unregistered.
    let error = dial_tools::tools::integrations::resources::scheme("jira://PROJ-1")
        .expect_err("an unregistered scheme must be refused");
    assert!(error.contains("unsupported resource scheme"));
}
