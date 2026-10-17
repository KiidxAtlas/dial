//! `/debug`: read-only snapshot of the session, model, usage, and tool facts the debug report shows.
//! Nothing here changes agent or session state.

use std::path::PathBuf;

use super::super::Agent;

/// Facts captured for one `/debug` report.
#[derive(Debug, Clone)]
pub struct DebugSnapshot {
    pub session_id: String,
    pub session_path: Option<PathBuf>,
    pub messages: usize,
    pub branch_leaf: Option<String>,
    pub version_id: i64,
    pub version_kind: String,
    pub version_status: String,
    pub backend: String,
    pub context_window: u32,
    pub context_tokens: u64,
    pub context_estimated: bool,
    pub responses: u64,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub compactions: u64,
    pub turns_completed: u64,
    pub tool_contract_errors: u64,
    pub session_cost: Option<f64>,
    pub plan_mode: bool,
    pub tools_offered: Vec<String>,
    pub tools_available: usize,
}

impl Agent<'_> {
    pub fn debug_snapshot(&self) -> DebugSnapshot {
        let session = self.session();
        let version = self.version();
        let (context_tokens, context_estimated) = self.context_usage();
        let metrics = self.metrics();
        DebugSnapshot {
            session_id: session.id().to_owned(),
            session_path: session.path().map(|path| path.to_path_buf()),
            messages: session.message_count(),
            branch_leaf: session.branch_leaf().map(str::to_owned),
            version_id: version.id,
            version_kind: version.kind.clone(),
            version_status: version.status.clone(),
            backend: self.backend_description(),
            context_window: self.context_window(),
            context_tokens,
            context_estimated,
            responses: metrics.responses,
            prompt_tokens: metrics.prompt_tokens,
            output_tokens: metrics.output_tokens,
            cached_tokens: metrics.cached_tokens,
            cache_write_tokens: metrics.cache_write_tokens,
            reasoning_tokens: metrics.reasoning_tokens,
            compactions: metrics.compactions,
            turns_completed: metrics.turns_completed,
            tool_contract_errors: metrics.tool_contract_errors,
            session_cost: self.session_cost(),
            plan_mode: self.plan_mode(),
            tools_offered: self.tool_names(),
            tools_available: self.available_tool_names().len(),
        }
    }
}
