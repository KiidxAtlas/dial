use super::*;

#[test]
fn options_are_strict_and_opt_in_before_any_action() {
    for args in [
        json!({}),
        json!({"observe":{}}),
        json!({"observe":{"snapshot":true,"screenshot":false,"events":true,"max_nodes":1000}}),
    ] {
        assert!(validate("click", &args).is_ok(), "{args}");
    }
    for value in [
        json!(null),
        json!([]),
        json!({"screenshot":"yes"}),
        json!({"max_nodes":0}),
        json!({"max_nodes":1001}),
        json!({"extra":true}),
    ] {
        assert!(
            validate("click", &json!({"observe":value})).is_err(),
            "{value}"
        );
    }
    for action in ["evaluate", "run", "call", "shutdown", "close", "screenshot"] {
        assert!(
            validate(action, &json!({"observe":{}})).is_err(),
            "{action}"
        );
        assert!(validate(action, &json!({})).is_ok(), "{action}");
    }
}
