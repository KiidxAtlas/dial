use super::{Env, MutationScope};
use crate::agent::Ui;
use crate::agent::safety::approval;
use crate::agent::safety::approval::Denial;
use crate::agent::safety::approval::PermissionRule;
use crate::config::paths::Layout;
use crate::config::types::Approval;
use crate::llm::ToolCall;
use crate::tools::integrations::ext::Registry;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Asks the user (or applies the mode) for a child's approval-requiring call.
pub(in crate::agent) trait Approver {
    fn tool_signals(
        &mut self,
        _call: &ToolCall,
        _registry: &Registry,
    ) -> Option<crate::tools::exec::signals::CallSignals> {
        None
    }

    fn model_route(&mut self, _route: &Value) {}
    fn progress(&mut self, _steps: u32, _phase: &str, _text: &str) {}
    fn ask_questions(
        &mut self,
        _request: &crate::agent::workflow::questions::QuestionRequest,
        _cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        crate::agent::workflow::questions::QuestionOutcome::Unsupported(
            "This child has no connected owner question input".into(),
        )
    }
    fn memory_capture_allowed(&mut self, _cancel: &Arc<AtomicBool>) -> Result<bool, String> {
        Err("memory capture owner policy is unavailable".into())
    }
    fn authorize(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
    ) -> Result<(), Denial>;
    fn authorize_with_cancel(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        if cancel.load(Ordering::SeqCst) {
            return Err(Denial::User);
        }
        let result = self.authorize(call, summary, needs_approval);
        if cancel.load(Ordering::SeqCst) {
            Err(Denial::User)
        } else {
            result
        }
    }
    /// Calls recovered from text must reach an approval owner, never an automatic fallback.
    fn authorize_from(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
        provenance: approval::CallProvenance,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        if provenance == approval::CallProvenance::RecoveredText {
            return Err(Denial::User);
        }
        self.authorize_with_cancel(call, summary, needs_approval, cancel)
    }
    /// Recheck computed native targets against the controller's current permissions.
    /// The enclosing operation was already approved; this must never widen its grant.
    fn authorize_mutation(
        &mut self,
        _original: &ToolCall,
        _tool: &str,
        _paths: &[PathBuf],
        _unbounded: bool,
        _cancel: &Arc<AtomicBool>,
        _scope: MutationScope,
    ) -> Result<(), String> {
        Err("native mutation authorization is unavailable for this approver".into())
    }
    fn authorize_managed_memory(
        &mut self,
        original: &ToolCall,
        paths: &[PathBuf],
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), String> {
        self.authorize_mutation(
            original,
            &original.name,
            paths,
            false,
            cancel,
            MutationScope::ManagedMemory,
        )
    }
    fn policy_explanation(&mut self, _call: &ToolCall, _explanation: &approval::PolicyExplanation) {
    }
    fn notice(&mut self, _message: &str) {}
    fn add_cost(&mut self, _cost: f64) {}
    fn take_messages(&mut self) -> Vec<String> {
        Vec::new()
    }
    fn live_coordination(&self) -> bool {
        false
    }
    fn peer_message(&mut self, _args: &Value) -> Result<Value, String> {
        Err("peer messaging unavailable in a synchronous task".into())
    }
    fn tool_started(&mut self, _call_id: &str, _name: &str, _registry: &Registry) {}
    fn tool_finished(&mut self, _call_id: &str) {}
    /// Worker-only settings queued by the owner; applied at the next request boundary, never global.
    fn take_worker_settings(
        &mut self,
    ) -> Option<crate::agent::workers::live_tasks::control::WorkerSettings> {
        None
    }
    /// Called after a queued settings update took effect. `resolved` is the worker's effective config,
    /// which owns the model the worker header shows.
    fn worker_settings_applied(
        &mut self,
        _settings: &crate::agent::workers::live_tasks::control::WorkerSettings,
        _resolved: &crate::config::types::Config,
    ) {
    }
    /// Called when a queued settings update could not be applied; the worker keeps its previous backend.
    fn worker_settings_failed(&mut self, _error: &str) {}
    /// Durable transcript for this job, when the runtime records one.
    fn transcript_sink(
        &self,
    ) -> Option<std::sync::Arc<crate::agent::workers::child_transcript::ChildTranscript>> {
        None
    }
}

/// Recheck a retained candidate through the same approval and isolated execution path as workers.
pub(in crate::agent) fn recheck_acceptance(
    env: Env<'_>,
    ids: &[String],
    gate: crate::agent::workers::live_tasks::ApprovalGate<'_>,
    ui: &mut dyn Ui,
) -> crate::agent::verify::worker_acceptance::Acceptance {
    let mut direct = Direct {
        approval: gate.approval,
        always: gate.rules,
        root: gate.root,
        extra_roots: gate.extra_roots,
        layout: gate.layout,
        plan_mode: gate.read_only,
        cost: gate.cost,
        ui,
    };
    crate::agent::verify::worker_acceptance::evaluate(env, ids, &mut direct)
}
/// Approves on the calling thread, straight through the parent's policy and UI.
pub(super) struct Direct<'a> {
    pub(super) approval: Approval,
    pub(super) always: &'a mut Vec<PermissionRule>,
    pub(super) root: &'a Path,
    pub(super) extra_roots: &'a [PathBuf],
    pub(super) layout: Option<&'a Layout>,
    pub(super) plan_mode: bool,
    pub(super) cost: &'a mut Option<f64>,
    pub(super) ui: &'a mut dyn Ui,
}

impl Approver for Direct<'_> {
    fn policy_explanation(&mut self, call: &ToolCall, explanation: &approval::PolicyExplanation) {
        self.ui
            .policy_explanation(&call.id, &call.name, explanation);
    }
    fn notice(&mut self, message: &str) {
        self.ui.notice(message);
    }
    fn ask_questions(
        &mut self,
        request: &crate::agent::workflow::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        self.ui.ask_questions(request, cancel)
    }
    fn memory_capture_allowed(&mut self, cancel: &Arc<AtomicBool>) -> Result<bool, String> {
        if cancel.load(Ordering::SeqCst) {
            return Err("memory capture cancelled".into());
        }
        Ok(!self.plan_mode && self.approval != Approval::Readonly)
    }
    fn add_cost(&mut self, cost: f64) {
        *self.cost = Some(self.cost.unwrap_or(0.0) + cost);
    }
    fn authorize(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
    ) -> Result<(), Denial> {
        approval::authorize_worker_call(
            approval::WorkerPolicy {
                approval: self.approval,
                rules: self.always,
                root: self.root,
                read_only: self.plan_mode,
                cancel: None,
                interrupt: None,
            },
            call,
            summary,
            needs_approval,
            approval::CallProvenance::Structured,
            self.ui,
        )
    }
    fn authorize_with_cancel(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        self.authorize_from(
            call,
            summary,
            needs_approval,
            approval::CallProvenance::Structured,
            cancel,
        )
    }
    fn authorize_from(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
        provenance: approval::CallProvenance,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), Denial> {
        approval::authorize_worker_call(
            approval::WorkerPolicy {
                approval: self.approval,
                rules: self.always,
                root: self.root,
                read_only: self.plan_mode,
                cancel: Some(cancel),
                interrupt: None,
            },
            call,
            summary,
            needs_approval,
            provenance,
            self.ui,
        )
    }
    fn authorize_mutation(
        &mut self,
        original: &ToolCall,
        tool: &str,
        paths: &[PathBuf],
        unbounded: bool,
        cancel: &Arc<AtomicBool>,
        scope: MutationScope,
    ) -> Result<(), String> {
        if cancel.load(Ordering::SeqCst) {
            return Err("native mutation cancelled".into());
        }
        let result = match scope {
            MutationScope::Workspace => approval::authorize_mutation_paths(
                approval::MutationPolicy {
                    rules: self.always,
                    root: self.root,
                    extra_roots: self.extra_roots,
                    read_only: matches!(self.approval, Approval::Readonly),
                },
                original,
                tool,
                paths,
                unbounded,
            ),
            MutationScope::SourceRead => approval::authorize_source_read_paths(
                self.always,
                self.root,
                self.extra_roots,
                original,
                tool,
                paths,
                unbounded,
            ),
            MutationScope::ManagedMemory => {
                if tool != original.name || unbounded {
                    return Err(
                        "managed memory authority does not match its declared operation".into(),
                    );
                }
                let layout = self.layout.ok_or("managed memory requires owning layout")?;
                approval::authorize_managed_memory_paths(
                    self.always,
                    layout,
                    matches!(self.approval, Approval::Readonly),
                    original,
                    paths,
                )
            }
        };
        if cancel.load(Ordering::SeqCst) {
            Err("native mutation cancelled".into())
        } else {
            result
        }
    }
}
