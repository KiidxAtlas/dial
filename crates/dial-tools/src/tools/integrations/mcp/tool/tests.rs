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

#[test]
fn protocol_capture_retention_and_text_presentation_are_independent() {
    let result = json!({"content":[{"type":"text","text":"caption"},{"type":"image","data":"AAAA"}],"structuredContent":{"n":1}});
    let output = render_result(&result);
    assert!(output.text.contains("caption"));
    assert!(output.text.contains(r#"{"n":1}"#));
    assert_eq!(output.data.as_ref(), Some(&result));
    let facts = output.execution.output.unwrap();
    assert_eq!(facts.capture, Completeness::Complete);
    assert_eq!(facts.retention, Completeness::Complete);
    assert_eq!(facts.presentation, Completeness::Partial);
    assert_eq!(facts.original_bytes, Some(result.to_string().len() as u64));
    assert_eq!(facts.omitted_bytes, Some(0));
    let bounded =
        render_result(&json!({"content":[{"type":"text","text":"x".repeat(MAX_RESULT * 2)}]}));
    assert_eq!(
        bounded.execution.output.as_ref().unwrap().presentation,
        Completeness::Partial
    );
    assert_eq!(
        bounded.execution.output.as_ref().unwrap().capture,
        Completeness::Complete
    );
    let plain = render_result(&json!({"content":[{"type":"text","text":"hello"}]}));
    assert_eq!(
        plain.execution.output.unwrap().presentation,
        Completeness::Complete
    );
}

#[test]
fn catalog_publication_at_admission_refuses_the_previously_approved_definition() {
    use super::super::tests::{config_with, fake_config};
    use crate::tools::testkit;
    let loaded = super::super::load(&config_with(vec![("fake", fake_config("", 30))]));
    let session = &loaded.sessions[0];
    let definitions = session.catalog_snapshot().1;
    let remote = definitions
        .iter()
        .find(|tool| tool.name == "echo")
        .unwrap()
        .clone();
    let tool = McpTool::with_lifecycle_hooks(
        "mcp__fake__echo".into(),
        "fake",
        remote,
        session.clone(),
        Duration::from_secs(2),
        vec![],
    );
    let first = std::cell::Cell::new(true);
    let (_dir, workspace) = testkit::workspace();
    let config = crate::config::types::Config::default();
    let output = tool.call_request(
        &ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        },
        &json!({}),
        &|| {
            if first.replace(false) {
                // A deterministic publication at the cancellation/admission boundary. Previously
                // this callback ran in request_with, after two independent catalog checks passed.
                let mut changed = definitions.clone();
                let echo = changed.iter_mut().find(|tool| tool.name == "echo").unwrap();
                echo.input_schema = json!({"type":"object", "required":["new_required_argument"]});
                echo.read_only_hint = !echo.read_only_hint;
                session.apply_catalog(changed);
            }
            false
        },
        &|_| {},
    );
    assert!(output.is_error, "{}", output.text);
    assert!(
        output.text.contains("definition changed"),
        "{}",
        output.text
    );
    let facts = output.execution.integration.unwrap();
    assert_eq!(facts.dispatch, DispatchState::NotDispatched);
    assert_eq!(facts.failure, Some(IntegrationFailure::Admission));
    assert_eq!(output.execution.effects, Effects::None);
}
