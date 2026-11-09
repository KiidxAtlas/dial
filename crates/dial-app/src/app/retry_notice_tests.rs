use crate::app::notices::RetryNotice;

#[test]
fn retry_notices_are_recognized_and_other_text_is_not() {
    let retry = RetryNotice::parse("transient provider failure: chat request failed … Connection refused (os error 61); retrying in 3.4s (attempt 4/10)").unwrap();
    assert_eq!(retry.max, "10");
    assert!(retry.cause.starts_with("chat request failed"));
    assert_eq!(retry.attempt, 4);
    assert_eq!(retry.delay, std::time::Duration::from_millis(3400));
    assert!(RetryNotice::parse("Changed files this turn: 2").is_none());
    assert!(RetryNotice::parse("retrying in 3s").is_none());
}

#[test]
fn a_recovered_request_is_closed_with_one_line_naming_the_attempt() {
    let retry = RetryNotice::parse("transient: reset (after 1.0s); retrying in 2.0s (attempt 2/5)")
        .unwrap();
    let text = RetryNotice::recovered_text(retry.attempt);
    assert_eq!(
        text,
        "The model request succeeded on attempt 2; the turn continues."
    );
    assert!(!text.contains('\n'));
}

#[test]
fn malformed_retry_timing_or_attempts_never_create_a_countdown() {
    for tail in [
        "NaNs (attempt 1/2)",
        "-1s (attempt 1/2)",
        "infs (attempt 1/2)",
        "1s (attempt 0/2)",
        "1s (attempt 3/2)",
        "1s (attempt 1/0)",
        "1s (attempt unknown/2)",
    ] {
        assert!(
            RetryNotice::parse(&format!("failure: cause; retrying in {tail}")).is_none(),
            "{tail}"
        );
    }
}
