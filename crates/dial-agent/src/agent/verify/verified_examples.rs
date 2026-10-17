//! Structure of similar verified successful tasks from this project, as advisory prompt text: a short
//! excerpt of the prior request, the ordered tool calls with argument key names, and the verifier.
//! Old argument values and outputs never enter the new prompt.
use crate::agent::{repair_terms, truncate};
use crate::storage::store::Store;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;

/// Verified live prompts scanned for term overlap; payloads are loaded only for the best `LOADED`.
const SCANNED: usize = 256;
const LOADED: usize = 8;
const MAX_CALLS: usize = 8;
const MAX_EXAMPLES: usize = 2;
const EXCERPT_CHARS: usize = 100;

pub(in crate::agent) fn verified_examples(
    store: &Store,
    project_root: &str,
    query: &str,
    offered: &[String],
) -> Result<Vec<String>> {
    let query_terms = repair_terms(query);
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }
    let offered: HashSet<&str> = offered.iter().map(String::as_str).collect();
    let threshold = if query_terms.len() <= 3 { 1 } else { 2 };
    let mut candidates: Vec<(usize, i64, String, String)> = store
        .recent_verified_live(SCANNED)?
        .into_iter()
        .filter_map(|(trajectory, prompt, source)| {
            let overlap = query_terms.intersection(&repair_terms(&prompt)).count();
            (overlap >= threshold).then_some((overlap, trajectory, prompt, source))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut seen = HashSet::new();
    let mut lines = Vec::new();
    for (_, trajectory, prompt, source) in candidates.into_iter().take(LOADED) {
        // A corrupt or vanished snapshot only loses this advisory example.
        let Ok(Some(payload)) = store.trajectory_messages(trajectory) else {
            continue;
        };
        let record = match payload["examples"].as_array() {
            Some(examples) => match examples.last() {
                Some(record) => record,
                None => continue,
            },
            None => &payload,
        };
        if [&payload, record].iter().any(|value| {
            value
                .get("project_root")
                .is_some_and(|root| root.as_str() != Some(project_root))
        }) {
            continue;
        }
        let Some(messages) = record["messages"].as_array() else {
            continue;
        };
        let calls = tool_sequence(messages, &prompt, &offered);
        if calls.is_empty() || !seen.insert(calls.clone()) {
            continue;
        }
        let excerpt: String = prompt
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .filter(|c| !c.is_control())
            .take(EXCERPT_CHARS)
            .collect();
        let source: String = source
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.'))
            .take(32)
            .collect();
        let source = if source.is_empty() {
            "an external verifier".to_owned()
        } else {
            source
        };
        lines.push(truncate(
            &format!(
                "Verified task \"{excerpt}\" (verified by {source}) used: {}. Re-read and validate current values.",
                calls.join(" -> ")
            ),
            480,
        ));
        if lines.len() == MAX_EXAMPLES {
            break;
        }
    }
    Ok(lines)
}

/// Offered tool calls of the verified turn, in order: those after the user message that carried the
/// trajectory prompt (the last user message when it cannot be located), with argument key names only.
fn tool_sequence(messages: &[Value], prompt: &str, offered: &HashSet<&str>) -> Vec<String> {
    let prompt = prompt.trim();
    let users = || {
        messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| message["role"] == "user")
    };
    let start = users()
        .find(|(_, message)| text(&message["content"]).contains(prompt))
        .or_else(|| users().next())
        .map_or(0, |(index, _)| index + 1);
    messages[start..]
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .filter_map(|call| {
            let name = call.pointer("/function/name")?.as_str()?;
            offered.contains(name).then(|| {
                format!(
                    "{name}({})",
                    argument_keys(&call["function"]["arguments"]).join(", ")
                )
            })
        })
        .take(MAX_CALLS)
        .collect()
}

fn argument_keys(arguments: &Value) -> Vec<String> {
    let parsed = arguments
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let mut keys = parsed
        .as_ref()
        .unwrap_or(arguments)
        .as_object()
        .map(|object| {
            object
                .keys()
                .filter(|key| {
                    key.len() <= 32
                        && key
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    keys.sort();
    keys.truncate(5);
    keys
}

fn text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests;
