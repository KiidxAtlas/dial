use super::ToolTraceCounts;
use crate::agent::*;
use serde_json::json;

fn schema(name: &str) -> serde_json::Value {
    json!({"type": "function", "function": {"name": name, "parameters": {}}})
}

#[test]
fn offered_names_count_once_per_request_and_accumulate_across_requests() {
    let mut trace = ToolTraceCounts::default();
    trace.record_offered(&[schema("read"), schema("read"), schema("find")]);
    trace.record_offered(&[schema("read")]);
    assert_eq!(trace.successful_requests, 2);
    assert_eq!(trace.offered.get("read"), Some(&2));
    assert_eq!(trace.offered.get("find"), Some(&1));
}

#[test]
fn malformed_schema_entries_are_ignored_without_failing_the_request() {
    let mut trace = ToolTraceCounts::default();
    trace.record_offered(&[
        json!({"function": {"name": 7}}),
        json!("bare"),
        schema("edit"),
    ]);
    assert_eq!(trace.successful_requests, 1);
    assert_eq!(
        trace.offered.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["edit"]
    );
}

#[test]
fn invoked_counts_are_separate_from_offers_and_per_tool() {
    let mut trace = ToolTraceCounts::default();
    trace.record_offered(&[schema("bash")]);
    trace.record_invoked("bash");
    trace.record_invoked("bash");
    assert_eq!(trace.invoked.get("bash"), Some(&2));
    assert_eq!(trace.offered.get("bash"), Some(&1));
    assert!(!trace.invoked.contains_key("find"));
}

fn sse_reply(text: &str) -> crate::llm::fake::Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

fn sse_tool_call(id: &str, name: &str, arguments: &str) -> crate::llm::fake::Script {
    let event = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments}}]},"finish_reason":"tool_calls"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

/// Runs the given user turns in one hermetic agent and returns its cumulative tool trace plus the
/// number of provider requests the scripted server received.
fn session_trace(
    prompts: &[&str],
    scripts: Vec<crate::llm::fake::Script>,
) -> (ToolTraceCounts, usize) {
    let config = Config::default();
    let workspace = tempfile::tempdir().unwrap();
    let server = crate::llm::fake::serve(scripts);
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    std::fs::write(workspace.path().join("notes.txt"), "alpha\n").unwrap();
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent =
        Agent::hermetic(&config, backend, &version, workspace.path(), None, None).unwrap();
    for prompt in prompts {
        agent.turn(prompt, &mut NullUi).unwrap();
    }
    let trace = agent.metrics().tool_trace.clone();
    drop(agent);
    (trace, server.requests().len())
}

#[test]
fn admitted_execution_is_counted_and_every_successful_reply_offers_the_schemas() {
    let (trace, requests) = session_trace(
        &["hi"],
        vec![
            sse_tool_call("call_read", "read", r#"{"path":"notes.txt"}"#),
            sse_reply("done"),
        ],
    );
    assert_eq!(trace.invoked.get("read"), Some(&1));
    assert_eq!(trace.successful_requests, requests as u64);
    let offered = trace.offered.get("read").copied().unwrap_or(0);
    assert!(
        (1..=requests as u64).contains(&offered),
        "offered on {offered} of {requests} replies"
    );
}

#[test]
fn rejected_calls_are_offered_context_but_never_counted_as_invoked() {
    let (trace, requests) = session_trace(
        &["hi"],
        vec![
            sse_tool_call("call_missing", "tool_that_does_not_exist", "{}"),
            sse_reply("done"),
        ],
    );
    assert!(
        !trace.invoked.contains_key("tool_that_does_not_exist"),
        "a rejected form must not count as an execution"
    );
    assert!(trace.invoked.is_empty());
    assert_eq!(trace.successful_requests, requests as u64);
}

#[test]
fn multi_turn_trace_is_cumulative_across_turns_in_one_runtime() {
    let (trace, requests) = session_trace(
        &["first", "second"],
        vec![
            sse_tool_call("call_read", "read", r#"{"path":"notes.txt"}"#),
            sse_reply("first done"),
            sse_reply("second done"),
        ],
    );
    assert_eq!(requests, 3);
    assert_eq!(trace.successful_requests, 3);
    assert_eq!(trace.invoked.get("read"), Some(&1));
    let offered = trace.offered.get("read").copied().unwrap_or(0);
    assert!(
        (1..=3).contains(&offered),
        "offered on {offered} of 3 replies"
    );
}
