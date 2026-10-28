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

#[test]
fn locate_requires_a_bounded_description_on_the_viewport() {
    assert!(validate("locate", &json!({ "description": "the Save button" })).is_ok());
    for args in [
        json!({}),
        json!({ "description": "  " }),
        json!({ "description": "x".repeat(501) }),
        json!({ "description": "Save", "full_page": true }),
    ] {
        assert!(validate("locate", &args).is_err(), "{args}");
    }
}

#[test]
fn located_device_pixels_map_to_viewport_css_pixels() {
    let located = crate::tools::vision::grounding::Located {
        x: 1961,
        y: 1251,
        width: 2560,
        height: 1600,
        model: "mlx/m".into(),
        reply: String::new(),
    };
    assert_eq!(css_point(&located, 1280.0, 800.0), (980.5, 625.5));
}
