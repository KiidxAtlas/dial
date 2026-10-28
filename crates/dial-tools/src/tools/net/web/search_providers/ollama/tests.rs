use super::super::domains::DomainFilter;
use super::*;

#[test]
fn clamps_max_results_to_the_api_range() {
    let filter = DomainFilter::default();
    let query = Query {
        text: "q",
        limit: 50,
        recency: None,
        today: 19_723,
        filter: &filter,
    };
    let request = build(&query, "k");
    let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["max_results"], 10);
}

#[test]
fn parses_content_and_tolerates_a_missing_array() {
    let parsed = parse(&serde_json::json!({"results": [
        {"title": "A", "url": "https://a.com", "content": "body"},
    ]}))
    .unwrap();
    assert_eq!(parsed.results[0].snippet, "body");
    assert!(parse(&serde_json::json!({})).unwrap().results.is_empty());
}
