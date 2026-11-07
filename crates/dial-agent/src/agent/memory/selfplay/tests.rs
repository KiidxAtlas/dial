use super::*;

#[test]
fn custom_checks_are_explicit_foreground_verdicts() {
    let mut call = ToolCall {
        id: "compare".into(),
        name: "bash".into(),
        malformed_arguments: false,
        arguments: json!({"command":"cmp -s expected actual"}),
    };
    assert!(!is_check_call(&call));
    call.arguments["check"] = json!(true);
    assert!(is_check_call(&call));
    call.arguments["background"] = json!(true);
    assert!(!is_check_call(&call));
    call.arguments["background"] = json!(false);
    call.arguments["pty"] = json!(true);
    assert!(!is_check_call(&call));
    call.arguments = json!({"command":"cargo test"});
    assert!(
        is_check_call(&call),
        "existing runner detection is preserved"
    );
    call.name = "write".into();
    assert!(!is_check_call(&call));
}
