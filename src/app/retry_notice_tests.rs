use super::RetryNotice;

#[test]
fn retry_notices_are_recognized_and_other_text_is_not() {
    let retry = RetryNotice::parse("transient provider failure: chat request failed … Connection refused (os error 61); retrying in 3.4s (attempt 4/10)").unwrap();
    assert_eq!(retry.max, "10");
    assert!(retry.cause.starts_with("chat request failed"));
    assert!(RetryNotice::parse("Changed files this turn: 2").is_none());
    assert!(RetryNotice::parse("retrying in 3s").is_none());
}
