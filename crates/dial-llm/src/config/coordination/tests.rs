use super::*;
use crate::config::{Config, schema};

#[test]
fn coordination_defaults_on_and_accepts_explicit_opt_out() {
    assert!(Config::default().coordination.enabled);
    let config: Config = toml::from_str("[coordination]\nenabled = false\n").unwrap();
    assert!(!config.coordination.enabled);
    assert!(toml::from_str::<CoordinationConfig>("").unwrap().enabled);
}

#[test]
fn coordination_rejects_unknown_fields_and_wrong_types() {
    assert!(toml::from_str::<Config>("[coordination]\nenabeld = false\n").is_err());
    assert!(toml::from_str::<Config>("[coordination]\nenabled = 'false'\n").is_err());
}

#[test]
fn coordination_setting_uses_typed_default_and_requires_restart() {
    let setting = schema::registry()
        .iter()
        .find(|setting| setting.path == "coordination.enabled")
        .unwrap();
    assert_eq!(setting.kind, schema::Kind::Bool);
    assert_eq!(setting.default_value(), Some(toml::Value::Boolean(true)));
    assert!(setting.restart);
}
