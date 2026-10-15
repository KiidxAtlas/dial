//! Self-hosted SearXNG JSON API. The instance is addressed by `SEARXNG_URL`; its host is still checked by
//! the SSRF guard, so loopback and private addresses are refused.

use super::super::fetch::percent_encode;
use super::json_apis::{optional_array, result, text};
use super::{HttpRequest, Parsed, Query, Recency};
use serde_json::Value;

/// `base` is the validated instance URL without a trailing slash.
pub fn build(query: &Query, base: &str) -> HttpRequest {
    let mut url = format!("{base}/search?q={}&format=json", percent_encode(query.text));
    if let Some(recency) = query.recency {
        // SearXNG has no week window; the nearest coarser one is used.
        url.push_str(match recency {
            Recency::Day => "&time_range=day",
            Recency::Week | Recency::Month => "&time_range=month",
            Recency::Year => "&time_range=year",
        });
    }
    HttpRequest::get(url, Vec::new())
}

/// Results carry `content` (or `snippet`); the first string in `answers` becomes the answer.
pub fn parse(value: &Value) -> Result<Parsed, String> {
    let results = optional_array(value, "/results")
        .iter()
        .filter_map(|item| {
            let snippet = text(item, "content")
                .or_else(|| text(item, "snippet"))
                .unwrap_or_default();
            result(text(item, "title"), text(item, "url"), snippet)
        })
        .collect();
    let answer = optional_array(value, "/answers")
        .iter()
        .find_map(|item| item.as_str().map(str::trim).filter(|s| !s.is_empty()))
        .map(str::to_owned);
    Ok(Parsed { answer, results })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn query(recency: Option<Recency>) -> Query<'static> {
        Query {
            text: "a+b c",
            limit: 4,
            recency,
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
}
