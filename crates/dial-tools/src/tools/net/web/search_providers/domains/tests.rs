use super::*;

fn hit(url: &str) -> SearchResult {
    SearchResult {
        title: url.to_owned(),
        url: url.to_owned(),
        snippet: String::new(),
    }
}

fn urls(results: &[SearchResult]) -> Vec<&str> {
    results.iter().map(|result| result.url.as_str()).collect()
}

fn filter(include: &[&str], exclude: &[&str]) -> Result<DomainFilter, String> {
    let include: Vec<String> = include.iter().map(|s| (*s).to_owned()).collect();
    let exclude: Vec<String> = exclude.iter().map(|s| (*s).to_owned()).collect();
    DomainFilter::new(&include, &exclude)
}

#[test]
fn host_and_subdomains_match_but_lookalikes_do_not() {
    let filter = filter(&["example.com"], &[]).unwrap();
    let (kept, note) = filter.apply(vec![
        hit("https://example.com/a"),
        hit("https://docs.example.com/b"),
        hit("https://notexample.com/c"),
    ]);
    assert_eq!(
        urls(&kept),
        ["https://example.com/a", "https://docs.example.com/b"]
    );
    assert!(note.is_none());
}

#[test]
fn path_entry_matches_on_segment_boundaries_only() {
    let filter = filter(&["example.com/docs"], &[]).unwrap();
    let (kept, _) = filter.apply(vec![
        hit("https://example.com/docs/intro"),
        hit("https://example.com/docsfoo"),
        hit("https://example.com/blog"),
    ]);
    assert_eq!(urls(&kept), ["https://example.com/docs/intro"]);
}

#[test]
fn exclusion_removes_matching_results() {
    let filter = filter(&[], &["spam.net"]).unwrap();
    let (kept, note) = filter.apply(vec![
        hit("https://spam.net/x"),
        hit("https://a.spam.net/y"),
        hit("https://ok.org/z"),
    ]);
    assert_eq!(urls(&kept), ["https://ok.org/z"]);
    assert!(note.is_none());
}

#[test]
fn include_that_matches_nothing_keeps_results_with_a_note() {
    let filter = filter(&["example.com"], &[]).unwrap();
    let (kept, note) = filter.apply(vec![hit("https://other.org/a")]);
    assert_eq!(urls(&kept), ["https://other.org/a"]);
    assert!(note.unwrap().contains("example.com"));
}

#[test]
fn exclusion_that_would_remove_everything_is_not_applied() {
    let filter = filter(&[], &["example.com"]).unwrap();
    let (kept, note) = filter.apply(vec![hit("https://example.com/a")]);
    assert_eq!(urls(&kept), ["https://example.com/a"]);
    assert!(note.is_some());
}

#[test]
fn empty_results_produce_no_relaxation_note() {
    let filter = filter(&["example.com"], &["x.com"]).unwrap();
    let (kept, note) = filter.apply(Vec::new());
    assert!(kept.is_empty());
    assert!(note.is_none());
}

#[test]
fn native_include_is_only_offered_for_whole_hosts() {
    let whole = filter(&["a.com", "b.org"], &[]).unwrap();
    assert_eq!(whole.native_include(), Some(vec!["a.com", "b.org"]));
    let with_path = filter(&["a.com", "b.org/docs"], &[]).unwrap();
    assert_eq!(with_path.native_include(), None);
    assert_eq!(filter(&[], &[]).unwrap().native_include(), None);
}

#[test]
fn native_exclude_skips_path_entries() {
    let filter = filter(&[], &["a.com", "b.org/docs"]).unwrap();
    assert_eq!(filter.native_exclude(), vec!["a.com"]);
}

#[test]
fn rejects_urls_ports_and_malformed_entries() {
    for entry in [
        "https://example.com",
        "example.com:8080",
        "",
        "exa mple.com",
        "example.com?q=1",
    ] {
        assert!(filter(&[entry], &[]).is_err(), "accepted {entry:?}");
    }
}

#[test]
fn rejects_more_than_the_entry_limit() {
    let many: Vec<String> = (0..=MAX_DOMAINS).map(|i| format!("site{i}.com")).collect();
    assert!(DomainFilter::new(&many, &[]).is_err());
}
