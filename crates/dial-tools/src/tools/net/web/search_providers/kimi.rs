//! Kimi search through the Kimi Code search endpoint. It takes no recency parameter, so the recency
//! window is reported as a note rather than enforced.

use super::json_apis::{count, optional_array, result, text};
use super::{HttpRequest, Parsed, Query};
use serde_json::{Value, json};

const KIMI_URL: &str = "https://api.kimi.com/coding/v1/search";
const MAX_RESULTS: usize = 20;

pub fn build(query: &Query, key: &str) -> HttpRequest {
    HttpRequest::post(
        KIMI_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        json!({
            "text_query": query.text,
            "limit": count(query.limit, MAX_RESULTS),
            "enable_page_crawling": false,
            "timeout_seconds": 30,
        }),
    )
}

pub fn parse(value: &Value) -> Result<Parsed, String> {
    let results = optional_array(value, "/search_results")
        .iter()
        .filter_map(|item| {
            let snippet = text(item, "snippet")
                .or_else(|| text(item, "content"))
                .unwrap_or_default();
            result(text(item, "title"), text(item, "url"), snippet)
        })
        .collect();
    Ok(Parsed {
        answer: None,
        results,
    })
}

#[cfg(test)]
mod tests;
