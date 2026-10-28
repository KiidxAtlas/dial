use super::*;

#[test]
fn parses_text_field_and_drops_entries_without_http_urls() {
    let parsed = parse(&serde_json::json!({"results": [
        {"title": "A", "url": "https://a.com", "text": "body"},
        {"title": "B", "url": "mailto:x@y.z", "text": "no"},
    ]}))
    .unwrap();
    assert_eq!(parsed.results.len(), 1);
    assert_eq!(parsed.results[0].snippet, "body");
}

#[test]
fn missing_results_array_means_no_results() {
    assert!(parse(&serde_json::json!({})).unwrap().results.is_empty());
}
