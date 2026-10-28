use super::super::Recency;
use super::super::domains::UNFILTERED;
use super::*;

fn query() -> Query<'static> {
    Query {
        text: "latest rust release",
        limit: 5,
        recency: Some(Recency::Month),
        filter: &UNFILTERED,
        today: 19_723,
    }
}

#[test]
fn builds_a_bearer_responses_request_forcing_the_web_search_tool() {
    let request = build(&query(), "sk-test");
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url, OPENAI_URL);
    assert_eq!(
        request.headers,
        vec![("Authorization", "Bearer sk-test".to_owned())]
    );
    let sent: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(sent["model"], MODEL);
    assert_eq!(sent["tools"][0]["type"], "web_search");
    assert_eq!(sent["tool_choice"]["type"], "web_search");
    assert_eq!(sent["include"][0], "web_search_call.action.sources");
    assert_eq!(sent["store"], false);
    assert!(sent["input"].as_str().unwrap().contains("past month"));
}

#[test]
fn parses_the_answer_citations_and_the_searched_sources() {
    let value = json!({"output": [
        {"type": "web_search_call", "action": {"sources": [
            {"url": "https://blog.rust-lang.org/", "title": "Blog"},
            {"url": "https://read-only.example/"}
        ]}},
        {"type": "message", "content": [{
            "type": "output_text",
            "text": "Rust 1.80 shipped.",
            "annotations": [
                {"type": "url_citation", "url": "https://blog.rust-lang.org/", "title": "Blog"},
                {"type": "file_citation", "url": "https://ignored.example/"}
            ]
        }]}
    ]});
    let parsed = parse(&value).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Rust 1.80 shipped."));
    let urls: Vec<&str> = parsed.results.iter().map(|r| r.url.as_str()).collect();
    assert_eq!(
        urls,
        vec!["https://blog.rust-lang.org/", "https://read-only.example/"]
    );
    assert_eq!(parsed.results[1].title, "https://read-only.example/");
}

#[test]
fn refuses_a_reply_that_did_not_run_a_search() {
    let value = json!({"output": [
        {"type": "message", "content": [{"type": "output_text", "text": "from memory"}]}
    ]});
    let error = parse(&value).unwrap_err();
    assert!(error.contains("without running a web search"), "{error}");
}

#[test]
fn reports_an_error_object_and_rejects_a_missing_output_array() {
    let error = parse(&json!({"error": {"message": "bad key"}})).unwrap_err();
    assert!(error.contains("bad key"), "{error}");
    assert!(parse(&json!({})).is_err());
}
