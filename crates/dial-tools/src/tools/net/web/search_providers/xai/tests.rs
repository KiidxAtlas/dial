use super::super::Recency;
use super::*;

fn query(recency: Option<Recency>) -> Query<'static> {
    Query {
        text: "latest rust release",
        limit: 5,
        recency,
        today: 19_723,
    }
}

#[test]
fn builds_a_bearer_responses_request_with_the_web_search_tool() {
    let request = build(&query(Some(Recency::Week)), "xk");
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url, XAI_URL);
    assert_eq!(
        request.headers,
        vec![("Authorization", "Bearer xk".to_owned())]
    );
    let sent: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(sent["model"], MODEL);
    assert_eq!(sent["tools"][0]["type"], "web_search");
    let prompt = sent["input"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("latest rust release") && prompt.contains("past week"));
    let plain: Value =
        serde_json::from_str(build(&query(None), "xk").body.as_deref().unwrap()).unwrap();
    assert!(
        !plain["input"][0]["content"]
            .as_str()
            .unwrap()
            .contains("past")
    );
}

#[test]
fn parses_the_answer_and_distinct_url_citations() {
    let value = json!({"output": [
        {"type": "web_search_call"},
        {"type": "message", "content": [{
            "type": "output_text",
            "text": "Rust 1.80 shipped.",
            "annotations": [
                {"type": "url_citation", "url": "https://blog.rust-lang.org/", "title": "Blog"},
                {"type": "url_citation", "url": "https://blog.rust-lang.org/", "title": "Blog"},
                {"type": "file_citation", "url": "https://ignored.example/"}
            ]
        }]}
    ]});
    let parsed = parse(&value).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Rust 1.80 shipped."));
    assert_eq!(parsed.results.len(), 1);
    assert_eq!(parsed.results[0].title, "Blog");
}

#[test]
fn falls_back_to_bare_citations_and_reports_api_errors() {
    let value = json!({
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "x"}]}],
        "citations": ["https://a.example/", "https://a.example/", "ftp://b.example/"]
    });
    let parsed = parse(&value).unwrap();
    assert_eq!(parsed.results.len(), 1);
    assert_eq!(parsed.results[0].title, "https://a.example/");
    let error = parse(&json!({"error": {"message": "invalid key"}})).unwrap_err();
    assert!(error.contains("invalid key"), "{error}");
    assert!(parse(&json!({})).is_err());
}
