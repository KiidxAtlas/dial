use super::*;

#[test]
fn lean_local_accepts_true_false_and_auto_and_defaults_to_auto() {
    let lean =
        |text: &str| toml::from_str::<AssistanceConfig>(text).map(|config| config.lean_local);
    assert_eq!(AssistanceConfig::default().lean_local, LeanLocal::Auto);
    assert_eq!(lean("").unwrap(), LeanLocal::Auto);
    assert_eq!(lean("lean_local = \"auto\"").unwrap(), LeanLocal::Auto);
    assert_eq!(lean("lean_local = true").unwrap(), LeanLocal::On);
    assert_eq!(lean("lean_local = false").unwrap(), LeanLocal::Off);
    assert!(lean("lean_local = \"maybe\"").is_err());
    assert!(lean("lean_local = \"true\"").is_err());
    // Serialized values keep the shape each one was written in.
    let written = |value| {
        toml::Table::try_from(AssistanceConfig {
            lean_local: value,
            ..AssistanceConfig::default()
        })
        .unwrap()["lean_local"]
            .clone()
    };
    assert_eq!(written(LeanLocal::Auto), toml::Value::String("auto".into()));
    assert_eq!(written(LeanLocal::On), toml::Value::Boolean(true));
    assert_eq!(written(LeanLocal::Off), toml::Value::Boolean(false));
    assert_eq!(LeanLocal::from(true), LeanLocal::On);
}
