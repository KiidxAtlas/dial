use super::*;
use crate::agent::NullUi;
use crate::agent::run::tool_exposure::default_offer_tests::with_review_disabled;
use crate::agent::window::request_prefix::Tracker;
use crate::llm::{
    Chat,
    fake::{self, Script},
    provider::{Backend, ProviderKind},
};

fn anthropic_sse(events: &[Value]) -> Script {
    Script::Sse(
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect(),
    )
}

/// A signed (display-omitted) thinking block, then the answer.
fn thinking_reply(n: usize) -> Script {
    anthropic_sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":format!("sig-{n}")}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":format!("answer {n}")}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ])
}

/// The history the provider received: wire messages without cache breakpoints, which move
/// between requests and are not part of the conversation; a string content is its text block.
fn wire_history(body: &Value) -> Chat {
    fn unmark(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.remove("cache_control");
                object.values_mut().for_each(unmark);
            }
            Value::Array(items) => items.iter_mut().for_each(unmark),
            _ => {}
        }
    }
    let mut messages = body["messages"].as_array().cloned().unwrap_or_default();
    for message in &mut messages {
        if let Some(text) = message["content"].as_str().map(str::to_owned) {
            message["content"] = json!([{"type":"text","text":text}]);
        }
        unmark(message);
    }
    Chat {
        messages,
        tools: vec![],
        temperature: 0.0,
        max_tokens: 0,
        seed: None,
    }
}

fn texts(body: &Value, needle: &str) -> usize {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message.to_string().contains(needle))
        .count()
}

/// Three turns on a preserved-thinking route, each request carrying the session-mode notice and
/// a one-shot background notice after its history.
fn run(model: &str) -> Vec<Value> {
    let server = fake::serve((0..3).map(thinking_reply).collect());
    with_review_disabled("", |agent| {
        agent.auth_config.provider = Some("anthropic".into());
        agent.auth_config.model = model.into();
        agent.backend = Some(Backend::new(
            ProviderKind::AnthropicEndpoint {
                base_url: server.url.clone(),
                model: model.into(),
                api_key: Some("anth-secret".into()),
            },
            std::path::PathBuf::new(),
            5,
        ));
        agent.plan_mode = true;
        for turn in 0..3 {
            agent
                .background_notices
                .push(format!("background notice {turn}"));
            agent
                .turn(&format!("question {turn}"), &mut NullUi)
                .unwrap();
        }
    });
    server
        .requests()
        .iter()
        .map(|raw| fake::body(raw))
        .collect()
}

#[test]
fn preserved_thinking_requests_only_append_to_the_history_already_sent() {
    let bodies = run("claude-opus-5-5");
    assert_eq!(bodies.len(), 3, "{bodies:#?}");
    let mut tracker = Tracker::default();
    for body in &bodies {
        tracker.assert_appends_only("claude-opus-5-5", &wire_history(body));
    }
    let last = bodies.last().unwrap();
    // Every one-shot notice stays where it was first sent, once.
    for turn in 0..3 {
        assert_eq!(
            texts(last, &format!("background notice {turn}")),
            1,
            "{last:#}"
        );
    }
    // The unchanged mode notice is sent once, not restated on every request.
    assert_eq!(texts(last, "Plan mode is on"), 1, "{last:#}");
}

#[test]
fn other_routes_keep_request_notes_transient() {
    let bodies = run("claude-opus-5");
    // Without the record, the second request already drops the first one's notes.
    let mut tracker = Tracker::default();
    tracker.observe("claude-opus-5", &wire_history(&bodies[0]));
    assert!(
        !tracker
            .observe("claude-opus-5", &wire_history(&bodies[1]))
            .appends_only()
    );
    let last = bodies.last().unwrap();
    assert_eq!(texts(last, "background notice 0"), 0, "{last:#}");
    assert_eq!(texts(last, "Plan mode is on"), 1, "{last:#}");
}

#[test]
fn notes_are_reinserted_after_the_history_they_followed_and_repeats_are_dropped() {
    let history = vec![
        json!({"role":"system","content":"rules"}),
        json!({"role":"user","content":"task"}),
        json!({"role":"assistant","content":"a1"}),
    ];
    let mode =
        json!({"role":"user","content":"mode","dial_transient":true,"dial_mode_notice":true});
    let epoch = epoch(&history).unwrap();
    let records = vec![
        (
            1,
            json!({"epoch":epoch,"notes":[resend(&mode), resend(&json!({"role":"user","content":"once","dial_transient":true}))]}),
        ),
        (
            2,
            json!({"epoch":"other","notes":[{"role":"user","content":"stale","dial_sent_note":"user"}]}),
        ),
    ];
    let mut messages = history.clone();
    messages.push(mode.clone());
    reinsert(&mut messages, &records, 2);
    assert_eq!(messages[2]["content"], "mode");
    assert_eq!(messages[3]["content"], "once");
    assert_eq!(messages[4]["content"], "a1");
    assert!(!messages.iter().any(|m| m["content"] == "stale"));
    drop_repeated(&mut messages);
    // The trailing identical mode notice is dropped; a changed one would be sent.
    assert_eq!(messages.len(), 5);
    let mut changed = messages.clone();
    changed.push(
        json!({"role":"user","content":"mode 2","dial_transient":true,"dial_mode_notice":true}),
    );
    drop_repeated(&mut changed);
    assert_eq!(changed.len(), 6);
    // Another history (a preview of a compacted one) receives nothing.
    let mut other = vec![
        json!({"role":"user","content":"summary"}),
        json!({"role":"assistant","content":"x"}),
    ];
    reinsert(&mut other, &records, 2);
    assert_eq!(other.len(), 2);
}

/// A signed thinking block, then a read-only tool call.
fn thinking_tool_call(n: usize) -> Script {
    anthropic_sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":format!("sig-call-{n}")}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":format!("call-{n}"),"name":"glob","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\"*.md\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ])
}

#[test]
fn a_tool_loop_keeps_each_request_an_extension_of_the_previous_one() {
    let model = "claude-sonnet-5-5";
    let server = fake::serve(vec![
        thinking_tool_call(0),
        thinking_tool_call(1),
        thinking_reply(2),
    ]);
    with_review_disabled("", |agent| {
        agent.auth_config.provider = Some("anthropic".into());
        agent.auth_config.model = model.into();
        agent.backend = Some(Backend::new(
            ProviderKind::AnthropicEndpoint {
                base_url: server.url.clone(),
                model: model.into(),
                api_key: Some("anth-secret".into()),
            },
            std::path::PathBuf::new(),
            5,
        ));
        agent.plan_mode = true;
        agent.background_notices.push("background notice".into());
        agent.turn("list the docs", &mut NullUi).unwrap();
    });
    let bodies: Vec<Value> = server
        .requests()
        .iter()
        .map(|raw| fake::body(raw))
        .collect();
    assert_eq!(bodies.len(), 3);
    let mut tracker = Tracker::default();
    for body in &bodies {
        tracker.assert_appends_only(model, &wire_history(body));
    }
    let last = bodies.last().unwrap();
    assert!(last.to_string().contains("sig-call-0"), "{last:#}");
    assert_eq!(texts(last, "background notice"), 1, "{last:#}");
}
