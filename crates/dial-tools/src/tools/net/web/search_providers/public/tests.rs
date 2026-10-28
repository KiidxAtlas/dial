use super::*;

fn hit(url: &str, snippet: &str) -> SearchResult {
    SearchResult {
        title: url.to_owned(),
        url: url.to_owned(),
        snippet: snippet.to_owned(),
    }
}

#[test]
fn urls_that_differ_only_in_www_or_trailing_slash_are_one_result() {
    let merged = merge(vec![
        vec![hit("https://www.a.com/page/", "")],
        vec![hit("https://a.com/page", "described")],
    ]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].snippet, "described");
}

#[test]
fn results_from_more_engines_rank_first() {
    let merged = merge(vec![
        vec![hit("https://solo.com/", ""), hit("https://shared.com/", "")],
        vec![hit("https://shared.com/", "")],
    ]);
    assert_eq!(merged[0].url, "https://shared.com/");
    assert_eq!(merged[1].url, "https://solo.com/");
}

#[test]
fn a_query_string_distinguishes_pages() {
    let merged = merge(vec![vec![
        hit("https://a.com/p?x=1", ""),
        hit("https://a.com/p?x=2", ""),
    ]]);
    assert_eq!(merged.len(), 2);
}

#[test]
fn duplicates_within_one_engine_count_once() {
    let merged = merge(vec![vec![
        hit("https://a.com/", ""),
        hit("https://a.com/", ""),
    ]]);
    assert_eq!(merged.len(), 1);
}
