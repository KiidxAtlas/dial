use super::super::Recency;
use super::super::domains::UNFILTERED;
use super::*;

fn query() -> Query<'static> {
    Query {
        text: "latest rust release",
        limit: 5,
        recency: None,
        filter: &UNFILTERED,
        today: 19_723,
    }
}

#[test]
fn builds_a_messages_request_with_the_versioned_web_search_tool() {
    let request = build(&query(), "sk-ant");
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url, ANTHROPIC_URL);
    assert_eq!(
        request.headers,
        vec![
            ("x-api-key", "sk-ant".to_owned()),
            ("anthropic-version", VERSION.to_owned()),
        ]
    );
    let sent: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(sent["model"], MODEL);
    assert_eq!(sent["max_tokens"], MAX_TOKENS);
    assert_eq!(sent["tools"][0]["type"], "web_search_20250305");
    assert_eq!(sent["tools"][0]["name"], "web_search");
    assert_eq!(sent["messages"][0]["role"], "user");
    assert!(sent.get("temperature").is_none());
}

#[test]
fn joins_text_blocks_with_a_paragraph_break_after_a_tool_block() {
    let value = json!({"content": [
        {"type": "text", "text": "Searching."},
        {"type": "server_tool_use", "name": "web_search", "input": {"query": "x"}},
        {"type": "web_search_tool_result", "content": [
            {"type": "web_search_result", "url": "https://a.example/", "title": "A"}
        ]},
        {"type": "text", "text": "Rust 1.80 shipped.", "citations": [
            {"type": "web_search_result_location", "url": "https://a.example/", "title": "A", "cited_text": "1.80"}
        ]}
    ]});
    let parsed = parse(&value).unwrap();
    assert_eq!(
        parsed.answer.as_deref(),
        Some("Searching.\n\nRust 1.80 shipped.")
    );
    assert_eq!(parsed.results.len(), 1);
    assert_eq!(parsed.results[0].url, "https://a.example/");
    assert_eq!(parsed.results[0].title, "A");
}

#[test]
fn adjacent_text_blocks_stay_one_paragraph() {
    let value = json!({"content": [
        {"type": "text", "text": "Rust "},
        {"type": "text", "text": "1.80."}
    ]});
    let parsed = parse(&value).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Rust 1.80."));
    assert!(parsed.results.is_empty());
}

#[test]
fn rejects_a_reply_without_content() {
    assert!(parse(&json!({"type": "message"})).is_err());
}
