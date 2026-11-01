//! Behaviour of Snapcompact layer views: which tool results are tagged, which stay expanded, and
//! how view calls fail closed. Filter: `cargo test agent::compact::snapcompact::views`.
use super::*;
use crate::agent::compact::snapcompact::archive_with_options;
use crate::config::compaction::CompactionConfig;
use crate::config::snapcompact::SnapcompactConfig;
use serde_json::json;

fn layer_call(id: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "session_search".into(),
        arguments,
        malformed_arguments: false,
    }
}

fn layer_args(id: &str) -> Value {
    json!({"view": "layers", "layer": id, "query": ""})
}

fn view_output(text: &str) -> ToolOutput {
    let mut output = ToolOutput::ok(text.into());
    output.data = Some(json!({"snapcompact_view": true}));
    output
}

fn tagged_tool(id: &str, body: &str, args: Value) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": id,
        "content": body,
        "is_error": false,
        "dial_snapcompact_view": args,
    })
}

fn is_retired(message: &Value) -> bool {
    message["content"]
        .as_str()
        .is_some_and(|text| text.starts_with("[Earlier Snapcompact view retired"))
}

fn identity() -> CompactionIdentity<'static> {
    CompactionIdentity {
        provider: "provider",
        model: "model",
        agent: None,
    }
}

fn context(policy: &CompactionConfig, window: u32, vision: bool) -> ViewContext<'_> {
    ViewContext {
        policy,
        identity: identity(),
        window,
        vision,
        family: SnapcompactFamily::Unknown,
    }
}

/// A real two-generation overlay history: the newest archive plus one following user message.
/// Built through the public archive API so the views read the same layered metadata as production.
fn layered_history() -> Vec<Value> {
    let cancel = AtomicBool::new(false);
    let config = SnapcompactConfig {
        overlay: true,
        ..SnapcompactConfig::default()
    };
    let mut messages = vec![json!({"role": "user", "content": "first generation"})];
    for generation in 0..2 {
        let archive =
            archive_with_options(&messages, &config, 64, &cancel).expect("layered archive builds");
        messages = archive.messages;
        messages.push(json!({
            "role": "user",
            "content": format!("following message {generation}"),
        }));
    }
    messages
}

#[test]
fn mark_tags_only_a_successful_snapcompact_view_result() {
    let args = layer_args("L1");
    let mut message = json!({"role": "tool", "tool_call_id": "c1", "content": "body"});
    mark(
        &mut message,
        &layer_call("c1", args.clone()),
        &view_output("body"),
    );
    assert_eq!(message["dial_snapcompact_view"], args);
}

#[test]
fn mark_refuses_failed_forged_and_unrelated_results() {
    let args = layer_args("L1");
    let mut failed = view_output("failed");
    failed.is_error = true;
    let mut forged = ToolOutput::ok("forged".into());
    forged.data = Some(json!({"snapcompact_view": false}));
    let unrelated = ToolOutput::ok("plain".into());
    let mut other_tool = layer_call("c1", args.clone());
    other_tool.name = "read".into();

    let cases = [
        ("failed", layer_call("c1", args.clone()), failed),
        ("forged", layer_call("c1", args.clone()), forged),
        ("unrelated", layer_call("c1", args.clone()), unrelated),
        ("other tool", other_tool, view_output("ok")),
    ];
    for (label, call, output) in cases {
        let mut message = json!({"role": "tool", "tool_call_id": "c1", "content": "body"});
        mark(&mut message, &call, &output);
        assert!(
            message.get("dial_snapcompact_view").is_none(),
            "{label} result must not be tagged replaceable"
        );
    }
}

#[test]
fn projection_keeps_only_the_two_latest_successful_views() {
    let mut messages = vec![
        tagged_tool("c1", "view one", layer_args("L1")),
        tagged_tool("c2", "view two", layer_args("L2")),
        tagged_tool("c3", "view three", layer_args("L3")),
    ];
    project(&mut messages);
    assert!(is_retired(&messages[0]), "oldest view must be retired");
    assert!(!is_retired(&messages[1]));
    assert!(!is_retired(&messages[2]));
}

#[test]
fn failed_view_does_not_consume_an_active_slot() {
    let mut failed = json!({
        "role": "tool",
        "tool_call_id": "e",
        "content": "error",
        "is_error": true,
        "dial_snapcompact_view": layer_args("E"),
    });
    failed["is_error"] = json!(true);
    let mut messages = vec![
        tagged_tool("s1", "first", layer_args("S1")),
        tagged_tool("s2", "second", layer_args("S2")),
        failed,
        tagged_tool("s3", "third", layer_args("S3")),
    ];
    project(&mut messages);
    assert!(is_retired(&messages[0]), "older success must retire");
    assert!(!is_retired(&messages[1]), "second success stays expanded");
    assert!(!is_retired(&messages[3]), "latest success stays expanded");
    assert_eq!(messages[2]["content"], "error", "failed view is untouched");
}

#[test]
fn retired_view_drops_bodies_and_images_but_keeps_call_identity() {
    let mut old = tagged_tool("old-call", "heavy body", layer_args("L1"));
    old["dial_images"] = json!([{"path": "a.png"}]);
    old["dial_provider_content"] = json!([{"type": "image_url"}]);
    let mut messages = vec![
        old,
        tagged_tool("c2", "two", layer_args("L2")),
        tagged_tool("c3", "three", layer_args("L3")),
    ];
    project(&mut messages);
    let retired = &messages[0];
    assert!(is_retired(retired));
    assert!(retired.get("dial_images").is_none());
    assert!(retired.get("dial_provider_content").is_none());
    assert_eq!(retired["role"], "tool");
    assert_eq!(retired["tool_call_id"], "old-call");
    assert_eq!(retired["dial_snapcompact_view"], layer_args("L1"));
}

#[test]
fn latest_views_keep_their_images() {
    let mut latest = tagged_tool("c3", "three", layer_args("L3"));
    latest["dial_images"] = json!([{"path": "b.png"}]);
    let mut messages = vec![
        tagged_tool("c1", "one", layer_args("L1")),
        tagged_tool("c2", "two", layer_args("L2")),
        latest,
    ];
    project(&mut messages);
    assert_eq!(messages[2]["dial_images"], json!([{"path": "b.png"}]));
}

#[test]
fn projection_is_idempotent_and_leaves_untagged_messages_alone() {
    let mut messages = vec![
        json!({"role": "user", "content": "question"}),
        tagged_tool("c1", "one", layer_args("L1")),
        tagged_tool("c2", "two", layer_args("L2")),
        tagged_tool("c3", "three", layer_args("L3")),
    ];
    project(&mut messages);
    let once = messages.clone();
    project(&mut messages);
    assert_eq!(messages, once, "a second projection changes nothing");
    assert_eq!(messages[0], json!({"role": "user", "content": "question"}));
}

#[test]
fn projecting_a_copy_never_mutates_the_original_history() {
    let original = vec![
        tagged_tool("c1", "one", layer_args("L1")),
        tagged_tool("c2", "two", layer_args("L2")),
        tagged_tool("c3", "three", layer_args("L3")),
    ];
    let mut copy = original.clone();
    project(&mut copy);
    assert!(is_retired(&copy[0]));
    assert_eq!(original[0]["content"], "one");
    assert!(!is_retired(&original[0]));
}

#[test]
fn schema_keeps_the_base_function_and_adds_layer_guidance() {
    let base = json!({
        "type": "function",
        "function": {
            "name": "session_search",
            "description": "Search earlier sessions.",
            "parameters": {"type": "object", "properties": {}}
        }
    });
    let schema = schema(base);
    assert_eq!(schema["function"]["name"], "session_search");
    let description = schema["function"]["description"].as_str().expect("text");
    assert!(description.starts_with("Search earlier sessions."));
    assert!(description.contains("view:\"layers\""));
}

#[test]
fn available_is_true_only_for_layered_archives() {
    assert!(available(&layered_history()));
    assert!(!available(&[json!({"role": "user", "content": "plain"})]));
    assert!(!available(&[]));
}

#[test]
fn cancelled_view_call_fails_closed_before_recovering_anything() {
    let cancel = AtomicBool::new(true);
    let config = CompactionConfig::default();
    let output = call(
        &layered_history(),
        &json!({"view": "layers", "query": ""}),
        context(&config, 1_000_000, true),
        &cancel,
    );
    assert!(output.is_error, "cancelled recovery must be an error");
    assert!(
        output.text.contains("layer recovery interrupted"),
        "wrong failure cause: {}",
        output.text
    );
    assert!(
        output.data.is_none(),
        "cancelled recovery must not tag output"
    );
}

#[test]
fn image_view_is_rejected_for_a_text_only_model_without_an_artifact() {
    let cancel = AtomicBool::new(false);
    let config = CompactionConfig::default();
    let args = json!({"view": "layers", "query": "", "format": "image"});
    let output = call(
        &layered_history(),
        &args,
        context(&config, 1_000_000, false),
        &cancel,
    );
    assert!(output.is_error, "text-only model must not receive an image");
    assert!(
        output.text.contains("vision-capable"),
        "wrong failure cause: {}",
        output.text
    );
    let artifact = output.data.as_ref().and_then(|data| data.get("artifact"));
    assert!(artifact.is_none(), "no PNG artifact may be produced");
}

#[test]
fn view_above_the_prompt_budget_is_refused_not_truncated() {
    let cancel = AtomicBool::new(false);
    let config = CompactionConfig::default();
    // A zero window leaves no prompt budget, so the non-empty view must be refused by the budget gate.
    let output = call(
        &layered_history(),
        &json!({"view": "layers", "query": ""}),
        context(&config, 0, true),
        &cancel,
    );
    assert!(output.is_error, "oversized view must be refused");
    assert!(
        output.text.contains("above safe prompt budget"),
        "wrong failure cause: {}",
        output.text
    );
    assert!(output.data.is_none(), "refused view must not be tagged");
}

#[test]
fn successful_text_view_is_marked_for_later_projection() {
    let cancel = AtomicBool::new(false);
    let config = CompactionConfig::default();
    let output = call(
        &layered_history(),
        &json!({"view": "layers", "query": ""}),
        context(&config, 1_000_000, true),
        &cancel,
    );
    assert!(!output.is_error, "{}", output.text);
    assert_eq!(
        output.data.as_ref().expect("data")["snapcompact_view"],
        true
    );
}

#[test]
fn image_view_artifact_is_a_decodable_png_for_a_vision_model() {
    let cancel = AtomicBool::new(false);
    let config = CompactionConfig::default();
    let messages = layered_history();
    let layers = crate::storage::snapcompact::layers(&messages).expect("layers parse");
    let newest = layers.last().expect("layer");
    let output = call(
        &messages,
        &json!({"view": "layers", "layer": newest.id, "query": "", "format": "image"}),
        context(&config, 1_000_000, true),
        &cancel,
    );
    assert!(!output.is_error, "{}", output.text);
    let artifact = output.data.as_ref().expect("data")["artifact"].clone();
    assert_eq!(artifact["media_type"], "image/png");
    let png = STANDARD
        .decode(artifact["data"].as_str().expect("base64"))
        .expect("valid base64");
    assert!(
        png.starts_with(&[0x89, b'P', b'N', b'G']),
        "artifact is a PNG"
    );
    assert!(artifact["width"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn layer_view_recovers_the_selected_layer_content_not_only_the_index() {
    let cancel = AtomicBool::new(false);
    let config = CompactionConfig::default();
    let messages = layered_history();
    let layers = crate::storage::snapcompact::layers(&messages).expect("layers parse");
    let oldest = &layers[0];
    let output = call(
        &messages,
        &json!({"view": "layers", "layer": oldest.id, "query": ""}),
        context(&config, 1_000_000, true),
        &cancel,
    );
    assert!(!output.is_error, "{}", output.text);
    assert!(
        output.text.contains(&oldest.id),
        "selected layer id must be shown"
    );
    assert!(
        output.text.contains("first generation"),
        "selected layer's own source must be recovered: {}",
        output.text
    );
}
