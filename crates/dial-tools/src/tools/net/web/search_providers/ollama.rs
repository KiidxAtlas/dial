//! Ollama hosted web search. The result count is clamped to the API's 1..=10 range; the recency window
//! is not a parameter, so it is reported as a note.

use super::json_apis::{count, optional_array, result, text};
use super::{HttpRequest, Parsed, Query};
use serde_json::{Value, json};

const OLLAMA_URL: &str = "https://ollama.com/api/web_search";
const MAX_RESULTS: usize = 10;

pub fn build(query: &Query, key: &str) -> HttpRequest {
    HttpRequest::post(
        OLLAMA_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        json!({"query": query.text, "max_results": count(query.limit, MAX_RESULTS)}),
    )
}

pub fn parse(value: &Value) -> Result<Parsed, String> {
    let results = optional_array(value, "/results")
        .iter()
        .filter_map(|item| {
            result(
                text(item, "title"),
                text(item, "url"),
                text(item, "content").unwrap_or_default(),
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
