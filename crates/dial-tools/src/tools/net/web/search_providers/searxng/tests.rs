use super::*;
use serde_json::json;

fn query(recency: Option<Recency>) -> Query<'static> {
    Query {
        text: "a+b c",
        limit: 4,
        recency,
        filter: &super::super::domains::UNFILTERED,
        today: 19_723,
    }
}

#[test]
fn builds_a_json_search_against_the_instance() {
    let request = build(&query(None), "https://search.example");
    assert_eq!(request.method(), "GET");
    assert_eq!(
        request.url,
        "https://search.example/search?q=a%2Bb%20c&format=json"
    );
    assert!(request.headers.is_empty());
}

#[test]
fn maps_recency_to_searxng_windows() {
    let week = build(&query(Some(Recency::Week)), "https://s.example");
    assert!(week.url.ends_with("&time_range=month"));
    let day = build(&query(Some(Recency::Day)), "https://s.example");
    assert!(day.url.ends_with("&time_range=day"));
    let year = build(&query(Some(Recency::Year)), "https://s.example");
    assert!(year.url.ends_with("&time_range=year"));
}

#[test]
fn parses_results_and_first_answer() {
    let value = json!({
        "answers": ["", "Forty-two"],
        "results": [
            {"title": "A", "url": "https://a.example/", "content": "c"},
            {"title": "B", "url": "https://b.example/", "snippet": "s"},
            {"title": "C", "url": "javascript:alert(1)"}
        ]
    });
    let parsed = parse(&value).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Forty-two"));
    assert_eq!(parsed.results.len(), 2);
    assert_eq!(parsed.results[1].snippet, "s");
    assert!(parse(&json!({})).unwrap().results.is_empty());
}
