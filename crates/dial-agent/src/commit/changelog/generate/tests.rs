use super::*;
use crate::llm::ToolCall;

#[test]
fn cap_chars_marks_elision() {
    assert_eq!(cap_chars("abcdef", 10), "abcdef");
    assert_eq!(cap_chars("abcdef", 4), "abcd\n[…2ch elided…]");
}

#[test]
fn extracts_entries_from_tool_call() {
    let reply = Reply {
        tool_calls: vec![ToolCall {
            id: "1".to_owned(),
            name: TOOL_NAME.to_owned(),
            arguments: json!({"entries": {"Added": ["thing"]}}),
            malformed_arguments: false,
        }],
        ..Reply::default()
    };
    let entries = extract_entries(&reply).expect("entries");
    assert_eq!(entries["Added"][0], "thing");
}

#[test]
fn extracts_entries_from_plain_text_json() {
    let reply = Reply {
        text: "Here: {\"entries\": {\"Fixed\": [\"bug\"]}} done".to_owned(),
        ..Reply::default()
    };
    let entries = extract_entries(&reply).expect("entries");
    assert_eq!(entries["Fixed"][0], "bug");
}

#[test]
fn render_existing_formats_sections() {
    let existing = vec![("Fixed".to_owned(), vec!["crash".to_owned()])];
    assert_eq!(render_existing(&existing), "### Fixed\n- crash");
}
