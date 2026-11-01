use super::*;
use serde_json::json;

#[test]
fn only_stateless_read_only_bash_joins_parallel_groups() {
    let ext = Registry::new();
    for args in [
        json!({"command":"ls -la"}),
        json!({"command":"cat README.md","timeout":30}),
        json!({"command":"ls","background":false}),
        json!({"command":"ls","pty":false}),
    ] {
        assert!(parallel_read(&ext, "bash", &args), "{args}");
    }
    for args in [
        json!({"command":"rm -rf target"}),
        json!({"command":"ls > out.txt"}),
        json!({"command":"ls","background":true}),
        json!({"command":"ls","async":true}),
        json!({"command":"ls","session":"review"}),
        json!({"command":"ls","background":"yes"}),
        json!({"command":"ls","pty":true}),
        json!({"command":"ls","pty":"yes"}),
    ] {
        assert!(!parallel_read(&ext, "bash", &args), "{args}");
    }
}

#[test]
fn terminal_shells_do_not_inherit_read_only_command_approval() {
    let ext = Registry::new();
    for (pty, readonly) in [
        (Value::Null, true),
        (json!(false), true),
        (json!(true), false),
        (json!("yes"), false),
    ] {
        let call = ToolCall {
            id: "terminal".into(),
            name: "bash".into(),
            arguments: json!({"command":"ls", "pty":pty}),
            malformed_arguments: false,
        };
        assert_eq!(
            is_read_only(&ext, &call.name, &call.arguments),
            readonly,
            "{pty}"
        );
        assert_eq!(needs_approval(&ext, &call), !readonly, "{pty}");
    }
}
