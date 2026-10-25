use super::*;

#[test]
fn spans_cover_only_the_secret_group() {
    let text = "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345 end";
    let found: Vec<_> = patterns()
        .iter()
        .flat_map(|pattern| pattern.spans(text))
        .map(|span| &text[span])
        .collect();
    assert_eq!(found, ["abcdefghijklmnopqrstuvwxyz012345"]);
}

#[test]
fn boundaries_reject_embedded_shapes() {
    let text = "xAKIAABCDEFGHIJKLMNOP AKIAABCDEFGHIJKLMNOPQ";
    assert!(
        patterns()
            .iter()
            .all(|pattern| pattern.spans(text).is_empty())
    );
}

#[test]
fn environment_filter_keeps_only_long_secret_named_values() {
    let env = env_secrets([
        ("MY_API_KEY".to_owned(), "zq9-literal-value".to_owned()),
        ("HOME".to_owned(), "/Users/someone".to_owned()),
        ("DB_PASSWORD".to_owned(), "short".to_owned()),
        ("AUTH_PATH".to_owned(), "/etc/secret/path".to_owned()),
    ]);
    assert_eq!(env, ["zq9-literal-value"]);
}
