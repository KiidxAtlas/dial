use super::super::domains::UNFILTERED;
use super::*;

fn query(limit: usize) -> Query<'static> {
    Query {
        text: "latest rust release",
        limit,
        recency: None,
        filter: &UNFILTERED,
        today: 19_723,
    }
}

#[test]
fn builds_a_chat_request_with_the_web_plugin_capped_at_the_limit() {
    let request = build(&query(50), "or-key");
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url, OPENROUTER_URL);
    assert_eq!(
        request.headers,
        vec![("Authorization", "Bearer or-key".to_owned())]
    );
    let sent: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(sent["model"], MODEL);
    assert_eq!(sent["plugins"][0]["id"], "web");
    assert_eq!(sent["plugins"][0]["max_results"], 20);
}

#[test]
fn parses_string_content_and_distinct_url_citations_with_snippets() {
    let value = json!({"choices": [{"message": {
        "content": "Rust 1.80 shipped.",
        "annotations": [
            {"type": "url_citation", "url_citation": {
                "url": "https://blog.rust-lang.org/", "title": "Blog", "content": "1.80 is out"
            }},
            {"type": "url_citation", "url_citation": {"url": "https://blog.rust-lang.org/", "title": "Dup"}},
            {"type": "other"}
        ]
    }}]});
    let parsed = parse(&value).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Rust 1.80 shipped."));
    assert_eq!(parsed.results.len(), 1);
    assert_eq!(parsed.results[0].title, "Blog");
    assert_eq!(parsed.results[0].snippet, "1.80 is out");
}

#[test]
fn joins_array_content_parts_and_refuses_an_empty_reply() {
    let value = json!({"choices": [{"message": {"content": [
        {"type": "text", "text": "One."},
        {"type": "text", "text": "Two."}
    ]}}]});
    assert_eq!(parse(&value).unwrap().answer.as_deref(), Some("One.\nTwo."));
    let empty = json!({"choices": [{"message": {"content": ""}}]});
    assert!(
        parse(&empty)
            .unwrap_err()
            .contains("no answer or citations")
    );
    assert!(parse(&json!({})).is_err());
}
