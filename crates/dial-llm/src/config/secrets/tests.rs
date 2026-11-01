use super::*;

#[test]
fn debug_output_never_contains_literal_secret_values() {
    let config = Config {
        enabled: true,
        env: true,
        values: vec!["sk-live-very-secret-value".into()],
    };
    let printed = format!("{config:?}");
    assert!(!printed.contains("very-secret"), "{printed}");
    assert!(printed.contains("1 redacted"), "{printed}");
}
