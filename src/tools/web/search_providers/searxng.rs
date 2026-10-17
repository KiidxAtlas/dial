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
mod tests;
