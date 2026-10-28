//! TinyFish search: one GET carrying the query, the result count, and any native domain and recency
//! filters.

use super::json_apis::{array, count, result, text};
use super::{HttpRequest, Parsed, Query, Recency};
use serde_json::Value;

const TINYFISH_URL: &str = "https://api.search.tinyfish.ai";
const MAX_RESULTS: usize = 20;

/// TinyFish takes its recency window in minutes.
fn recency_minutes(recency: Recency) -> u32 {
    match recency {
        Recency::Day => 1_440,
        Recency::Week => 10_080,
        Recency::Month => 43_200,
        Recency::Year => 525_600,
    }
}

pub fn build(query: &Query, key: &str) -> Result<HttpRequest, String> {
    let mut params: Vec<(&str, String)> = vec![
        ("query", query.text.to_owned()),
        ("num_results", count(query.limit, MAX_RESULTS).to_string()),
    ];
    if let Some(recency) = query.recency {
        params.push(("recency_minutes", recency_minutes(recency).to_string()));
    }
    if let Some(hosts) = query.filter.native_include() {
        params.push(("include_domains", hosts.join(",")));
    }
    let excluded = query.filter.native_exclude();
    if !excluded.is_empty() {
        params.push(("exclude_domains", excluded.join(",")));
    }
    let url = url::Url::parse_with_params(TINYFISH_URL, &params)
        .map_err(|error| format!("the TinyFish request URL is invalid: {error}"))?;
    Ok(HttpRequest::get(
        url.to_string(),
        vec![("X-API-Key", key.to_owned())],
    ))
}

pub fn parse(value: &Value) -> Result<Parsed, String> {
    let results = array(value, "/results", "TinyFish")?
        .iter()
        .filter_map(|item| {
            result(
                text(item, "title"),
                text(item, "url"),
                text(item, "snippet").unwrap_or_default(),
            )
        })
        .collect();
    Ok(Parsed {
        answer: None,
        results,
    })
}

#[cfg(test)]
mod tests;
