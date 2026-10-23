use super::*;
use serde_json::json;

fn step(method: &str, args: Vec<Value>) -> ComputerCallStep {
    ComputerCallStep {
        method: method.into(),
        args,
    }
}

#[test]
fn computer_call_read_only_classification() {
    assert_eq!(
        is_read_only_computer_call(&[step("screenshot", vec![])]),
        Ok(true)
    );
    assert_eq!(
        is_read_only_computer_call(&[step("click", vec![json!(1), json!(2)])]),
        Ok(false)
    );
    assert_eq!(
        is_read_only_computer_call(&[step("window", vec![json!(7)]), step("ax", vec![])]),
        Ok(true)
    );
    assert_eq!(
        is_read_only_computer_call(&[step("window", vec![json!(7)]), step("raise", vec![])]),
        Ok(false)
    );
}

#[test]
fn computer_call_rejects_invalid_chains() {
    assert!(validate_chain(&[]).is_err());
    assert!(validate_chain(&[step("nope", vec![])]).is_err());
    assert!(validate_chain(&[step("apps.list", vec![]), step("x", vec![])]).is_err());
    assert!(
        validate_chain(&[
            step("window", vec![json!(1)]),
            step("ax", vec![]),
            step("ax", vec![]),
        ])
        .is_err()
    );
}

#[test]
fn computer_call_renders_literals() {
    let rendered =
        render_computer_call(&[step("click", vec![json!(10), json!({"button": "left"})])]).unwrap();
    assert_eq!(
        rendered,
        r#"return await desktop.click(10, JSON.parse("{\"button\":\"left\"}"));"#
    );
    let two =
        render_computer_call(&[step("window", vec![json!(3)]), step("raise", vec![])]).unwrap();
    assert_eq!(two, "return await (await desktop.window(3)).raise();");
}
