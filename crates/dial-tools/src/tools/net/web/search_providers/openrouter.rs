//! OpenRouter chat completions with the web plugin: the model's answer plus the `url_citation`
//! annotations it carries. The plugin takes no date filter, so the window travels in the prompt.

use super::grounded::{add_source, prompt};
use super::json_apis::{optional_array, result, text};
use super::{HttpRequest, Parsed, Query};
use serde_json::{Value, json};

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
/// OpenRouter routes the request to a model that can use the web plugin.
const MODEL: &str = "openrouter/auto";

pub fn build(query: &Query, key: &str) -> HttpRequest {
    HttpRequest::post(
        OPENROUTER_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        json!({
            "model": MODEL,
            "plugins": [{"id": "web", "max_results": query.limit.clamp(1, 20)}],
            "messages": [{"role": "user", "content": prompt(query)}],
        }),
    )
}

/// The answer is the message content, a string or the text parts of a content array. Sources are the
/// distinct `url_citation` annotations, with their cited text as the snippet.
pub fn parse(value: &Value) -> Result<Parsed, String> {
    let message = value
        .pointer("/choices/0/message")
        .ok_or("the OpenRouter response has no choices[0].message")?;
    let answer = match &message["content"] {
        Value::String(body) => body.trim().to_owned(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| text(part, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let mut results = Vec::new();
    for note in optional_array(message, "/annotations") {
        let citation = &note["url_citation"];
        if citation.is_null() {
            continue;
        }
        add_source(
            &mut results,
            result(
                text(citation, "title"),
                text(citation, "url"),
                text(citation, "content").unwrap_or_default(),
            ),
        );
    }
    if answer.is_empty() && results.is_empty() {
        return Err("OpenRouter web search returned no answer or citations".into());
    }
    Ok(Parsed {
        answer: (!answer.is_empty()).then_some(answer),
        results,
    })
}

#[cfg(test)]
mod tests;
