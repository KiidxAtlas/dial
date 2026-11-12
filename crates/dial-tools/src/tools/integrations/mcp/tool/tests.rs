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
    assert!(read.declared_read_only && !write.declared_read_only);
    let contradictory = RemoteTool::parse(
        &json!({"name":"wipe","annotations":{"readOnlyHint":true,"destructiveHint":true}}),
    )
    .unwrap();
    assert!(
        !contradictory.declared_read_only,
        "destructive is never effect-free"
    );
    let open_world = RemoteTool::parse(
        &json!({"name":"search","annotations":{"readOnlyHint":true,"openWorldHint":true}}),
    )
    .unwrap();
    assert!(
        open_world.declared_read_only,
        "an open-world read is still a read"
    );
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

#[test]
fn png_images_travel_in_the_shared_attachment_envelope() {
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB";
    let result = json!({"content":[
        {"type":"text","text":"chart"},
        {"type":"image","data":png,"mimeType":"image/png"},
        {"type":"image","data":"/9j/4AAQSkZJRgABAQ","mimeType":"image/jpeg"},
    ]});
    let output = render_result(&result);
    assert_eq!(
        output.text,
        "chart\n[image content, image/png, attached for models that accept images]\n[image content, image/jpeg, not shown]"
    );
    let data = output.data.unwrap();
    assert_eq!(
        data["artifact"],
        json!({"data": png, "encoding": "base64", "media_type": "image/png"})
    );
    assert_eq!(data["content"], result["content"]);
    // Bytes that are not a PNG, and error results, are noted only.
    let fake =
        render_result(&json!({"content":[{"type":"image","data":"AAAA","mimeType":"image/png"}]}));
    assert!(fake.data.unwrap().get("artifact").is_none());
    let failed = render_result(
        &json!({"isError":true,"content":[{"type":"image","data":png,"mimeType":"image/png"}]}),
    );
    assert!(failed.data.unwrap().get("artifact").is_none());
}

#[test]
fn jpeg_gif_and_webp_images_attach_when_their_bytes_match_the_declared_type() {
    use base64::Engine;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let cases = [
        (
            "image/jpeg",
            encode(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00"),
        ),
        ("image/gif", encode(b"GIF89a\x01\x00\x01\x00\x80\x00\x00")),
        ("image/webp", encode(b"RIFF\x24\x00\x00\x00WEBPVP8 ")),
    ];
    for (kind, data) in &cases {
        let output = render_result(&json!({"content":[
            {"type":"image","data":data,"mimeType":kind},
        ]}));
        assert_eq!(
            output.text,
            format!("[image content, {kind}, attached for models that accept images]")
        );
        assert_eq!(
            output.data.unwrap()["artifact"],
            json!({"data": data, "encoding": "base64", "media_type": kind})
        );
    }
    // A JPEG labelled as WebP is noted, and the first genuine image attaches instead.
    let (_, jpeg) = &cases[0];
    let (_, gif) = &cases[1];
    let output = render_result(&json!({"content":[
        {"type":"image","data":jpeg,"mimeType":"image/webp"},
        {"type":"image","data":gif,"mimeType":"image/gif"},
    ]}));
    assert_eq!(
        output.text,
        "[image content, image/webp, not shown]\n[image content, image/gif, attached for models that accept images]"
    );
    assert_eq!(output.data.unwrap()["artifact"]["media_type"], "image/gif");
    // Other image types and audio stay noted only.
    let other = render_result(&json!({"content":[
        {"type":"image","data":encode(b"<svg xmlns='x'/>"),"mimeType":"image/svg+xml"},
        {"type":"audio","data":"AAAA","mimeType":"audio/wav"},
    ]}));
    assert!(other.data.unwrap().get("artifact").is_none());
}

#[test]
fn structured_content_is_not_repeated_after_the_same_text() {
    let both = render_result(&json!({
        "content":[{"type":"text","text":"{\"n\": 1, \"m\": [2]}"}],
        "structuredContent":{"m":[2],"n":1},
    }));
    assert_eq!(both.text, "{\"n\": 1, \"m\": [2]}");
    let different = render_result(&json!({
        "content":[{"type":"text","text":"summary"}],
        "structuredContent":{"n":1},
    }));
    assert_eq!(different.text, "summary\n{\"n\":1}");
}

#[test]
fn oversized_input_schemas_are_shortened_in_steps() {
    let small = json!({"type":"object","properties":{"q":{"type":"string","description":"query"}}});
    assert_eq!(bounded_input_schema(&small), (small.clone(), false));

    let long = "x".repeat(MAX_INPUT_SCHEMA);
    let annotated = json!({"type":"object","required":["description"],"properties":{
        "description":{"type":"string","description":long},
        "mode":{"type":"string","enum":["a","b"],"title":"Mode"},
    }});
    let (lean, shortened) = bounded_input_schema(&annotated);
    assert!(shortened);
    assert_eq!(
        lean,
        json!({"type":"object","required":["description"],"properties":{
            "description":{"type":"string"},
            "mode":{"type":"string","enum":["a","b"]},
        }})
    );

    let nested = json!({"type":"object","required":["a"],"properties":{
        "a":{"type":"object","properties":{"deep":{"type":"string","enum":[long]}}},
    }});
    let (flat, _) = bounded_input_schema(&nested);
    assert_eq!(
        flat,
        json!({"type":"object","required":["a"],"properties":{"a":{"type":"object"}}})
    );

    let wide = json!({"type":"object","properties":{"a":{"enum":[long]}}});
    assert_eq!(bounded_input_schema(&wide).0, json!({"type":"object"}));
}

#[test]
fn context_consolidates_both_surfaces_without_losing_rpc_arguments_or_sync_identity() {
    use crate::tools::integrations::mcp::load;
    use crate::tools::integrations::mcp::tests::{config_with, fake_config};
    use crate::tools::testkit;

    let mut server = fake_config("context", 30);
    server.lifecycle_hooks = vec!["turn_started".into()];
    let config = config_with(vec![("fake", server)]);
    let mut loaded = load(&config);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    loaded.live.sync();
    let offered = loaded.live.offered_tools();
    assert!(!offered.iter().any(|tool| matches!(
        tool.name(),
        "mcp__fake__read_resource" | "mcp__fake__get_prompt"
    )));
    let context = offered
        .iter()
        .find(|tool| tool.name() == "mcp__fake__context")
        .unwrap();
    assert!(offered.iter().any(|tool| tool.name() == "mcp__fake__echo"));
    assert_eq!(context.lifecycle_hooks(), &["turn_started"]);
    assert_eq!(context.lifecycle_channel(), Some("fake"));
    let (_dir, workspace) = testkit::workspace();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    for (args, pointer, expected) in [
        (
            json!({"surface":"resource"}),
            "/resources/0/uri",
            json!("fake:///readme"),
        ),
        (
            json!({"surface":"resource"}),
            "/templates/0/uriTemplate",
            json!("fake:///{name}"),
        ),
        (
            json!({"surface":"resource","uri":"fake:///chosen"}),
            "/contents/0/uri",
            json!("fake:///chosen"),
        ),
        (
            json!({"surface":"prompt"}),
            "/prompts/0/name",
            json!("review"),
        ),
        (
            json!({"surface":"prompt","name":"chosen","arguments":{"topic":"rust"}}),
            "/description",
            json!("chosen"),
        ),
    ] {
        assert!(context.needs_approval(&args));
        assert!(!context.read_only_hint());
        let output = context.call(&ctx, &args);
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(
            output.data.as_ref().unwrap().pointer(pointer),
            Some(&expected)
        );
        if args.get("arguments").is_some() {
            let forwarded: Value = serde_json::from_str(
                output.data.as_ref().unwrap()["messages"][0]["content"]["text"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                forwarded,
                json!({"name":"chosen","arguments":{"topic":"rust"}})
            );
        }
    }
    for args in [json!({}), json!({"surface":"tools"})] {
        assert!(context.call(&ctx, &args).is_error);
    }
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let output = context.call_streaming(
        &ctx,
        &json!({"surface":"resource","uri":"fake:///chosen"}),
        &|_| {},
        &cancel,
    );
    assert!(output.is_error, "pre-cancelled reads must not dispatch");
    assert!(loaded.live.sync().upserted.is_empty());
    let after = loaded.live.offered_tools();
    assert!(Arc::ptr_eq(
        context,
        after
            .iter()
            .find(|tool| tool.name() == context.name())
            .unwrap()
    ));
}

#[test]
fn context_rejects_unadvertised_capabilities() {
    use crate::tools::integrations::mcp::load;
    use crate::tools::integrations::mcp::tests::{config_with, fake_config};
    use crate::tools::testkit;

    for (flags, unsupported) in [("resources-only", "prompt"), ("prompts-only", "resource")] {
        let config = config_with(vec![("fake", fake_config(flags, 30))]);
        let loaded = load(&config);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let offered = loaded.live.offered_tools();
        let context = offered
            .iter()
            .find(|tool| tool.name() == "mcp__fake__context")
            .unwrap();
        let (_dir, workspace) = testkit::workspace();
        let ctx = ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        let output = context.call(&ctx, &json!({"surface":unsupported}));
        assert!(output.is_error);
        assert!(
            output.text.contains("does not currently advertise"),
            "{}",
            output.text
        );
    }
}
