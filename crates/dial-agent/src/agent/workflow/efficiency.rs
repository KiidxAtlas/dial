//! Provider-neutral request economy measurements. Bytes are projections, never billed tokens.
use crate::agent::window::context_meter::RequestOverhead;
use crate::{config::types::EfficiencyConfig, tools};
use serde::Serialize;

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
}
impl Metrics {
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
