//! Per-request policy resolution: sampling, output ceiling, single-call mode and lightweight
//! gating resolved in one place with explicit precedence before history projection and dispatch.

use super::*;

/// Effective per-request policy resolved once per step before request construction.
pub(super) struct ResolvedRequestPolicy {
    pub selection: crate::adaptive::Selection,
    pub single_call: bool,
    pub measured_single: bool,
    pub lightweight: bool,
}

impl Agent<'_> {
    /// Resolve adaptive selection, local profile sampling/output overrides, single-call policy
    /// reasons, and lightweight-request eligibility in one deterministic pass.
    pub(super) fn resolve_request_policy(
        &self,
        turn: &TurnState<'_>,
        backend: &Backend,
        step: u32,
        request_control: &mut Value,
    ) -> ResolvedRequestPolicy {
        let pre_action_secs = if step == 0 {
            self.task_policy.initial_reasoning_secs
        } else {
            self.config.reasoning_guard.max_pre_action_secs
        };
        let mut selection = crate::adaptive::select(
            self.effective_config(),
            &self.request_phase,
            crate::adaptive::Request {
                kind: self.task_policy.kind,
                temperature: turn.harness.temperature,
                tokens: turn.reasoning_budget.unwrap_or(turn.max_tokens),
                pre_action_secs,
                base_profile: backend.model_profile(),
                recovering_output: turn.recovering_output || turn.reasoning_budget.is_some(),
                hermetic: self.hermetic,
            },
        );
        if let Some(source) = self.apply_local_sampling(&mut selection.profile) {
            selection.sampling_source = source.to_owned();
            if let Some(profile) = &selection.profile {
                selection.temperature = profile.temperature;
                selection.top_p = profile.top_p;
            }
        }
        // Recovery budgets remain explicit; ordinary unlimited requests inherit the host's limit.
        if !turn.recovering_output
            && turn.reasoning_budget.is_none()
            && !(selection.enabled && self.config.adaptive.output_limits)
        {
            self.apply_local_output_limit(&mut selection.profile, turn.harness.max_tokens);
        }

        let (call_reasons, measured_single) = self.resolve_call_policy_reasons(turn);
        let single_call = !call_reasons.is_empty();
        request_control["reliability"]["call_policy"] = json!({
            "mode": if single_call { "one_call" } else { "batch" },
            "reasons": call_reasons,
        });

        let lightweight = self.config.efficiency.enabled
            && !self.hermetic
            && self.task_policy.lightweight
            && step < self.config.efficiency.lightweight_requests
            && !self.todo.has_open()
            && self.efficiency_metrics.failed_tool_results == 0;

        ResolvedRequestPolicy {
            selection,
            single_call,
            measured_single,
            lightweight,
        }
    }

    fn resolve_call_policy_reasons(&self, turn: &TurnState<'_>) -> (Vec<&'static str>, bool) {
        if self.hermetic {
            return (Vec::new(), false);
        }
        let mut call_reasons = crate::reliability::single_call_reasons(
            self.config,
            turn.measured.as_ref(),
            &turn.reliability,
        );
        let measured_single = !call_reasons.is_empty();
        // Local and measured constraints compose; record every applicable cause.
        if self.local_one_call() {
            call_reasons.push("small_local_tier_without_batch_evidence");
        }
        (call_reasons, measured_single)
    }
}

#[cfg(test)]
mod tests;
