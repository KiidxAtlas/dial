use super::*;
use serde_json::json;

#[test]
fn only_new_actions_and_conditioned_waits_are_owned_here() {
    assert!(is_page_action("click_at", &json!({})));
    assert!(is_page_action("wait", &json!({"text": "Saved"})));
    assert!(!is_page_action("wait", &json!({"selector": "#ready"})));
    assert!(!is_page_action("click", &json!({"selector": "#go"})));
    assert!(!is_page_action("wait", &json!({"url": null})));
}

#[test]
fn coordinates_must_be_finite_numbers_within_limit() {
    assert!(validate("mouse_move", &json!({"x": 10, "y": -5.5})).is_ok());
    assert!(validate("click_at", &json!({"x": 10})).is_err());
    assert!(validate("click_at", &json!({"x": "10", "y": 1})).is_err());
    assert!(validate("click_at", &json!({"x": 100001, "y": 1})).is_err());
    assert!(validate("click_at", &json!({"x": 1, "y": -100000})).is_ok());
}

#[test]
fn wheel_and_scroll_require_a_delta_and_accept_zero() {
    assert!(validate("wheel", &json!({})).is_err());
    assert!(validate("wheel", &json!({"dy": 0})).is_ok());
    assert!(validate("scroll", &json!({"selector": "#list"})).is_err());
    assert!(validate("scroll", &json!({"selector": "#list", "dx": 3})).is_ok());
}

#[test]
fn button_accepts_only_playwright_button_names() {
    assert!(validate("mouse_down", &json!({})).is_ok());
    assert!(validate("mouse_up", &json!({"button": "right"})).is_ok());
    assert!(validate("click_at", &json!({"x": 1, "y": 1, "button": "back"})).is_err());
}

#[test]
fn selector_is_required_for_element_actions_but_optional_for_text() {
    assert!(validate("dblclick", &json!({})).is_err());
    assert!(validate("dblclick", &json!({"selector": ""})).is_err());
    assert!(validate("count", &json!({"selector": "li"})).is_ok());
    assert!(validate("text", &json!({})).is_ok());
    let long = "a".repeat(SELECTOR_MAX_BYTES + 1);
    assert!(validate("html", &json!({"selector": long})).is_err());
}

#[test]
fn drag_needs_source_and_target_selectors() {
    assert!(validate("drag", &json!({"selector": "#a"})).is_err());
    assert!(validate("drag", &json!({"selector": "#a", "target": "#b"})).is_ok());
}

#[test]
fn attr_requires_a_name() {
    assert!(validate("attr", &json!({"selector": "a"})).is_err());
    assert!(validate("attr", &json!({"selector": "a", "name": "href"})).is_ok());
}

#[test]
fn styles_props_are_bounded_css_names() {
    assert!(validate("styles", &json!({"selector": "p"})).is_ok());
    assert!(validate("styles", &json!({"selector": "p", "props": []})).is_err());
    assert!(validate("styles", &json!({"selector": "p", "props": ["color; x"]})).is_err());
    assert!(validate("styles", &json!({"selector": "p", "props": ["--brand-1"]})).is_ok());
    let too_many: Vec<String> = (0..=STYLE_PROPERTIES_MAX)
        .map(|i| format!("p{i}"))
        .collect();
    assert!(validate("styles", &json!({"selector": "p", "props": too_many})).is_err());
}

#[test]
fn highlight_duration_is_bounded() {
    assert!(validate("highlight", &json!({"selector": "p"})).is_ok());
    assert!(validate("highlight", &json!({"selector": "p", "duration": 10000})).is_ok());
    assert!(validate("highlight", &json!({"selector": "p", "duration": 10001})).is_err());
    assert!(validate("highlight", &json!({"selector": "p", "duration": -1})).is_err());
}

#[test]
fn key_actions_bound_key_names() {
    assert!(validate("key_down", &json!({"key": "Shift"})).is_ok());
    assert!(validate("key_up", &json!({})).is_err());
    let long = "k".repeat(KEY_MAX_BYTES + 1);
    assert!(validate("key_down", &json!({"key": long})).is_err());
}

#[test]
fn wait_conditions_are_exclusive_and_scoped() {
    assert!(validate("wait", &json!({"url": "/dashboard"})).is_ok());
    assert!(validate("wait", &json!({"response": "/api/save"})).is_ok());
    assert!(validate("wait", &json!({"text": "Saved", "selector": "#status"})).is_ok());
    assert!(validate("wait", &json!({"url": "/a", "response": "/b"})).is_err());
    assert!(validate("wait", &json!({"url": "/a", "selector": "#x"})).is_err());
    assert!(validate("wait", &json!({"response": "/b", "text": "x"})).is_err());
    assert!(validate("wait", &json!({"url": ""})).is_err());
    assert!(validate("wait", &json!({"text": "Saved", "state": "visible"})).is_err());
}
