use super::*;

#[test]
fn samples_need_one_source_and_tool_arguments_must_be_json() {
    let refused = sample(None, None, Some("bash".into()), Some("{not json".into())).unwrap_err();
    assert!(refused.to_string().contains("--args"), "{refused}");
    assert!(sample(None, None, None, None).is_err());
    assert!(matches!(
        sample(None, Some("plan".into()), None, None).unwrap(),
        Sample::Thinking(text) if text == "plan"
    ));
    assert!(matches!(
        sample(None, None, Some("bash".into()), Some(r#"{"command":"ls"}"#.into())).unwrap(),
        Sample::Tool(call) if call.name == "bash" && call.arguments["command"] == "ls"
    ));
}
