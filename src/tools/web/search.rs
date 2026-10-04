//! Search backends for `web_search`: parsing result pages into title / URL / snippet.
//!
//! The default backend is DuckDuckGo's keyless HTML endpoint. That is a scraping path: it is not an
//! API, its markup (`result__a`, `result__snippet`) or bot protection can change without notice, and
//! when it does the tool reports a parse failure rather than inventing results. For anything
//! dependable, point `web.search_url` at a search API that returns JSON (SearXNG with
//! `format=json`, Brave, Serper and similar are understood by [`parse_json`]).

use super::fetch::{percent_decode, resolve_url};
use super::html::{self, Token};
use serde_json::Value;

pub const DEFAULT_SEARCH_URL: &str = "https://html.duckduckgo.com/html/?q={query}";

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

enum Part {
    Title,
    Snippet,
}

struct Capture {
    part: Part,
    tag: String,
    depth: usize,
    text: String,
    url: String,
}

/// Parse a DuckDuckGo-style HTML results page. An empty list means the page said there were no
/// results; a page that is neither results nor a "no results" page is an error.
pub fn parse_html(page: &str) -> Result<Vec<SearchResult>, String> {
    let mut results: Vec<SearchResult> = Vec::new();
    let mut capture: Option<Capture> = None;
    for token in html::tokenize(page) {
        let Some(open) = capture.as_mut() else {
            if let Token::Open { name, attrs, self_closing: false } = token {
                let classes = html::attr(&attrs, "class").unwrap_or("");
                let part = if name == "a" && has_class(classes, "result__a") {
                    Part::Title
                } else if has_class(classes, "result__snippet") {
                    Part::Snippet
                } else {
                    continue;
                };
                let url = html::attr(&attrs, "href").map(target_url).unwrap_or_default();
                capture = Some(Capture { part, tag: name, depth: 1, text: String::new(), url });
            }
            continue;
        };
        match token {
            Token::Text(text) => open.text.push_str(&html::decode_entities(&text)),
            Token::Open { name, self_closing: false, .. } if name == open.tag && !html::is_void(&name) => open.depth += 1,
            Token::Close(name) if name == open.tag => {
                open.depth -= 1;
                if open.depth == 0 {
                    if let Some(done) = capture.take() {
                        finish(done, &mut results);
                    }
                }
            }
            _ => {}
        }
    }
    results.retain(|result| !result.title.is_empty() && is_organic(&result.url));
    if !results.is_empty() {
        return Ok(results);
    }
    if page.contains("no-results") || page.contains("No results.") {
        return Ok(results);
    }
    if page.contains("anomaly") {
        return Err("the search backend is asking for a bot check (captcha) and refused the request; try again later or configure web.search_url".into());
    }
    Err("no search results could be found in the response: the backend's page layout may have changed or the request was blocked; configure web.search_url to use a search API".into())
}

fn has_class(classes: &str, wanted: &str) -> bool {
    classes.split_whitespace().any(|class| class == wanted)
}

fn finish(capture: Capture, results: &mut Vec<SearchResult>) {
    let text = capture.text.split_whitespace().collect::<Vec<_>>().join(" ");
    match capture.part {
        Part::Title => results.push(SearchResult { title: text, url: capture.url, snippet: String::new() }),
        Part::Snippet => {
            if let Some(last) = results.last_mut().filter(|last| last.snippet.is_empty()) {
                last.snippet = text;
            }
        }
    }
}

/// The destination of a result link: DuckDuckGo wraps it as `//duckduckgo.com/l/?uddg=<encoded>`.
fn target_url(href: &str) -> String {
    let absolute = resolve_url("https://duckduckgo.com/html/", href).unwrap_or_default();
    match absolute.split_once("uddg=") {
        Some((_, rest)) => percent_decode(rest.split('&').next().unwrap_or("")),
        None => absolute,
    }
}

/// A web page, and not one of the sponsored `duckduckgo.com/y.js` ad redirects.
fn is_organic(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://")) && !url.contains("duckduckgo.com/y.js")
}

/// Parse a JSON search API response. Understands a top-level array or an array under `results`,
/// `web.results`, `organic_results`, `organic`, `items` or `data`, whose entries carry a title
/// (`title`/`name`), a URL (`url`/`link`/`href`) and optionally a snippet
/// (`snippet`/`content`/`description`/`body`/`text`).
pub fn parse_json(value: &Value) -> Result<Vec<SearchResult>, String> {
    let list = [&value["results"], &value["web"]["results"], &value["organic_results"], &value["organic"], &value["items"], &value["data"], value]
        .into_iter()
        .find_map(Value::as_array)
        .ok_or("the search response is JSON but has no recognizable results array (results, web.results, organic_results, organic, items, data)")?;
    let field = |entry: &Value, keys: &[&str]| -> String {
        keys.iter()
            .find_map(|key| entry.get(key).and_then(Value::as_str))
            .map(|text| html::decode_entities(text).split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };
    let results: Vec<SearchResult> = list
        .iter()
        .map(|entry| SearchResult {
            title: field(entry, &["title", "name"]),
            url: field(entry, &["url", "link", "href"]),
            snippet: field(entry, &["snippet", "content", "description", "body", "text"]),
        })
        .filter(|result| !result.title.is_empty() && is_organic(&result.url))
        .collect();
    if results.is_empty() && !list.is_empty() {
        return Err("the search response has results but none with both a title and an http(s) URL".into());
    }
    Ok(results)
}

#[cfg(test)]
/// Result entries copied (whitespace and ranking markup trimmed) from a live html.duckduckgo.com
/// response for `rust language` on 2026-10-01. The sponsored entry follows DuckDuckGo's
/// `result--ad` / `duckduckgo.com/y.js` convention and the snippet-less last entry is hand-made.
pub(super) const DDG_SAMPLE: &str = r##"<!DOCTYPE html><html><head><title>rust language at DuckDuckGo</title></head><body>
<div class="serp__results"><div id="links" class="results">
  <div class="result results_links results_links_deep result--ad">
    <h2 class="result__title"><a rel="nofollow" class="result__a" href="https://duckduckgo.com/y.js?ad_domain=shop.example&amp;u3=1">Buy <b>rust</b> remover</a></h2>
    <a class="result__snippet" href="https://duckduckgo.com/y.js?ad_domain=shop.example">Sponsored copy</a>
  </div>
  <div class="result results_links results_links_deep web-result">
    <div class="links_main links_deep result__body">
      <h2 class="result__title">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust%2Dlang.org%2F&amp;rut=ea146e42cad9126b88c9c73109152586c61c90eef804231c1dc2a83abf8d979e">Rust Programming Language</a>
      </h2>
      <div class="result__extras"><a class="result__url" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust%2Dlang.org%2F&amp;rut=ea146e42cad9126b88c9c73109152586c61c90eef804231c1dc2a83abf8d979e">rust-lang.org</a></div>
      <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust%2Dlang.org%2F&amp;rut=ea146e42cad9126b88c9c73109152586c61c90eef804231c1dc2a83abf8d979e"><b>Rust</b> is a fast, reliable, and productive programming <b>language</b> that can run on embedded devices, web services, and more.</a>
    </div>
  </div>
  <div class="result results_links results_links_deep web-result">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_(programming_language)&amp;rut=9b76da90f36df75906c510a85413eca90afe5752168b471ee27d6fdf82f4b377">Rust (programming language) - Wikipedia</a>
    </h2>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_(programming_language)&amp;rut=9b76da90f36df75906c510a85413eca90afe5752168b471ee27d6fdf82f4b377"><b>Rust</b> (programming <b>language</b>) ... <b>Rust</b> is a general-purpose programming <b>language</b> that emphasizes performance, type safety, and memory safety.</a>
  </div>
  <div class="result results_links results_links_deep web-result">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fgithub.com%2Frust%2Dlang%2Frust&amp;rut=bd141dccca276db6ba5edad1b46c5058fb5cbf725e48f81c2422d05b7672fb79">rust-lang / rust: Empowering everyone to build reliable and ... - GitHub</a>
    </h2>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fgithub.com%2Frust%2Dlang%2Frust&amp;rut=bd141dccca276db6ba5edad1b46c5058fb5cbf725e48f81c2422d05b7672fb79">The <b>Rust</b> Foundation owns the Cargo trademarks (the &quot;<b>Rust</b> Trademarks&quot;). It&#x27;s documented.</a>
  </div>
  <div class="result web-result"><h2 class="result__title"><a class="result__a" href="https://doc.rust-lang.org/book/">The Book</a></h2></div>
</div></div></body></html>"##;

#[cfg(test)]
mod tests {
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
        assert_eq!(parse_html(r#"<div class="no-results">No  results.</div>"#), Ok(vec![]));
        let blocked = parse_html(r#"<div class="anomaly-modal__modal">Unfortunately, bots use DuckDuckGo too.</div>"#).unwrap_err();
        assert!(blocked.contains("bot check"), "{blocked}");
        let unknown = parse_html("<html><body><p>Something else entirely</p></body></html>").unwrap_err();
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
            vec![SearchResult { title: "A".into(), url: "https://a.example/".into(), snippet: "about a".into() }]
        );
        let brave = json!({"web": {"results": [{"title": "C", "url": "https://c.example/", "description": "about c"}]}});
        assert_eq!(parse_json(&brave).unwrap()[0].snippet, "about c");
        let bare = json!([{"name": "D", "link": "http://d.example/"}]);
        assert_eq!(parse_json(&bare).unwrap()[0].url, "http://d.example/");
        assert_eq!(parse_json(&json!({"results": []})), Ok(vec![]));
        assert!(parse_json(&json!({"results": [{"title": "x"}]})).is_err());
        assert!(parse_json(&json!({"error": "bad key"})).is_err());
    }
}
