use super::super::Recency;
use super::super::domains::UNFILTERED;
use super::*;

fn query(recency: Option<Recency>, limit: usize) -> Query<'static> {
    Query {
        text: "latest rust release",
        limit,
        recency,
        filter: &UNFILTERED,
        today: 19_723,
    }
}

#[test]
fn the_prompt_carries_the_window_and_a_clamped_citation_cap() {
    let windowed = prompt(&query(Some(Recency::Week), 5));
    assert!(windowed.contains("latest rust release"));
    assert!(windowed.contains("past week"));
    assert!(windowed.contains("at most 5 sources"));
    assert!(!prompt(&query(None, 5)).contains("past"));
    assert!(prompt(&query(None, 0)).contains("at most 1 sources"));
    assert!(prompt(&query(None, 500)).contains("at most 20 sources"));
}

#[test]
fn only_grounded_key_variables_are_looked_up() {
    assert_eq!(saved_key(Path::new("/nonexistent"), "BRAVE_API_KEY"), None);
}

#[test]
fn add_source_keeps_the_first_of_each_url_and_skips_none() {
    let mut results = Vec::new();
    let found = |url: &str| {
        Some(SearchResult {
            title: url.to_owned(),
            url: url.to_owned(),
            snippet: String::new(),
        })
    };
    add_source(&mut results, found("https://a.example/"));
    add_source(&mut results, found("https://a.example/"));
    add_source(&mut results, None);
    assert_eq!(results.len(), 1);
}
