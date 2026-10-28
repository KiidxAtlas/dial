use super::*;
use std::collections::HashSet;

#[test]
fn device_table_has_unique_names() {
    let unique: HashSet<&str> = DEVICE_NAMES.iter().copied().collect();
    assert_eq!(unique.len(), DEVICE_NAMES.len());
    assert_eq!(DEVICE_NAMES.len(), 131);
}

#[test]
fn unknown_device_is_rejected_before_any_browser_work() {
    let error = parse_emulate(&json!({"device": "iPhone 99"})).unwrap_err();
    assert!(error.contains("unknown device"), "{error}");
    assert!(parse_emulate(&json!({"device": "iPhone 15 Pro"})).is_ok());
}

#[test]
fn emulate_requires_at_least_one_option() {
    assert!(parse_emulate(&json!({"action": "emulate"})).is_err());
    assert!(parse_emulate(&json!({"device": null})).is_err());
}

#[test]
fn viewport_bounds_are_enforced() {
    assert!(parse_emulate(&json!({"viewport": {"width": 0, "height": 720}})).is_err());
    assert!(parse_emulate(&json!({"viewport": {"width": 1280, "height": 7681}})).is_err());
    assert!(
        parse_emulate(&json!({"viewport": {"width": 1280, "height": 720, "scale": 0}})).is_err()
    );
    assert!(
        parse_emulate(&json!({"viewport": {"width": 1280, "height": 720, "scale": 5}})).is_err()
    );
    let ok = parse_emulate(&json!({"viewport": {"width": 390, "height": 844, "scale": 2}}))
        .expect("valid viewport");
    assert_eq!(ok["viewport"]["scale"], json!(2.0));
}

#[test]
fn viewport_rejects_unknown_fields() {
    let error =
        parse_emulate(&json!({"viewport": {"width": 1, "height": 1, "mobile": true}})).unwrap_err();
    assert!(error.contains("does not accept"), "{error}");
}

#[test]
fn geolocation_null_is_a_clear_and_coordinates_are_bounded() {
    let cleared = parse_emulate(&json!({"geolocation": null, "offline": false})).expect("clear");
    assert_eq!(cleared["geolocation"], Value::Null);
    assert!(parse_emulate(&json!({"geolocation": {"latitude": 91, "longitude": 0}})).is_err());
    assert!(parse_emulate(&json!({"geolocation": {"latitude": 0, "longitude": 181}})).is_err());
    assert!(
        parse_emulate(&json!({"geolocation": {"latitude": 0, "longitude": 0, "accuracy": -1}}))
            .is_err()
    );
}

#[test]
fn color_scheme_and_offline_are_typed() {
    assert!(parse_emulate(&json!({"color_scheme": "sepia"})).is_err());
    assert!(parse_emulate(&json!({"color_scheme": "dark"})).is_ok());
    assert!(parse_emulate(&json!({"offline": "yes"})).is_err());
}

#[test]
fn locale_must_be_a_plain_tag_and_empty_resets() {
    assert!(parse_emulate(&json!({"locale": "en-US; drop"})).is_err());
    let reset = parse_emulate(&json!({"locale": ""})).expect("empty locale resets");
    assert_eq!(reset["locale"], json!(""));
}

#[test]
fn user_agent_cannot_be_empty_or_contain_controls() {
    assert!(parse_emulate(&json!({"user_agent": ""})).is_err());
    assert!(parse_emulate(&json!({"user_agent": "a\nb"})).is_err());
}

#[test]
fn open_options_split_tab_scope_from_session_scope() {
    let options = parse_open(&json!({
        "viewport": {"width": 800, "height": 600},
        "user_agent": "agent/1",
        "ignore_https_errors": true
    }))
    .expect("open options");
    let tab = options.tab.expect("tab-scoped fields");
    assert_eq!(tab["viewport"]["width"], json!(800));
    assert_eq!(tab["user_agent"], json!("agent/1"));
    assert_eq!(options.ignore_https_errors, Some(true));

    let bare = parse_open(&json!({"url": "https://example.com"})).expect("plain open");
    assert!(bare.tab.is_none());
    assert!(bare.ignore_https_errors.is_none());
}

#[test]
fn open_rejects_non_boolean_https_policy() {
    assert!(parse_open(&json!({"ignore_https_errors": "yes"})).is_err());
}
