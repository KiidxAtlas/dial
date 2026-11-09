//! One reply's aggregate inline-result budget (`efficiency.batch_result_percent`), shared by the
//! parent's `publish` stage and worker controllers so both clip a wide batch the same way.
use crate::llm::ToolCall;
use crate::runtime::tool_output::ToolOutput;

/// Rough bytes per token for budgeting inline text; the window meter owns real counts.
const BYTES_PER_TOKEN: u64 = 4;
/// A clipped result still shows at least this much before its evidence link.
pub(in crate::agent) const MIN_RESULT_SHARE: usize = 4096;

/// Each result is already capped on its own, but a wide batch (32 parallel reads) could still fill
/// the window; the budget is water-filled across the results known at publication, so the largest
/// are clipped first. Only results whose full text is retained as an artifact are clipped; errors
/// and mutation receipts are exempt.
pub(in crate::agent) struct ResultBudget {
    remaining: usize,
}

impl ResultBudget {
    /// `percent` of the free window (`window - used` tokens); 0 disables the budget.
    pub(in crate::agent) fn new(percent: u32, window: u32, used: u64) -> Self {
        let free = u64::from(window).saturating_sub(used);
        let bytes = free.saturating_mul(BYTES_PER_TOKEN) * u64::from(percent) / 100;
        Self {
            remaining: if percent == 0 {
                usize::MAX
            } else {
                usize::try_from(bytes).unwrap_or(usize::MAX)
            },
        }
    }

    /// The configured share, or 0 (off) when the context economy is disabled.
    pub(in crate::agent) fn percent(config: &crate::config::types::EfficiencyConfig) -> u32 {
        if config.enabled {
            config.batch_result_percent
        } else {
            0
        }
    }

    /// The cap for a result of `current` bytes when `pending` eligible results of the same reply
    /// are already known: the water level at which all of them fit the remaining budget.
    pub(in crate::agent) fn cap(&self, current: usize, pending: &[usize]) -> usize {
        let mut sizes: Vec<usize> = pending.iter().copied().chain([current]).collect();
        if sizes
            .iter()
            .fold(0usize, |sum, size| sum.saturating_add(*size))
            <= self.remaining
        {
            return usize::MAX;
        }
        sizes.sort_unstable();
        let mut budget = self.remaining;
        let mut left = sizes.len();
        for size in sizes {
            if size.saturating_mul(left) <= budget {
                budget -= size;
                left -= 1;
            } else {
                return (budget / left).max(MIN_RESULT_SHARE);
            }
        }
        usize::MAX
    }

    /// The visible limit for one result: `limit` (the per-result preview cap), lowered to the
    /// reply's water level when the result is eligible for clipping.
    pub(in crate::agent) fn limit(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        limit: usize,
        retained: bool,
        duplicate: bool,
        pending: &[usize],
    ) -> usize {
        if retained && !duplicate && !budget_exempt(call, output) {
            limit.min(self.cap(output.text.len().min(limit), pending))
        } else {
            limit
        }
    }

    pub(in crate::agent) fn spend(&mut self, bytes: usize) {
        self.remaining = self.remaining.saturating_sub(bytes);
    }
}

/// Eligible sizes of the results a group already produced for the calls after the current one,
/// each bounded by the per-result cap.
pub(in crate::agent) fn pending_sizes<'a>(
    later: &[ToolCall],
    produced: impl IntoIterator<Item = &'a ToolOutput>,
    result_cap: usize,
) -> Vec<usize> {
    later
        .iter()
        .zip(produced)
        .filter(|(call, output)| !budget_exempt(call, output))
        .map(|(_, output)| output.text.len().min(result_cap))
        .collect()
}

/// Errors, mutation receipts and controller bookkeeping are never clipped by the batch budget.
pub(in crate::agent) fn budget_exempt(call: &ToolCall, output: &ToolOutput) -> bool {
    output.is_error
        || !output.execution.changed_paths.is_empty()
        || matches!(
            call.name.as_str(),
            "edit"
                | "write"
                | "ast_edit"
                | "edit_preview"
                | "revert_edit"
                | "todo"
                | "finish"
                | "tool_load"
                | "tool_search"
                | "ask"
        )
}

#[cfg(test)]
mod tests;
