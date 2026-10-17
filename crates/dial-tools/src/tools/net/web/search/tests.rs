use super::*;
use serde_json::json;

#[test]
fn parses_ddg_results_and_skips_ads() {
    let results = parse_html(DDG_SAMPLE).unwrap();
    assert_eq!(
        results,
        vec![
            SearchResult {
                title: "Rust Programming Language".into(),
                url: "https://rust-lang.org/".into(),
                snippet: "Rust is a fast, reliable, and productive programming language that can run on embedded devices, web services, and more.".into(),
            },
            SearchResult {
                title: "Rust (programming language) - Wikipedia".into(),
                url: "https://en.wikipedia.org/wiki/Rust_(programming_language)".into(),
                snippet: "Rust (programming language) ... Rust is a general-purpose programming language that emphasizes performance, type safety, and memory safety.".into(),
            },
            SearchResult {
                title: "rust-lang / rust: Empowering everyone to build reliable and ... - GitHub".into(),
                url: "https://github.com/rust-lang/rust".into(),
                snippet: "The Rust Foundation owns the Cargo trademarks (the \"Rust Trademarks\"). It's documented.".into(),
            },
            SearchResult { title: "The Book".into(), url: "https://doc.rust-lang.org/book/".into(), snippet: String::new() },
        ]
    );
}

#[test]
fn distinguishes_no_results_from_unrecognized_pages() {
    assert_eq!(
        parse_html(r#"<div class="no-results">No  results.</div>"#),
        Ok(vec![])
    );
    let blocked = parse_html(
        r#"<div class="anomaly-modal__modal">Unfortunately, bots use DuckDuckGo too.</div>"#,
    )
    .unwrap_err();
    assert!(blocked.contains("bot check"), "{blocked}");
    let unknown =
        parse_html("<html><body><p>Something else entirely</p></body></html>").unwrap_err();
    assert!(unknown.contains("layout may have changed"), "{unknown}");
}

#[test]
fn parses_json_apis() {
    let searxng = json!({"results": [
        {"title": "A", "url": "https://a.example/", "content": "about a"},
        {"title": "no url"},
        {"title": "B", "url": "javascript:alert(1)"},
    ]});
    assert_eq!(
        parse_json(&searxng).unwrap(),
        vec![SearchResult {
            title: "A".into(),
            url: "https://a.example/".into(),
            snippet: "about a".into()
        }]
    );
    let brave = json!({"web": {"results": [{"title": "C", "url": "https://c.example/", "description": "about c"}]}});
    assert_eq!(parse_json(&brave).unwrap()[0].snippet, "about c");
    let bare = json!([{"name": "D", "link": "http://d.example/"}]);
    assert_eq!(parse_json(&bare).unwrap()[0].url, "http://d.example/");
    assert_eq!(parse_json(&json!({"results": []})), Ok(vec![]));
    assert!(parse_json(&json!({"results": [{"title": "x"}]})).is_err());
    assert!(parse_json(&json!({"error": "bad key"})).is_err());
}
