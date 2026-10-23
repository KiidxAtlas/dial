use super::*;

#[test]
fn computer_error_payload_round_trip() {
    let error = ComputerError::abort("stopped");
    let payload = error.to_payload();
    assert_eq!(ComputerError::from_payload(&payload), error);
}

#[test]
fn computer_parse_null_args_as_empty_object() {
    let parsed: ListApplicationsArgs = parse("listApplications", Value::Null).unwrap();
    assert!(parsed.options.is_none());
}

#[cfg(unix)]
#[test]
fn clipboard_helpers_are_bounded_and_failures_are_classified() {
    let short = std::time::Duration::from_millis(300);
    assert!(matches!(
        run_clipboard("dial-no-such-clipboard-helper", &[], None, short),
        Err(ClipboardFailure::Missing(_))
    ));
    match run_clipboard("sh", &["-c", "echo no display >&2; exit 3"], None, short) {
        Err(ClipboardFailure::Failed(message)) => assert!(message.contains("no display")),
        _ => panic!("a helper that ran and failed is a failure, not a missing helper"),
    }
    let started = std::time::Instant::now();
    assert!(matches!(
        run_clipboard("sh", &["-c", "exec sleep 30"], None, short),
        Err(ClipboardFailure::Failed(message)) if message.contains("did not finish")
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(
        run_clipboard("sh", &["-c", "printf copied"], None, short).ok(),
        Some("copied".to_owned())
    );
    // A writer that never reads its input is still stopped by the deadline.
    let large = "x".repeat(1 << 20);
    assert!(matches!(
        run_clipboard("sh", &["-c", "exec sleep 30"], Some(&large), short),
        Err(ClipboardFailure::Failed(_))
    ));
}
