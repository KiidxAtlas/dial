//! Read-only decision support over the ordinary request projection and compaction policies.
use crate::agent::Agent;
use crate::agent::compact::compaction::{self, cut_with_policy, prompt_budget};
use crate::agent::window::context_breakdown::ContextBreakdown;
use crate::config::compaction::{
    CompactionIdentity, CompactionMethod, CompactionOptions, CompactionStrategy,
};
use anyhow::Result;
use serde_json::Value;

#[derive(Debug)]
pub struct ContextDecision {
    pub provider: String,
    pub model: String,
    pub window: u32,
    pub response_reserve: u64,
    pub prompt_budget: u64,
    /// None means automatic compaction is disabled, not that the trigger is zero.
    pub trigger: Option<u64>,
    pub next_tokens: u64,
    pub fixed_tokens: u64,
    /// Difference from an empty-history projection, not a promised saving.
    pub history_tokens: u64,
    pub pending_messages: usize,
    pub last_request: Option<ContextBreakdown>,
    pub approximate_media: bool,
}

#[derive(Debug)]
pub struct CompactionPreflight {
    pub methods: Vec<CompactionMethod>,
    pub eligible_messages: usize,
    pub retained_messages: usize,
    pub keep_recent_tokens: u64,
    pub notes_reset: bool,
    pub summary_provider: String,
    pub summary_model: String,
    pub remote_endpoint: bool,
}

impl Agent<'_> {
    /// `pending` contains prospective user messages in submission order (queue then draft).
    /// This does not admit input, freeze projection memos, run hooks or contact a provider.
    /// Future enrichment, routing, tools and provider tokenization can change the actual request.
    pub fn context_decision(&self, pending: &[Value]) -> Result<ContextDecision> {
        let mut history = self.session.messages();
        history.extend_from_slice(pending);
        let request = self.projected_request(&history, None)?;
        let fixed = self.projected_request(&[], None)?;
        let selection = self.auth_config.selection();
        let policy = &self.config.compaction;
        let identity = CompactionIdentity {
            provider: &selection.provider,
            model: &selection.model,
            agent: None,
        };
        let window = self.budget_window();
        // Do not calibrate against a previous model's billed usage: this estimate belongs to
        // today's selected model and the prospective input, not the last dispatched request.
        let accounting = compaction::request_accounting_with_policy(&request, policy, identity);
        let fixed_tokens = compaction::request_tokens_with_policy(&fixed, policy, identity);
        let budget = prompt_budget(policy, window, request.max_tokens);
        let trigger = if policy.enabled
            && policy.strategy != CompactionStrategy::Off
            && !policy.experimental_context_management
        {
            Some(policy.effective_threshold(identity, window)?)
        } else {
            None
        };
        Ok(ContextDecision {
            provider: selection.provider,
            model: selection.model,
            window,
            response_reserve: u64::from(window).saturating_sub(budget),
            prompt_budget: budget,
            trigger,
            next_tokens: accounting.tokens,
            fixed_tokens,
            history_tokens: accounting.tokens.saturating_sub(fixed_tokens),
            pending_messages: pending.len(),
            last_request: self.last_request_breakdown().cloned(),
            approximate_media: accounting
                .messages
                .iter()
                .any(|parts| parts.images > 0 || parts.native > parts.portable),
        })
    }

    /// Lightweight policy preview only. Execution revalidates the live snapshot and can no-op,
    /// refuse, or choose a fallback; no summary backend is acquired and no history is modified.
    pub fn compaction_preflight(&self, options: &CompactionOptions) -> Result<CompactionPreflight> {
        let policy = &self.config.compaction;
        let selection = self.auth_config.selection();
        let identity = CompactionIdentity {
            provider: &selection.provider,
            model: &selection.model,
            agent: None,
        };
        let request = self.projected_request(&self.session.messages(), None)?;
        let fixed = compaction::replacement_request(&request, &[]);
        let fixed_tokens = compaction::request_tokens_with_policy(&fixed, policy, identity);
        let keep_recent_tokens = match options.keep_recent_tokens {
            Some(tokens) => u64::from(tokens),
            None => policy.keep_recent_budget(identity, self.budget_window(), fixed_tokens)?,
        };
        let cut = cut_with_policy(&request.messages, keep_recent_tokens, policy, identity);
        let count = |messages: &[Value]| {
            messages
                .iter()
                .filter(|m| crate::storage::transcript::is_history(m))
                .count()
        };
        let (eligible_messages, retained_messages) =
            cut.map_or((0, count(&request.messages)), |at| {
                (
                    count(&request.messages[..at]),
                    count(&request.messages[at..]),
                )
            });
        let supported = self
            .backend
            .as_ref()
            .is_some_and(|backend| compaction::native_supported(backend, policy, identity));
        let summary = self
            .role_selection("small")
            .cloned()
            .unwrap_or_else(|| selection.clone());
        let notes_reset = policy.experimental_context_management
            && options.method.is_none()
            && options.strategy.is_none()
            && options.keep_recent_tokens.is_none()
            && options.instructions.is_none()
            && policy.summary_instructions.is_none();
        Ok(CompactionPreflight {
            methods: if notes_reset {
                Vec::new()
            } else {
                policy.available_methods(options, supported)
            },
            eligible_messages,
            retained_messages,
            keep_recent_tokens,
            notes_reset,
            summary_provider: summary.provider,
            summary_model: summary.model,
            remote_endpoint: policy.remote_endpoint.is_some(),
        })
    }
}

#[cfg(test)]
mod tests;
