//! Rejected-call repair context and deterministic call-form validation.

use super::*;

pub(super) const MAX_REPAIR_REJECTIONS: usize = 3;
pub(super) const MAX_REPAIR_REJECTION_BYTES: usize = 8 * 1024;

pub(super) struct Pending {
    pub(super) context: Vec<Value>,
    pub(super) kinds: Vec<String>,
    pub(super) rejected: Vec<Value>,
}

impl Pending {
    pub(super) fn new(mut context: Vec<Value>) -> Self {
        for message in &mut context {
            llm::messages::canonicalize(message);
        }
        Self {
            context,
            kinds: Vec::new(),
            rejected: Vec::new(),
        }
    }

    pub(super) fn reject(&mut self, message: Value) {
        if self.rejected.len() < MAX_REPAIR_REJECTIONS
            && serde_json::to_vec(&message)
                .is_ok_and(|bytes| bytes.len() <= MAX_REPAIR_REJECTION_BYTES)
        {
            self.rejected.push(message);
        }
    }
}

pub(super) fn rejected_tool_call_message(call: &ToolCall) -> Value {
    json!({
        "role":"assistant",
        "content":null,
        "tool_calls":[{
            "id":call.id,
            "type":"function",
            "function":{"name":call.name,"arguments":call.arguments.to_string()}
        }]
    })
}
pub(super) fn repair_terms(text: &str) -> HashSet<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "from", "this", "that", "with", "into", "when", "after", "before",
        "how", "can", "please", "help", "make", "tool", "tools", "call", "run", "fix", "use",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| word.chars().count() >= 3 && !STOP.contains(&word.as_str()))
        .take(128)
        .collect()
}

/// Only tiny structural hints from verified repairs are retrieved; old argument values and outputs
/// never enter the new prompt.
pub(super) fn verified_repair_hints(
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
    let mut candidates: Vec<(usize, i64, String, String)> = Vec::new();
    for (trajectory, payload) in store.recent_tool_recoveries(project_root, 8)? {
        if payload["project_root"].as_str() != Some(project_root) {
            continue;
        }
        let Some(context) = payload["context"].as_array() else {
            continue;
        };
        let Some(prompt) = context
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
        else {
            continue;
        };
        let prior_terms = repair_terms(prompt);
        let overlap = query_terms.intersection(&prior_terms).count();
        if overlap < threshold {
            continue;
        }
        let kinds = payload["failure_kinds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|kind| {
                kind.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .take(32)
                    .collect::<String>()
            })
            .filter(|kind| !kind.is_empty())
            .take(3)
            .collect::<Vec<_>>();
        let calls = payload
            .pointer("/corrected/tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for call in calls {
            let Some(name) = call.pointer("/function/name").and_then(Value::as_str) else {
                continue;
            };
            if !offered.contains(name) {
                continue;
            }
            let argument = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            let mut keys = argument
                .as_ref()
                .and_then(Value::as_object)
                .map(|object| {
                    object
                        .keys()
                        .filter(|key| {
                            key.len() <= 32
                                && key.chars().all(|c| {
                                    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
                                })
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            keys.sort();
            keys.truncate(5);
            let signature = format!("{name}:{}:{}", keys.join(","), kinds.join(","));
            let reason = if kinds.is_empty() {
                "a malformed call".to_owned()
            } else {
                kinds.join(", ")
            };
            let fields = if keys.is_empty() {
                "its schema".to_owned()
            } else {
                keys.join(", ")
            };
            let hint = format!(
                "Similar verified repair used `{name}` with fields {fields} after {reason}. Re-read and validate current values."
            );
            candidates.push((overlap, trajectory, signature, truncate(&hint, 240)));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut seen = HashSet::new();
    Ok(candidates
        .into_iter()
        .filter_map(|(_, _, signature, hint)| seen.insert(signature).then_some(hint))
        .take(3)
        .collect())
}

/// Deterministic call-form check against the schema we offered. `None` means the call is well formed
/// (it may still fail at runtime, which is environment feedback rather than a model skill).
pub(super) fn call_form_error(
    call: &ToolCall,
    schemas: &[Value],
    names: &[String],
) -> Option<(&'static str, String)> {
    crate::tools::surface::catalog::validate_offered_call(call, schemas, names)
}
