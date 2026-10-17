//! Perplexity chat completions with web search: the model's answer plus the sources it cites.

use super::json_apis::{optional_array, result, text};
use super::{HttpRequest, Parsed, Query, Recency};
use serde_json::{Value, json};

const PERPLEXITY_URL: &str = "https://api.perplexity.ai/chat/completions";
const MODEL: &str = "sonar-pro";

pub fn build(query: &Query, key: &str) -> HttpRequest {
    let mut body = json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": query.text}],
        "temperature": 0.2,
        "search_mode": "web",
        "num_search_results": query.limit.clamp(1, 20),
        "return_related_questions": false,
    });
    if let Some(recency) = query.recency {
        body["search_recency_filter"] = json!(match recency {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        });
    }
    HttpRequest::post(
        PERPLEXITY_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        body,
    )
}

/// The answer is `choices[0].message.content`. Sources come from `search_results` when present, else
/// from the bare `citations` URL list.
pub fn parse(value: &Value) -> Result<Parsed, String> {
    let answer = text(value, "/choices/0/message/content");
    let mut results: Vec<_> = optional_array(value, "/search_results")
        .iter()
        .filter_map(|item| {
            result(
                text(item, "title"),
                text(item, "url"),
                text(item, "snippet").unwrap_or_default(),
            )
        })
        .collect();
    if results.is_empty() {
        results = optional_array(value, "/citations")
            .iter()
            .filter_map(|item| {
                let url = item.as_str()?.to_owned();
                result(None, Some(url), String::new())
            })
            .collect();
    }
    Ok(Parsed { answer, results })
}

#[cfg(test)]
mod tests;
