use super::*;
use std::io::Cursor;

fn event_frame(event: &str, value: Value) -> Vec<u8> {
    let mut header = Vec::new();
    for (name, value) in [(":message-type", "event"), (":event-type", event)] {
        header.push(name.len() as u8);
        header.extend_from_slice(name.as_bytes());
        header.push(7);
        header.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header.extend_from_slice(value.as_bytes());
    }
    let payload = serde_json::to_vec(&value).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&((16 + header.len() + payload.len()) as u32).to_be_bytes());
    bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes());
    bytes
}

#[test]
fn corrupt_or_partial_aws_frames_cannot_be_consumed_as_events() {
    let original = event_frame("messageStop", json!({"stopReason":"end_turn"}));
    for end in 1..original.len() {
        assert!(frame(&mut Cursor::new(&original[..end]), &mut 0).is_err());
    }
    let mut corrupt = original;
    let byte = corrupt.len() - 5;
    corrupt[byte] ^= 1;
    assert!(frame(&mut Cursor::new(corrupt), &mut 0).is_err());
}

#[test]
fn fragmented_tool_input_is_committed_only_after_terminal_message() {
    let mut state = Accumulator::default();
    let mut text: Option<&mut dyn FnMut(&str) -> bool> = None;
    let mut events = |_: &StreamEvent| true;
    for (event, value) in [
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockStart",
            json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"call_1","name":"read"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"path\":"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"\"notes.txt\"}"}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
    ] {
        state.feed(event, value, &mut text, &mut events).unwrap();
    }
    let mut incomplete = Accumulator::default();
    std::mem::swap(&mut state, &mut incomplete);
    assert!(
        incomplete
            .finish("scope")
            .unwrap_err()
            .downcast_ref::<super::super::stream::IncompleteStream>()
            .is_some()
    );
    for (event, value) in [
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockStart",
            json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"call_1","name":"read"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"path\":"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"\"notes.txt\"}"}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
        ("messageStop", json!({"stopReason":"tool_use"})),
        (
            "metadata",
            json!({"usage":{"inputTokens":20,"outputTokens":8,"cacheReadInputTokens":50,"cacheWriteInputTokens":10}}),
        ),
    ] {
        state.feed(event, value, &mut text, &mut events).unwrap();
    }
    let reply = state.finish("scope").unwrap();
    assert_eq!(reply.tool_calls[0].arguments, json!({"path":"notes.txt"}));
    assert!(!reply.tool_calls[0].malformed_arguments);
    assert_eq!(reply.prompt_tokens, Some(80));
    assert_eq!(reply.cached_prompt_tokens, Some(50));
    assert_eq!(reply.cache_creation_tokens, Some(10));
}

#[test]
fn redacted_reasoning_chunks_replay_as_one_binary_value_and_reject_foreign_scope() {
    let mut state = Accumulator::default();
    let mut text: Option<&mut dyn FnMut(&str) -> bool> = None;
    let mut events = |_: &StreamEvent| true;
    for (event, value) in [
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"AQ=="}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"AgM="}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
        ("messageStop", json!({"stopReason":"end_turn"})),
    ] {
        state.feed(event, value, &mut text, &mut events).unwrap();
    }
    let reply = state.finish("account-a").unwrap();
    let chat = Chat {
        messages: vec![
            json!({"role":"assistant","content":"","dial_provider_content":reply.provider_content}),
        ],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 100,
        seed: None,
    };
    let body = request("anthropic.claude-v2", &chat, None, "account-a").unwrap();
    assert_eq!(
        body["messages"][0]["content"][0]["reasoningContent"]["redactedContent"],
        "AQID"
    );
    assert!(request("anthropic.claude-v2", &chat, None, "account-b").is_err());
}

#[test]
fn reasoning_effort_wire_reaches_bedrock_output_config() {
    use crate::llm::profiles::{ModelCapabilities, ModelProfile, ReasoningApi, ReasoningEffort};
    use std::collections::BTreeMap;
    let chat = Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 64,
        seed: None,
    };
    let mut profile = ModelProfile {
        provider: "bedrock".into(),
        model: "anthropic.claude-v2".into(),
        reasoning_effort: Some(ReasoningEffort::High),
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::AnthropicAdaptive,
            reasoning_efforts: vec![ReasoningEffort::High],
            reasoning_effort_wire: BTreeMap::from([(ReasoningEffort::High, "max".to_string())]),
            ..Default::default()
        },
        ..Default::default()
    };
    let effort =
        |body: &Value| body["additionalModelRequestFields"]["output_config"]["effort"].clone();
    let body = request("anthropic.claude-v2", &chat, Some(&profile), "main").unwrap();
    assert_eq!(effort(&body), "max");
    profile.capabilities.reasoning_effort_wire.clear();
    let body = request("anthropic.claude-v2", &chat, Some(&profile), "main").unwrap();
    assert_eq!(effort(&body), "high");
}
