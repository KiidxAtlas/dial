//! One authorization boundary for synchronous and live delegated tool calls.
use super::{Denial, PermissionRule, PolicyReason};
use crate::{agent::Ui, config::types::Approval, llm::ToolCall};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

/// Authority source of a call, kept outside model-controlled tool arguments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::agent) enum CallProvenance {
    #[default]
    Structured,
    RecoveredText,
}

impl CallProvenance {
    pub fn from_reply(reply: &crate::llm::Reply) -> Self {
        if reply.text_calls_lifted {
            Self::RecoveredText
        } else {
            Self::Structured
        }
    }
}

/// Recovered text is never covered by automatic modes, reusable grants or batch tickets.
/// The human decision applies once, even when the UI offers a reusable choice.
pub(in crate::agent) fn authorize_recovered_call(
    policy: &WorkerPolicy<'_>,
    call: &ToolCall,
    summary: &str,
    ui: &mut dyn Ui,
) -> Result<(), Denial> {
    let cancelled = || {
        [policy.cancel, policy.interrupt]
            .into_iter()
            .flatten()
            .any(|flag| flag.load(Ordering::SeqCst))
    };
    if cancelled() {
        super::explain(ui, call, PolicyReason::Cancelled);
        return Err(Denial::User);
    }
    if super::denied_by_rule(policy.rules, call, policy.root) {
        super::explain_rule(ui, call, policy.root, policy.rules, super::Effect::Deny);
        return Err(Denial::Rule);
    }
    if policy.approval == Approval::Readonly {
        super::explain(ui, call, PolicyReason::ReadOnly);
        return Err(Denial::ReadOnly);
    }
    super::explain(ui, call, PolicyReason::PromptRequired);
    let summary = format!(
        "{summary}\nThis call was recovered from model text, not a structured tool call. Approve this action once? No reusable permission will be saved."
    );
    let decision = ui.approve_call(&call.id, &call.name, &summary, policy.cancel);
    let reason = if cancelled() {
        PolicyReason::Cancelled
    } else if decision == crate::agent::Decision::Deny {
        PolicyReason::OperatorDeny
    } else {
        PolicyReason::OperatorAllow
    };
    super::explain(ui, call, reason);
    if matches!(reason, PolicyReason::Cancelled | PolicyReason::OperatorDeny) {
        Err(Denial::User)
    } else {
        Ok(())
    }
}

pub(in crate::agent) struct WorkerPolicy<'a> {
    pub approval: Approval,
    pub rules: &'a mut Vec<PermissionRule>,
    pub root: &'a Path,
    pub read_only: bool,
    pub cancel: Option<&'a AtomicBool>,
    pub interrupt: Option<&'a AtomicBool>,
}

pub(in crate::agent) fn authorize_worker_call(
    policy: WorkerPolicy<'_>,
    call: &ToolCall,
    summary: &str,
    needs_approval: bool,
    provenance: CallProvenance,
    ui: &mut dyn Ui,
) -> Result<(), Denial> {
    let cancelled = || {
        [policy.cancel, policy.interrupt]
            .into_iter()
            .flatten()
            .any(|flag| flag.load(Ordering::SeqCst))
    };
    if cancelled() {
        super::explain(ui, call, PolicyReason::Cancelled);
        return Err(Denial::User);
    }
    let prior_rules = policy.rules.len();
    let result = if super::denied_by_rule(policy.rules, call, policy.root) {
        super::explain_rule(ui, call, policy.root, policy.rules, super::Effect::Deny);
        Err(Denial::Rule)
    } else if needs_approval && policy.read_only {
        super::explain(ui, call, PolicyReason::ReadOnly);
        Err(Denial::ReadOnly)
    } else if provenance == CallProvenance::RecoveredText {
        authorize_recovered_call(&policy, call, summary, ui)
    } else if !needs_approval {
        super::explain(ui, call, PolicyReason::SafeCall);
        Ok(())
    } else {
        // Delegates may add session grants, but never persist project permission rules.
        match policy.cancel {
            Some(cancel) => super::authorize_with_cancel(
                policy.approval,
                policy.rules,
                policy.root,
                call,
                summary,
                ui,
                cancel,
            ),
            None => super::authorize(
                policy.approval,
                policy.rules,
                policy.root,
                call,
                summary,
                ui,
            ),
        }
        .map(|_persist| ())
    };
    if cancelled() {
        policy.rules.truncate(prior_rules);
        if let Some(cancel) = policy.cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        super::explain(ui, call, PolicyReason::Cancelled);
        Err(Denial::User)
    } else {
        result
    }
}

#[cfg(test)]
mod tests;
