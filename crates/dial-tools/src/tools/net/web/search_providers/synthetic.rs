//! Synthetic search. The API takes only the query, so the result count is applied locally and the
//! recency window is reported as a note.

use super::json_apis::{optional_array, result, text};
use super::{HttpRequest, Parsed, Query};
use serde_json::{Value, json};

const SYNTHETIC_URL: &str = "https://api.synthetic.new/v2/search";

pub fn build(query: &Query, key: &str) -> HttpRequest {
    HttpRequest::post(
        SYNTHETIC_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        json!({"query": query.text}),
    )
}

pub fn parse(value: &Value) -> Result<Parsed, String> {
    let results = optional_array(value, "/results")
        .iter()
        .filter_map(|item| {
            result(
                text(item, "title"),
                text(item, "url"),
                text(item, "text").unwrap_or_default(),
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
