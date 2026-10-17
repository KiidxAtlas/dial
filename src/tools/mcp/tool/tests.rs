use super::*;

#[test]
fn text_blocks_are_joined_and_other_blocks_noted_by_type() {
    let out = render_result(&json!({"content": [
        {"type": "text", "text": "one"},
        {"type": "image", "data": "AAAA", "mimeType": "image/png"},
        {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "body"}},
        {"type": "resource", "resource": {"uri": "file:///b.bin", "blob": "AA", "mimeType": "application/octet-stream"}},
        {"type": "resource_link", "uri": "https://x/y"},
        {"type": "hologram"},
    ]}));
    assert!(!out.is_error);
    assert_eq!(
        out.text,
        "one\n[image content, image/png, not shown]\n[resource: file:///a.txt]\nbody\n\
             [resource: file:///b.bin, application/octet-stream, binary content not shown]\n\
             [resource link: https://x/y]\n[hologram content not shown]"
    );
}

#[test]
fn is_error_maps_and_empty_results_are_explained() {
    let failed =
        render_result(&json!({"isError": true, "content": [{"type": "text", "text": "boom"}]}));
    assert!(failed.is_error);
    assert_eq!(failed.text, "boom");
    assert_eq!(
        render_result(&json!({"content": []})).text,
        "(the tool returned no content)"
    );
    assert_eq!(
        render_result(&json!({"structuredContent": {"n": 1}})).text,
        r#"{"n":1}"#
    );
}

#[test]
fn output_is_bounded_on_a_char_boundary_and_stripped_of_escapes() {
    let big = "é".repeat(MAX_RESULT);
    let out = render_result(&json!({"content": [{"type": "text", "text": big}]}));
    assert!(out.text.contains("[output truncated:"));
    assert!(out.text.len() < MAX_RESULT + 100);
    let escaped =
        render_result(&json!({"content": [{"type": "text", "text": "a\u{1b}[31mred\u{7}\nb"}]}));
    assert_eq!(escaped.text, "a[31mred\nb");
}

#[test]
fn unnamed_remote_tools_are_rejected() {
    assert!(RemoteTool::parse(&json!({"description": "no name"})).is_none());
    assert!(RemoteTool::parse(&json!({"name": ""})).is_none());
}

#[test]
fn read_only_annotation_is_parsed_from_tool_annotations() {
    let read =
        RemoteTool::parse(&json!({"name":"read","annotations":{"readOnlyHint":true}})).unwrap();
    let write =
        RemoteTool::parse(&json!({"name":"write","annotations":{"readOnlyHint":false}})).unwrap();
    assert!(read.read_only_hint);
    assert!(!write.read_only_hint);
}

#[test]
fn resource_contents_render_text_and_note_blobs() {
    let out = render_resource(&json!({"contents": [
        {"uri": "file:///a.txt", "text": "body"},
        {"uri": "file:///b.bin", "blob": "AA", "mimeType": "application/octet-stream"},
    ]}));
    assert_eq!(
        out.text,
        "[resource: file:///a.txt]\nbody\n\
             [resource: file:///b.bin, application/octet-stream, binary content not shown]"
    );
    assert_eq!(
        render_resource(&json!({"contents": []})).text,
        "(the resource has no content)"
    );
}

#[test]
fn prompt_messages_render_with_role_and_description() {
    let out = render_prompt(&json!({
        "description": "review",
        "messages": [
            {"role": "user", "content": {"type": "text", "text": "check this"}},
            {"role": "assistant", "content": {"type": "text", "text": "ok"}},
        ],
    }));
    assert_eq!(out.text, "review\nuser: check this\nassistant: ok");
    assert_eq!(
        render_prompt(&json!({"messages": []})).text,
        "(the prompt returned no messages)"
    );
}
