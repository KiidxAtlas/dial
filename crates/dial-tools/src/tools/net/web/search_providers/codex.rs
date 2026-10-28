//! Codex: the ChatGPT subscription's own web search, sent through dial-llm's Responses stream on the
//! active ChatGPT sign-in. It has no API key, so it is named-only (never in the automatic chain), and
//! its reply is read with the same OpenAI Responses parser that the API provider uses.

use super::grounded::prompt;
use super::openai;
use super::{Parsed, Query};
use crate::llm::chatgpt;
use serde_json::{Value, json};
use std::path::Path;

const MODEL: &str = "gpt-5.1-codex-mini";
const TIMEOUT_SECS: u64 = 120;

/// The Responses body the ChatGPT Codex endpoint takes: the question as user input, one built-in web
/// search, and the sources it read. Codex requires instructions and a streamed, unstored turn.
pub fn body(query: &Query) -> Value {
    json!({
        "model": MODEL,
        "instructions": "Answer from web search and cite the sources you used.",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": prompt(query)}],
        }],
        "tools": [{"type": "web_search"}],
        "tool_choice": {"type": "web_search"},
        "include": ["web_search_call.action.sources"],
        "store": false,
        "stream": true,
    })
}

/// Runs the turn on the active ChatGPT account and reads the streamed result.
pub fn search(user_dir: &Path, query: &Query) -> Result<Parsed, String> {
    let reply = chatgpt::grounded_search(user_dir, &body(query), TIMEOUT_SECS)
        .map_err(|error| error.to_string())?;
    read(&reply.text, &reply.provider_content)
}

/// The native output items the stream kept (`dial_response_item` wrappers) go through the OpenAI
/// parser; when it finds no answer text, the streamed visible text is the answer.
pub(super) fn read(text: &str, provider_content: &[Value]) -> Result<Parsed, String> {
    let output: Vec<Value> = provider_content
        .iter()
        .filter(|item| item["type"] == "dial_response_item")
        .map(|item| item["item"].clone())
        .collect();
    let mut parsed = openai::parse(&json!({ "output": output }))?;
    if parsed.answer.is_none() {
        let text = text.trim();
        parsed.answer = (!text.is_empty()).then(|| text.to_owned());
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests;
