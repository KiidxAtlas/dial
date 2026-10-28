use super::*;
use serde_json::json;

#[test]
fn screenshot_quality_requires_jpeg_and_stays_in_range() {
    assert!(validate("screenshot", &json!({})).is_ok());
    assert!(validate("screenshot", &json!({ "format": "jpeg", "quality": 80 })).is_ok());
    assert!(validate("screenshot", &json!({ "quality": 80 })).is_err());
    assert!(validate("screenshot", &json!({ "format": "jpeg", "quality": 0 })).is_err());
    assert!(validate("screenshot", &json!({ "format": "jpeg", "quality": 101 })).is_err());
    assert!(validate("screenshot", &json!({ "format": "gif" })).is_err());
}

#[test]
fn screenshot_flags_must_be_booleans() {
    assert!(
        validate(
            "screenshot",
            &json!({ "annotate": true, "if_changed": false })
        )
        .is_ok()
    );
    assert!(validate("screenshot", &json!({ "annotate": "yes" })).is_err());
    assert!(validate("screenshot", &json!({ "if_changed": 1 })).is_err());
}

#[test]
fn record_restart_must_be_boolean() {
    assert!(validate("record_start", &json!({ "record_restart": true })).is_ok());
    assert!(validate("record_start", &json!({ "record_restart": "true" })).is_err());
}

#[test]
fn capture_actions_are_recognised() {
    assert!(is_action("record_stop"));
    assert!(is_action("vitals"));
    assert!(!is_action("screenshot"));
    assert!(!is_action("click"));
}
