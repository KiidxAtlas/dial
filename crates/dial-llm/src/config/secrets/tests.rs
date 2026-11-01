use super::*;

#[test]
fn debug_never_exposes_literal_secrets() {
    let config = Config {
        enabled: true,
        values: vec!["private-literal-secret".into()],
        ..Default::default()
    };
    let debug = format!("{config:?}");
    assert!(debug.contains("REDACTED"));
    assert!(!debug.contains(&config.values[0]));
    assert_eq!(
        serde_json::to_value(&config).unwrap()["values"][0],
        config.values[0]
    );
}
