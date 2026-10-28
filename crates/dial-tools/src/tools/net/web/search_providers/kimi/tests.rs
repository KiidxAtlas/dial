use super::super::domains::DomainFilter;
use super::*;

#[test]
fn parses_snippet_or_content_and_skips_non_http_urls() {
    let parsed = parse(&serde_json::json!({"search_results": [
        {"title": "A", "url": "https://a.com", "content": "body"},
        {"title": "B", "url": "javascript:alert(1)", "snippet": "x"},
    ]}))
    .unwrap();
    assert_eq!(parsed.results.len(), 1);
    assert_eq!(parsed.results[0].snippet, "body");
}

#[test]
fn missing_results_array_means_no_results() {
    let parsed = parse(&serde_json::json!({})).unwrap();
    assert!(parsed.results.is_empty());
}

#[test]
fn request_is_a_bearer_post_with_the_query_text() {
    let filter = DomainFilter::default();
    let query = Query {
        text: "kimi",
        limit: 3,
        recency: None,
        today: 19_723,
        filter: &filter,
    };
    let request = build(&query, "secret");
    assert_eq!(
        request.headers,
        vec![("Authorization", "Bearer secret".to_owned())]
    );
    let body: serde_json::Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["text_query"], "kimi");
    assert_eq!(body["limit"], 3);
}
