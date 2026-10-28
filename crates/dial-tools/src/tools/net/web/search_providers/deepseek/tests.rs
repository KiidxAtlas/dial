use super::super::Recency;
use super::super::domains::DomainFilter;
use super::*;

fn query<'a>(text: &'a str, filter: &'a DomainFilter, recency: Option<Recency>) -> Query<'a> {
    Query {
        text,
        limit: 4,
        recency,
        today: 19_723,
        filter,
    }
}

fn sent_query(request: &HttpRequest) -> String {
    let body = request.body.as_deref().unwrap_or_default();
    let value: Value = serde_json::from_str(body).unwrap();
    value["query"].as_str().unwrap_or_default().to_owned()
}

#[test]
fn filters_become_query_operators() {
    let filter = DomainFilter::new(&["a.com".to_owned()], &["b.org".to_owned()]).unwrap();
    let request = build(&query("rust", &filter, Some(Recency::Week)), "k").unwrap();
    assert_eq!(
        sent_query(&request),
        "rust site:a.com -site:b.org after:2023-12-25"
    );
}

#[test]
fn rejects_a_query_over_the_length_limit() {
    let filter = DomainFilter::default();
    let long = "x".repeat(MAX_QUERY_CHARS + 1);
    assert!(build(&query(&long, &filter, None), "k").is_err());
}

#[test]
fn prefers_summary_over_snippet() {
    let parsed = parse(&serde_json::json!({"results": [
        {"title": "T", "url": "https://a.com", "summary": "S", "snippet": "N"},
    ]}))
    .unwrap();
    assert_eq!(parsed.results[0].snippet, "S");
}
