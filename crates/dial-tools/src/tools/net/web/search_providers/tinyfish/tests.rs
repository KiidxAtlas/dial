use super::super::domains::DomainFilter;
use super::*;
use serde_json::json;

fn query<'a>(filter: &'a DomainFilter, recency: Option<Recency>) -> Query<'a> {
    Query {
        text: "rust async",
        limit: 5,
        recency,
        today: 19_723,
        filter,
    }
}

fn params(request: &HttpRequest) -> Vec<(String, String)> {
    url::Url::parse(&request.url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[test]
fn request_carries_query_count_and_key_header() {
    let filter = DomainFilter::default();
    let request = build(&query(&filter, None), "k1").unwrap();
    let pairs = params(&request);
    assert!(pairs.contains(&("query".into(), "rust async".into())));
    assert!(pairs.contains(&("num_results".into(), "5".into())));
    assert!(!pairs.iter().any(|(k, _)| k == "recency_minutes"));
    assert_eq!(request.headers, vec![("X-API-Key", "k1".to_owned())]);
}

#[test]
fn recency_maps_to_minutes() {
    let filter = DomainFilter::default();
    let request = build(&query(&filter, Some(Recency::Week)), "k").unwrap();
    assert!(params(&request).contains(&("recency_minutes".into(), "10080".into())));
}

#[test]
fn whole_host_filters_are_sent_natively() {
    let filter = DomainFilter::new(
        &["a.com".to_owned()],
        &["b.org".to_owned(), "c.net/x".to_owned()],
    )
    .unwrap();
    let request = build(&query(&filter, None), "k").unwrap();
    let pairs = params(&request);
    assert!(pairs.contains(&("include_domains".into(), "a.com".into())));
    assert!(pairs.contains(&("exclude_domains".into(), "b.org".into())));
}

#[test]
fn parses_results_and_skips_entries_without_http_urls() {
    let parsed = parse(&json!({"results": [
        {"title": "A", "url": "https://a.com/1", "snippet": "s"},
        {"title": "B", "url": "ftp://b.com"},
    ]}))
    .unwrap();
    assert_eq!(parsed.results.len(), 1);
    assert_eq!(parsed.results[0].snippet, "s");
}

#[test]
fn rejects_a_response_without_a_results_array() {
    assert!(parse(&json!({"items": []})).is_err());
}
