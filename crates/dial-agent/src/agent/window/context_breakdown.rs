//! Per-category breakdown of one model request. It is measured from the same wire parts as
//! `context_meter::overhead`, so the total equals `llm::wire::estimate_parts`. Tokens are estimates
//! (serialized bytes ÷ 4, images at the fixed image cost), never provider-billed usage.
use super::prompt::CONTEXT_HEADING;
use crate::llm::wire::{WireParts, json_bytes};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// Tool outputs whose call id has no preceding assistant tool call are grouped under this label.
const UNKNOWN_TOOL: &str = "unknown";
/// Longest tool name shown; model-supplied names are bounded for display.
const MAX_NAME_CHARS: usize = 64;

/// Byte accounting of one request, grouped by what the context contains.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContextBreakdown {
    system_prompt: usize,
    context_files: usize,
    tool_definitions: usize,
    user: usize,
    assistant: usize,
    tool_outputs: BTreeMap<String, usize>,
    images: usize,
    provider_opaque: usize,
    call_names: HashMap<String, String>,
}

/// One displayed category and its estimated tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRow {
    pub label: String,
    pub tokens: u64,
}

fn tokens(bytes: usize) -> u64 {
    bytes.div_ceil(4) as u64
}

/// Strips control characters from a model-supplied name so it cannot inject terminal sequences.
fn display_name(name: &str) -> String {
    name.chars()
        .filter(|character| !character.is_control())
        .take(MAX_NAME_CHARS)
        .collect()
}

impl ContextBreakdown {
    pub(in crate::agent) fn add_tools(&mut self, tools: &[Value]) {
        self.tool_definitions += tools.iter().map(json_bytes).sum::<usize>();
    }

    /// Adds one message given its wire parts. A provider-native replay that the request sends
    /// instead of the portable copy is counted as opaque; otherwise the portable copy is counted by
    /// role. Images always count, whichever copy is sent.
    pub(in crate::agent) fn add_message(&mut self, message: &Value, parts: WireParts) {
        self.images += parts.images;
        if parts.native > parts.portable {
            self.provider_opaque += parts.native;
            return;
        }
        let bytes = parts.portable;
        let content = message["content"].as_str().unwrap_or_default();
        match message["role"].as_str() {
            Some("system" | "developer") if content.starts_with(CONTEXT_HEADING) => {
                self.context_files += bytes;
            }
            Some("system" | "developer") => self.system_prompt += bytes,
            Some("tool") => {
                let name = message["tool_call_id"]
                    .as_str()
                    .and_then(|id| self.call_names.get(id))
                    .map_or_else(|| UNKNOWN_TOOL.to_owned(), String::clone);
                *self.tool_outputs.entry(name).or_default() += bytes;
            }
            Some("assistant") => {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    if let (Some(id), Some(name)) =
                        (call["id"].as_str(), call["function"]["name"].as_str())
                    {
                        self.call_names.insert(id.to_owned(), display_name(name));
                    }
                }
                self.assistant += bytes;
            }
            _ => self.user += bytes,
        }
    }

    /// Non-empty categories in display order, tool outputs listed by tool name.
    pub fn rows(&self) -> Vec<ContextRow> {
        let mut rows = vec![
            ("system prompt".to_owned(), self.system_prompt),
            ("context files".to_owned(), self.context_files),
            ("tool definitions".to_owned(), self.tool_definitions),
            ("user messages".to_owned(), self.user),
            ("assistant messages".to_owned(), self.assistant),
        ];
        rows.extend(
            self.tool_outputs
                .iter()
                .map(|(name, bytes)| (format!("tool output: {name}"), *bytes)),
        );
        rows.push(("images (estimated)".to_owned(), self.images));
        rows.push((
            "provider replay (size only)".to_owned(),
            self.provider_opaque,
        ));
        rows.into_iter()
            .filter(|(_, bytes)| *bytes > 0)
            .map(|(label, bytes)| ContextRow {
                label,
                tokens: tokens(bytes),
            })
            .collect()
    }

    /// Estimated tokens of the whole request; equals `llm::wire::estimate_parts` for the same input.
    pub fn total_tokens(&self) -> u64 {
        let bytes = self.system_prompt
            + self.context_files
            + self.tool_definitions
            + self.user
            + self.assistant
            + self.tool_outputs.values().sum::<usize>()
            + self.images
            + self.provider_opaque;
        tokens(bytes)
    }
}

#[cfg(test)]
mod tests;
