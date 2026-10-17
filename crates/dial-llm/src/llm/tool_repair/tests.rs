use super::*;

fn tool(name: &str) -> Value {
    json!({"type":"function","function":{"name":name,"description":"d","parameters":{
        "type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}})
}

#[test]
fn schema_has_one_branch_per_tool_with_const_name() {
    let schema = schema_for_tools(&[tool("read"), tool("write")]).unwrap();
    let branches = schema["oneOf"].as_array().unwrap();
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[1]["properties"]["name"]["const"], "write");
    assert_eq!(
        branches[0]["properties"]["arguments"]["required"][0],
        "path"
    );
    assert_eq!(branches[0]["additionalProperties"], false);
}

#[test]
fn schema_rejects_no_tools_and_non_object_parameters() {
    assert!(schema_for_tools(&[]).is_err());
    let bad = json!({"type":"function","function":{"name":"x","parameters":true}});
    assert!(schema_for_tools(&[bad]).is_err());
}

#[test]
fn constrained_output_becomes_tool_call() {
    let offered = vec!["read".to_owned()];
    let call = call_from_reply(
        r#" {"name":"read","arguments":{"path":"a.rs"}} "#,
        &offered,
        "repair-1".into(),
    )
    .unwrap();
    assert_eq!(call.id, "repair-1");
    assert_eq!(call.name, "read");
    assert_eq!(call.arguments, json!({"path":"a.rs"}));
    assert!(!call.malformed_arguments);
}

#[test]
fn invalid_constrained_output_is_rejected() {
    let offered = vec!["read".to_owned()];
    for text in [
        "plain prose",
        r#"{"name":"delete","arguments":{}}"#,
        r#"{"name":"read","arguments":["a"]}"#,
        r#"{"name":"read"}"#,
        r#"{"name":"read","arguments":{},"extra":1}"#,
    ] {
        assert!(
            call_from_reply(text, &offered, "x".into()).is_err(),
            "accepted {text}"
        );
    }
}
