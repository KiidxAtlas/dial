//! Provider-neutral request economy measurements. Bytes are projections, never billed tokens.
use crate::agent::window::context_meter::RequestOverhead;
use crate::{config::types::EfficiencyConfig, tools};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

mod commands;
pub(in crate::agent) use commands::compact_command;

/// Bounded controller-observed call identities, not evidence freshness or model comprehension.
#[derive(Default, Serialize, Deserialize)]
pub(in crate::agent) struct Inspections {
    signatures: BTreeSet<String>,
    pub truncated: bool,
}
impl Inspections {
    const LIMIT: usize = 256;

    fn signature(call: &crate::llm::ToolCall) -> Option<String> {
        if !matches!(call.name.as_str(), "read" | "grep" | "glob" | "find") {
            return None;
        }
        use sha2::Digest;
        let bytes = serde_json::to_vec(&(&call.name, &call.arguments)).ok()?;
        Some(crate::hex(&sha2::Sha256::digest(bytes)))
    }

    pub fn record(&mut self, call: &crate::llm::ToolCall) {
        if let Some(signature) = Self::signature(call) {
            self.insert(signature);
        }
    }

    fn insert(&mut self, signature: String) {
        if self.signatures.len() < Self::LIMIT || self.signatures.contains(&signature) {
            self.signatures.insert(signature);
        } else {
            self.truncated = true;
        }
    }

    pub fn extend(&mut self, other: Self) {
        self.truncated |= other.truncated;
        for signature in other.signatures {
            self.insert(signature);
        }
    }
}

#[derive(Default, Serialize)]
pub(in crate::agent) struct Metrics {
    pub requests: u64,
    pub lightweight_requests: u64,
    pub system_bytes: u64,
    pub history_bytes: u64,
    pub request_tail_bytes: u64,
    pub tool_schema_bytes: u64,
    pub full_result_bytes: u64,
    pub visible_result_bytes: u64,
    pub reduced_results: u64,
    pub failed_tool_results: u64,
    pub delivered_worker_results: u64,
    pub parent_inspections_matching_delivered_workers: u64,
    pub worker_inspection_coverage_truncated: bool,
    pub first_tool_result_ms: Option<u64>,
    pub first_workspace_change_result_ms: Option<u64>,
    #[serde(skip)]
    worker_inspections: Inspections,
    #[serde(skip)]
    delivered_worker_ids: BTreeSet<String>,
}
impl Metrics {
    pub fn tool_result(
        &mut self,
        call: &crate::llm::ToolCall,
        changed: bool,
        elapsed: std::time::Duration,
    ) {
        let ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        self.first_tool_result_ms.get_or_insert(ms);
        if changed {
            self.first_workspace_change_result_ms.get_or_insert(ms);
        }
        self.inspection(call);
    }
    /// Exact call overlap after delivery is observable; whether repetition was necessary is not.
    pub fn inspection(&mut self, call: &crate::llm::ToolCall) {
        if Inspections::signature(call)
            .is_some_and(|signature| self.worker_inspections.signatures.contains(&signature))
        {
            self.parent_inspections_matching_delivered_workers += 1;
        }
    }

    pub fn worker_results(&mut self, value: &Value) {
        if let Some(jobs) = value.as_array().or_else(|| value["jobs"].as_array()) {
            for job in jobs {
                self.worker_results(job);
            }
            return;
        }
        if !matches!(
            value["status"].as_str(),
            Some("done" | "completed" | "failed" | "cancelled" | "incomplete")
        ) {
            return;
        }
        if let Some(id) = value["execution_id"]
            .as_str()
            .or_else(|| value["id"].as_str())
        {
            if self.delivered_worker_ids.contains(id) {
                return;
            }
            if self.delivered_worker_ids.len() >= Inspections::LIMIT {
                self.worker_inspection_coverage_truncated = true;
                return;
            }
            self.delivered_worker_ids.insert(id.to_owned());
        }
        self.delivered_worker_results += 1;
        if let Some(inspections) = value
            .get("inspections")
            .and_then(|value| serde_json::from_value::<Inspections>(value.clone()).ok())
        {
            self.worker_inspections.extend(inspections);
            self.worker_inspection_coverage_truncated |= self.worker_inspections.truncated;
        }
    }
    pub fn request(&mut self, overhead: &RequestOverhead, lightweight: bool) {
        self.requests += 1;
        self.lightweight_requests += u64::from(lightweight);
        self.tool_schema_bytes += overhead.tool_schema_bytes as u64;
        self.system_bytes += overhead.economy_system_bytes as u64;
        self.request_tail_bytes += overhead.request_tail_bytes as u64;
        self.history_bytes += overhead.history_bytes as u64;
    }
    pub fn result(&mut self, full: &str, visible: &str) {
        self.full_result_bytes += full.len() as u64;
        self.visible_result_bytes += visible.len() as u64;
        self.reduced_results += u64::from(visible.len() < full.len());
    }
}

/// Add an ambient block only if it is absent from the current model-visible branch. Looking at
/// live history rather than a session-wide seen set lets compaction or branching re-admit it.
/// Earlier messages are never rewritten, preserving their cacheable prefix and archived evidence.
pub(in crate::agent) fn append_ambient(context: &mut String, block: &str, history: &[Value]) {
    if block.trim().is_empty() || context.contains(block) {
        return;
    }
    let contains = |needle: &str| {
        ambient_contains(context, needle)
            || history.iter().any(|message| {
                message["dial_recall"]
                    .as_str()
                    .is_some_and(|text| ambient_contains(text, needle))
                    || message["content"]
                        .as_str()
                        .is_some_and(|text| ambient_contains(text, needle))
                    || message["content"].as_array().is_some_and(|parts| {
                        parts.iter().any(|part| {
                            part["text"]
                                .as_str()
                                .is_some_and(|text| ambient_contains(text, needle))
                        })
                    })
            })
    };
    if contains(block) {
        return;
    }
    let block = novel_ambient(block, &contains);
    if block.is_empty() {
        return;
    }
    if !context.is_empty() {
        context.push_str("\n\n");
    }
    context.push_str(&block);
}

fn ambient_contains(text: &str, needle: &str) -> bool {
    if text.contains(needle) {
        return true;
    }
    // JSON maps may have different key order after transcript persistence. Compare whole note
    // records, never IDs alone, so corrected content/provenance is admitted again.
    let Ok(record) = serde_json::from_str::<Value>(needle) else {
        return false;
    };
    text.lines().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .is_some_and(|value| {
                value["entries"]
                    .as_array()
                    .is_some_and(|entries| entries.contains(&record))
            })
    })
}

/// Only shorten built-in edits when the complete raw result was retained. The producer's validated
/// projection keeps fresh anchors; UI/raw evidence still contain the diff representation.
pub(in crate::agent) fn edit_result(
    tool: &str,
    output: &crate::runtime::tool_output::ToolOutput,
    retained: bool,
) -> Option<String> {
    if tool != "edit" || !retained {
        return None;
    }
    let message = crate::tools::surface::outcome::message_for(tool, "", output);
    let text = message["content"].as_str()?;
    (text != output.text).then(|| text.to_owned())
}

/// Deduplicate exact records rather than entire changing bundles. Full record bytes (including
/// revisions and source digests) must match; a summary or an ID alone is never sufficient.
fn novel_ambient(block: &str, contains: &dyn Fn(&str) -> bool) -> String {
    if block.starts_with("Task architecture context ")
        && let Some(at) = block.find("Saved codebase understanding ")
    {
        let (architecture, sources) = block.split_at(at);
        let sources = novel_ambient(sources, contains);
        return if contains(architecture.trim_end()) {
            sources
        } else {
            format!("{architecture}{sources}")
        };
    }
    if block.starts_with("Relevant persistent notes ")
        && let Some((header, body)) = block.split_once('\n')
        && let Ok(mut value) = serde_json::from_str::<Value>(body)
        && let Some(entries) = value.get_mut("entries").and_then(Value::as_array_mut)
    {
        entries.retain(|entry| !contains(&entry.to_string()));
        return if entries.is_empty() {
            String::new()
        } else {
            format!("{header}\n{value}")
        };
    }
    if !block.starts_with("Saved codebase understanding ") {
        return block.to_owned();
    }
    let mut removed = false;
    let mut kept_data = false;
    let mut lines = Vec::new();
    for line in block.lines() {
        let record = ["Finding ", "Source ", "Architecture source "]
            .iter()
            .any(|prefix| {
                line.strip_prefix(prefix).is_some_and(|json| {
                    serde_json::from_str::<Value>(json).is_ok_and(|value| value.is_object())
                })
            });
        if record && contains(line) {
            removed = true;
            continue;
        }
        kept_data |= !line.is_empty()
            && !line.starts_with("Saved codebase understanding ")
            && !line.starts_with("Ambient brief:");
        lines.push(line);
    }
    if removed && !kept_data {
        String::new()
    } else if removed {
        format!("{}\n", lines.join("\n"))
    } else {
        block.to_owned()
    }
}

/// Normalize an optional byte cap for composition: zero means no cap.
pub(in crate::agent) fn byte_limit(limit: usize) -> usize {
    if limit == 0 { usize::MAX } else { limit }
}

pub(in crate::agent) fn preview_limit(
    config: &EfficiencyConfig,
    window: u32,
    tool: &str,
    failed: bool,
    retained: bool,
) -> usize {
    let original = tools::preview_limit(window, tool, failed, retained);
    if !config.enabled
        || failed
        || !retained
        || matches!(
            tool,
            "read"
                | "edit"
                | "write"
                | "artifact_read"
                | "edit_preview"
                | "tool_load"
                | "tool_search"
                | "todo"
                | "finish"
                | "codebase_memory"
                | "working_state"
        )
    {
        original
    } else {
        original.min(byte_limit(config.tool_preview_bytes))
    }
}

/// Visible body of one tool result, shared by the parent and child controllers so each clips against
/// the window of the model that reads it. A replacement body (investigation advice or a read pointer)
/// stands for the result and is never clipped itself. Returns the body and whether the complete
/// result had to be dropped to fit `limit`.
pub(in crate::agent) fn visible_result(
    text: &str,
    limit: usize,
    replacement: Option<String>,
) -> (String, bool) {
    match replacement {
        Some(body) => (body, false),
        None => (
            crate::agent::safety::guard::clip_evidence(text, limit),
            text.len() > limit,
        ),
    }
}

/// Appends the complete-evidence link only when the visible body dropped content, or when it is
/// investigation advice that refers to the retained result. An unclipped result carries no link.
pub(in crate::agent) fn evidence_link(body: &mut String, uri: &str, dropped: bool) {
    if dropped || body.starts_with("[Investigation check") {
        body.push_str(&format!(
            "\n[Complete tool evidence: {uri}; retrieve with artifact_read]"
        ));
    }
}

#[cfg(test)]
mod tests;
