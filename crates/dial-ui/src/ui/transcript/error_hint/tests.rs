use super::*;

#[test]
fn common_provider_errors_get_a_kind_and_a_next_step() {
    let kind = |m: &str| classify(m).map(|(kind, _)| kind);
    assert_eq!(
        kind("HTTP 429 Too Many Requests: rate limit reached"),
        Some("rate limited")
    );
    assert_eq!(
        kind("401 Unauthorized: invalid x-api-key"),
        Some("not signed in")
    );
    assert_eq!(
        kind("400: prompt is too long: 210000 tokens > 200000 maximum"),
        Some("context too long")
    );
    assert_eq!(kind("404: model not found: gpt-9"), Some("model not found"));
    assert_eq!(kind("529 Overloaded"), Some("provider busy"));
    assert_eq!(
        kind("error sending request: operation timed out"),
        Some("network problem")
    );
    assert!(
        classify("401 Unauthorized")
            .unwrap()
            .1
            .contains("/providers")
    );
}

#[test]
fn unrecognized_errors_and_numbers_inside_words_get_no_guess() {
    assert_eq!(classify("could not parse tool arguments"), None);
    assert_eq!(classify("wrote 4290 bytes"), None);
    assert_eq!(classify("file does not exist"), None);
}
