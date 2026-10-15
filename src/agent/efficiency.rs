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
}
