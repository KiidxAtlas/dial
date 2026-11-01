use super::*;

fn names() -> Vec<String> {
    vec!["read".into(), "bash".into()]
}

#[test]
fn json_call_shapes_are_matched_with_flexible_spacing_and_fences() {
    for text in [
        r#"{"name":"read","arguments":{"path":"a"}}"#,
        r#"{"name": "read", "arguments": {}}"#,
        "{\n  \"name\" : \"read\",\n  \"parameters\": {}\n}",
        "```json\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\n```",
        r#"{{"name": "read", "arguments": {"path": "a""#,
    ] {
        assert!(looks_like_text_call(text, &names()), "{text}");
    }
}

#[test]
fn prose_unknown_tools_and_prefix_names_are_not_calls() {
    for text in [
        "I would call read with the path.",
        r#"Use {"name": "read", "arguments": {}} like this"#,
        r#"{"name": "reader", "arguments": {}}"#,
        r#"{"name": "write", "arguments": {}}"#,
        r#"{"name": "read"}"#,
    ] {
        assert!(!looks_like_text_call(text, &names()), "{text}");
    }
    assert!(!looks_like_text_call(&"{".repeat(9000), &names()));
}

#[test]
fn unlifted_dialect_markup_naming_an_offered_tool_is_a_call() {
    for text in [
        "<tool_call>read<arg_key>path</arg_key>",
        "<｜tool▁call▁begin｜>function<｜tool▁sep｜>read",
        "<|tool_call_begin|>functions.bash:0<|tool_call_argument_begin|>{",
        "[TOOL_CALLS]read[ARGS]{}",
    ] {
        assert!(looks_like_text_call(text, &names()), "{text}");
    }
    // The markup must name an offered tool, and a bare section marker is not a call.
    assert!(!looks_like_text_call("<tool_call>write", &names()));
    assert!(!looks_like_text_call(
        "<|tool_calls_section_begin|> read",
        &names()
    ));
}

#[test]
fn shared_byte_prefix_preserves_unicode_and_maximum_byte_budget() {
    let text = "aé🦀z";
    for (max, expected) in [
        (0, ""),
        (1, "a"),
        (2, "a"),
        (3, "aé"),
        (6, "aé"),
        (7, "aé🦀"),
        (usize::MAX, text),
    ] {
        assert_eq!(byte_prefix(text, max), expected);
    }
}
