use super::*;

#[test]
fn unknown_or_repeated_answers_are_refused() {
    let calls = ClientFsCalls::default();
    assert!(calls.complete("host-1", "text".into(), false).is_err());
}
