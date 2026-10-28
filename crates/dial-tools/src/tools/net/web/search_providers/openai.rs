//! OpenAI with the Responses API `web_search` tool: the model's answer plus the sources it cited. A
//! reply without a `web_search_call` item is not a search result and is refused.

use super::grounded::{add_source, prompt};
use super::json_apis::{optional_array, result, text};
use super::{HttpRequest, Parsed, Query};
use serde_json::{Value, json};

const OPENAI_URL: &str = "https://api.openai.com/v1/responses";
const MODEL: &str = "gpt-4.1-mini";

pub fn build(query: &Query, key: &str) -> HttpRequest {
    HttpRequest::post(
        OPENAI_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        json!({
            "model": MODEL,
            "input": prompt(query),
            "tools": [{"type": "web_search"}],
            "tool_choice": {"type": "web_search"},
            "include": ["web_search_call.action.sources"],
            "store": false,
        }),
    )
}

/// The answer joins every `output_text` part. Sources are the distinct `url_citation` annotations, then
/// the sources the search call read.
pub fn parse(value: &Value) -> Result<Parsed, String> {
    if let Some(message) = text(value, "/error/message") {
        return Err(format!("OpenAI reported an error: {message}"));
    }
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or("the OpenAI response has no output array")?;
    if !output.iter().any(|item| item["type"] == "web_search_call") {
        return Err(
            "OpenAI answered without running a web search, so the reply is not a search result"
                .into(),
        );
    }
    let mut answer = Vec::new();
    let mut results = Vec::new();
    for item in output.iter().filter(|item| item["type"] == "message") {
        for part in optional_array(item, "/content") {
            if let Some(body) = text(part, "text") {
                answer.push(body);
            }
            for note in optional_array(part, "/annotations") {
                if note["type"] == "url_citation" {
                    add_source(
                        &mut results,
                        result(text(note, "title"), text(note, "url"), String::new()),
                    );
                }
            }
        }
    }
    for call in output
        .iter()
        .filter(|item| item["type"] == "web_search_call")
    {
        for source in optional_array(call, "/action/sources") {
            add_source(
                &mut results,
                result(text(source, "title"), text(source, "url"), String::new()),
            );
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
