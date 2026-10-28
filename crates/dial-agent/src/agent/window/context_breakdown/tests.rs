use super::*;
use crate::agent::window::context_meter::overhead;
use crate::agent::window::prompt::CONTEXT_HEADING;
use crate::llm::Chat;
use crate::llm::wire::{IMAGE_ESTIMATE_TOKENS, estimate_parts, wire_parts};
use serde_json::json;

fn request(messages: Vec<Value>, tools: Vec<Value>) -> Chat {
    Chat {
        messages,
        tools,
        temperature: 0.0,
        max_tokens: 8192,
        seed: None,
    }
}

fn tokens_of(breakdown: &ContextBreakdown, label: &str) -> Option<u64> {
    breakdown
        .rows()
        .into_iter()
        .find(|row| row.label == label)
        .map(|row| row.tokens)
}

#[test]
fn total_equals_the_wire_estimate_of_the_same_request() {
    let chat = request(
        vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"system","content":format!("{CONTEXT_HEADING}<context file=\"AGENTS.md\">x</context>")}),
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"cmd\":\"ls\"}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","content":"out"}),
            json!({"role":"user","content":"look","dial_images":[{"path":"a.png"}]}),
        ],
        vec![json!({"type":"function","function":{"name":"bash"}})],
    );
    let breakdown = overhead(&chat).breakdown;
    assert_eq!(
        breakdown.total_tokens(),
        estimate_parts(&chat.messages, &chat.tools)
    );
}

#[test]
fn tool_outputs_are_grouped_by_the_tool_that_produced_them() {
    let result = json!({"role":"tool","tool_call_id":"c1","content":"out"});
    let chat = request(
        vec![
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            result.clone(),
        ],
        vec![],
    );
    let breakdown = overhead(&chat).breakdown;
    let expected = wire_parts(&result).portable.div_ceil(4) as u64;
    assert_eq!(tokens_of(&breakdown, "tool output: bash"), Some(expected));
}

#[test]
fn tool_result_without_a_known_call_is_grouped_as_unknown() {
    let chat = request(
        vec![json!({"role":"tool","tool_call_id":"missing","content":"out"})],
        vec![],
    );
    let breakdown = overhead(&chat).breakdown;
    assert!(tokens_of(&breakdown, "tool output: unknown").is_some());
}

#[test]
fn instruction_files_are_separated_from_the_system_prompt() {
    let chat = request(
        vec![
            json!({"role":"system","content":format!("{CONTEXT_HEADING}<context file=\"AGENTS.md\">x</context>")}),
        ],
        vec![],
    );
    let breakdown = overhead(&chat).breakdown;
    assert!(tokens_of(&breakdown, "context files").is_some());
    assert_eq!(tokens_of(&breakdown, "system prompt"), None);
}

#[test]
fn images_cost_the_fixed_image_estimate_not_their_bytes() {
    let chat = request(
        vec![
            json!({"role":"user","content":"look","dial_images":[{"path":"a.png"},{"path":"b.png"}]}),
        ],
        vec![],
    );
    let breakdown = overhead(&chat).breakdown;
    assert_eq!(
        tokens_of(&breakdown, "images (estimated)"),
        Some(2 * IMAGE_ESTIMATE_TOKENS as u64)
    );
}

#[test]
fn provider_native_replay_is_counted_as_opaque_size_only() {
    let message = json!({
        "role":"assistant",
        "content":"a",
        "dial_provider_content":[{"type":"opaque","data":"x".repeat(500)}],
    });
    let chat = request(vec![message.clone()], vec![]);
    let parts = wire_parts(&message);
    assert!(parts.native > parts.portable);
    let breakdown = overhead(&chat).breakdown;
    assert_eq!(
        tokens_of(&breakdown, "provider replay (size only)"),
        Some(parts.native.div_ceil(4) as u64)
    );
    assert_eq!(tokens_of(&breakdown, "assistant messages"), None);
}

#[test]
fn tool_names_are_stripped_of_control_characters() {
    let chat = request(
        vec![
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"ba\u{1b}sh","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","content":"out"}),
        ],
        vec![],
    );
    let breakdown = overhead(&chat).breakdown;
    assert!(tokens_of(&breakdown, "tool output: bash").is_some());
}
