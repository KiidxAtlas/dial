//! Provider-neutral request economy measurements. Bytes are projections, never billed tokens.
use super::context_meter::RequestOverhead;
use crate::{config::types::EfficiencyConfig, tools};
use serde::Serialize;

#[derive(Default, Serialize)]
pub(super) struct Metrics {
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
pub(super) fn byte_limit(limit: usize) -> usize {
    if limit == 0 { usize::MAX } else { limit }
}

pub(super) fn preview_limit(
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
pub(super) fn visible_result(
    text: &str,
    limit: usize,
    replacement: Option<String>,
) -> (String, bool) {
    match replacement {
        Some(body) => (body, false),
        None => (super::guard::clip_evidence(text, limit), text.len() > limit),
    }
}

/// Appends the complete-evidence link only when the visible body dropped content, or when it is
/// investigation advice that refers to the retained result. An unclipped result carries no link.
pub(super) fn evidence_link(body: &mut String, uri: &str, dropped: bool) {
    if dropped || body.starts_with("[Investigation check") {
        body.push_str(&format!(
            "\n[Complete tool evidence: {uri}; retrieve with artifact_read]"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn smaller_inspections_preserve_errors_snapshots_and_unretained_evidence() {
        let config = EfficiencyConfig {
            tool_preview_bytes: 4096,
            ..EfficiencyConfig::default()
        };
        assert_eq!(preview_limit(&config, 128000, "grep", false, true), 4096);
        for (tool, failed, retained) in [
            ("grep", true, true),
            ("edit", false, true),
            ("read", false, true),
            ("artifact_read", false, true),
            ("grep", false, false),
        ] {
            assert_eq!(
                preview_limit(&config, 128000, tool, failed, retained),
                tools::preview_limit(128000, tool, failed, retained)
            );
        }
        let disabled = EfficiencyConfig {
            enabled: false,
            ..config
        };
        assert_eq!(
            preview_limit(&disabled, 128000, "grep", false, true),
            tools::preview_limit(128000, "grep", false, true)
        );
    }
    #[test]
    fn zero_preview_cap_does_not_discard_results() {
        let config = EfficiencyConfig {
            tool_preview_bytes: 0,
            ..EfficiencyConfig::default()
        };
        assert_eq!(
            preview_limit(&config, 128000, "grep", false, true),
            tools::preview_limit(128000, "grep", false, true)
        );
        assert_eq!(byte_limit(0).min(byte_limit(4096)), 4096);
        assert_eq!(byte_limit(4096).min(byte_limit(0)), 4096);
    }

    #[test]
    fn a_small_window_child_shapes_results_to_its_own_window_and_links_only_clipped_ones() {
        // A 32k child reads a result shaped for its window, not the 200k parent's cap.
        let child_window = 32 * 1024;
        let parent_limit = tools::model_output_limit(200 * 1024);
        let child_limit = preview_limit(
            &EfficiencyConfig::default(),
            child_window,
            "grep",
            false,
            true,
        );
        assert_eq!(child_limit, 16 * 1024);
        assert!(child_limit < parent_limit);
        let big = "line of evidence\n".repeat(2000);
        let (body, dropped) = visible_result(&big, child_limit, None);
        assert!(dropped && body.len() < big.len());
        let mut clipped = body.clone();
        evidence_link(&mut clipped, "artifact://big", dropped);
        assert!(clipped.contains("[Complete tool evidence: artifact://big"));
        // An unclipped result carries no link.
        let small = "two matches\n";
        let (body, dropped) = visible_result(small, child_limit, None);
        assert!(!dropped);
        let mut unclipped = body.clone();
        evidence_link(&mut unclipped, "artifact://small", dropped);
        assert_eq!(unclipped, small);
    }

    #[test]
    fn replacement_bodies_are_never_clipped_and_pointers_carry_no_link() {
        let text = "evidence ".repeat(10_000);
        let pointer = format!(
            "{} This read of the same range returned identical content.",
            crate::agent::investigation::UNCHANGED_READ
        );
        let (body, dropped) = visible_result(&text, 64, Some(pointer.clone()));
        assert_eq!(body, pointer);
        assert!(!dropped);
        let mut body = body;
        evidence_link(&mut body, "artifact://full", dropped);
        assert_eq!(body, pointer);
    }
}
