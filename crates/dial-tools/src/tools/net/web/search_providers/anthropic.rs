//! Anthropic Messages with the server-side `web_search` tool: the model's answer, the sources it cited
//! and the results its searches read. The tool takes no date filter, so the window travels in the prompt.

use super::grounded::{add_source, prompt};
use super::json_apis::{optional_array, result, text};
use super::{HttpRequest, Parsed, Query};
use serde_json::{Value, json};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const VERSION: &str = "2023-06-01";
const MODEL: &str = "claude-haiku-4-5";
const MAX_TOKENS: u32 = 4096;
const MAX_USES: u32 = 5;

pub fn build(query: &Query, key: &str) -> HttpRequest {
    HttpRequest::post(
        ANTHROPIC_URL,
        vec![
            ("x-api-key", key.to_owned()),
            ("anthropic-version", VERSION.to_owned()),
        ],
        json!({
            "model": MODEL,
            "max_tokens": MAX_TOKENS,
            "messages": [{"role": "user", "content": prompt(query)}],
            "tools": [{
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": MAX_USES,
            }],
        }),
    )
}

/// Text blocks join into the answer, with a paragraph break where text resumes after a tool block. Split
/// citation blocks keep their own spacing. Sources are the results of the search blocks, then the
/// citations on the text.
pub fn parse(value: &Value) -> Result<Parsed, String> {
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or("the Anthropic response has no content array")?;
    let mut answer = String::new();
    let mut previous_was_text = false;
    let mut results = Vec::new();
    for block in blocks {
        if block["type"] != "text" {
            previous_was_text = false;
        }
        match block["type"].as_str() {
            Some("text") => {
                if let Some(body) = block["text"]
                    .as_str()
                    .filter(|body| !body.trim().is_empty())
                {
                    if !answer.is_empty() && !previous_was_text {
                        answer.push_str("\n\n");
                    }
                    answer.push_str(body);
                    previous_was_text = true;
                }
                for citation in optional_array(block, "/citations") {
                    add_source(
                        &mut results,
                        result(
                            text(citation, "title"),
                            text(citation, "url"),
                            String::new(),
                        ),
                    );
                }
            }
            Some("web_search_tool_result") => {
                for found in optional_array(block, "/content") {
                    if found["type"] == "web_search_result" {
                        add_source(
                            &mut results,
                            result(text(found, "title"), text(found, "url"), String::new()),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    let answer = answer.trim().to_owned();
    Ok(Parsed {
        answer: (!answer.is_empty()).then_some(answer),
        results,
    })
}

#[cfg(test)]
mod tests;
