//! xAI Grok with its server-side web search tool (Responses API): the model's answer plus the URLs it
//! cited. The API has no recency parameter, so the window is requested in the prompt.

use super::json_apis::{optional_array, result, text};
use super::{HttpRequest, Parsed, Query};
use serde_json::{Value, json};

const XAI_URL: &str = "https://api.x.ai/v1/responses";
const MODEL: &str = "grok-4-fast";

pub fn build(query: &Query, key: &str) -> HttpRequest {
    let window = query
        .recency
        .map(|recency| format!(" Prefer sources from the past {}.", recency.name()))
        .unwrap_or_default();
    HttpRequest::post(
        XAI_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        json!({
            "model": MODEL,
            "input": [{
                "role": "user",
                "content": format!(
                    "Search the web for: {}\nAnswer concisely and cite at most {} sources.{window}",
                    query.text,
                    query.limit.clamp(1, 20)
                ),
            }],
            "tools": [{"type": "web_search"}],
            "store": false,
        }),
    )
}

/// The answer joins every `output_text` part of the message items. Sources are the distinct
/// `url_citation` annotations, else the response's bare `citations` list.
pub fn parse(value: &Value) -> Result<Parsed, String> {
    if let Some(message) = text(value, "/error/message").or_else(|| text(value, "error")) {
        return Err(format!("xAI reported an error: {message}"));
    }
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or("the xAI response has no output array")?;
    let mut answer = Vec::new();
    let mut results: Vec<super::super::search::SearchResult> = Vec::new();
    for item in output.iter().filter(|item| item["type"] == "message") {
        for part in optional_array(item, "/content") {
            if let Some(body) = text(part, "text") {
                answer.push(body);
            }
            for note in optional_array(part, "/annotations") {
                if note["type"] != "url_citation" {
                    continue;
                }
                let found = result(text(note, "title"), text(note, "url"), String::new());
                if let Some(found) = found
                    && results.iter().all(|seen| seen.url != found.url)
                {
                    results.push(found);
                }
            }
        }
    }
    if results.is_empty() {
        for url in optional_array(value, "/citations")
            .iter()
            .filter_map(Value::as_str)
        {
            if let Some(found) = result(None, Some(url.to_owned()), String::new())
                && results.iter().all(|seen| seen.url != found.url)
            {
                results.push(found);
            }
        }
    }
    let answer = answer.join("\n\n");
    Ok(Parsed {
        answer: (!answer.is_empty()).then_some(answer),
        results,
    })
}

#[cfg(test)]
mod tests;
