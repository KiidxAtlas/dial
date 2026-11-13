use super::{Approver, MutationScope};
use crate::agent::safety::approval;
use crate::agent::safety::approval::Denial;
use crate::llm::ToolCall;
use crate::tools::integrations::ext::Registry;
use serde_json::Value;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Rewrites paths inside the isolated copy to the project's, so permission rules (which are about
/// project paths) judge what the worker really targets even when it uses absolute paths.
pub(in crate::agent) struct Rebase<'a> {
    pub inner: &'a mut dyn Approver,
    pub from: &'a Path,
    pub to: &'a Path,
    pub descendant: Option<&'a str>,
}

/// Rewrites `from` to `to` only where `from` is a whole path: a match may not continue into a longer
/// name (`/tmp/ws` must not rewrite `/tmp/ws2/x`) nor start inside one (`/x/tmp/ws`). Returns `None`
/// when nothing changed.
pub(in crate::agent) fn rebase_text(text: &str, from: &str, to: &str) -> Option<String> {
    if from.is_empty() {
        return None;
    }
    let name_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.');
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, _) in text.match_indices(from) {
        let end = start + from.len();
        let starts_name = text[..start].chars().next_back().is_some_and(name_char);
        let continues_name =
            !from.ends_with(['/', '\\']) && text[end..].chars().next().is_some_and(name_char);
        if starts_name || continues_name {
            continue;
        }
        out.push_str(&text[last..start]);
        out.push_str(to);
        last = end;
    }
    (last > 0).then(|| {
        out.push_str(&text[last..]);
        out
    })
}

fn rebase_value(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(text) => {
            if let Some(rebased) = rebase_text(text, from, to) {
                *text = rebased;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rebase_value(v, from, to)),
        Value::Object(map) => map.values_mut().for_each(|v| rebase_value(v, from, to)),
        _ => {}
    }
}

impl Approver for Rebase<'_> {
    fn tool_signals(
        &mut self,
        call: &ToolCall,
        registry: &Registry,
    ) -> Option<crate::tools::exec::signals::CallSignals> {
        self.inner.tool_signals(call, registry)
    }

    fn model_route(&mut self, route: &Value) {
        if let Some(label) = self.descendant {
            self.inner.notice(&format!(
                "Descendant {label}: {}",
                route["model"].as_str().unwrap_or("configured model")
            ));
        } else {
            self.inner.model_route(route);
        }
    }
    fn progress(&mut self, steps: u32, phase: &str, text: &str) {
        if let Some(label) = self.descendant {
            self.inner
                .progress(steps, &format!("descendant {label}: {phase}"), text);
        } else {
            self.inner.progress(steps, phase, text);
        }
    }
    fn authorize(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
    ) -> Result<(), Denial> {
        let mut call = call.clone();
        rebase_value(
            &mut call.arguments,
            &self.from.to_string_lossy(),
            &self.to.to_string_lossy(),
        );
        let summary = format!("[{}] {summary}", self.descendant.unwrap_or("isolated copy"));
        self.inner.authorize(&call, &summary, needs_approval)
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
        if cancel.load(Ordering::SeqCst) {
            return Err(Denial::User);
        }
        let mut call = call.clone();
        rebase_value(
            &mut call.arguments,
            &self.from.to_string_lossy(),
            &self.to.to_string_lossy(),
        );
        let summary = format!("[{}] {summary}", self.descendant.unwrap_or("isolated copy"));
        self.inner
            .authorize_from(&call, &summary, needs_approval, provenance, cancel)
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
        if scope == MutationScope::ManagedMemory {
            return self
                .inner
                .authorize_mutation(original, tool, paths, unbounded, cancel, scope);
        }
        let paths: Vec<PathBuf> = paths
            .iter()
            .map(|path| {
                if !path.is_absolute()
                    || path.components().any(|component| {
                        matches!(component, Component::ParentDir | Component::CurDir)
                    })
                {
                    return Err("native mutation target is not canonical".to_owned());
                }
                let relative = path.strip_prefix(self.from).map_err(|_| {
                    "native mutation target is outside the isolated workspace".to_owned()
                })?;
                Ok(self.to.join(relative))
            })
            .collect::<Result<_, String>>()?;
        let mut original = original.clone();
        rebase_value(
            &mut original.arguments,
            &self.from.to_string_lossy(),
            &self.to.to_string_lossy(),
        );
        self.inner
            .authorize_mutation(&original, tool, &paths, unbounded, cancel, scope)
    }
    fn policy_explanation(&mut self, call: &ToolCall, explanation: &approval::PolicyExplanation) {
        self.inner.policy_explanation(call, explanation);
    }
    fn notice(&mut self, message: &str) {
        self.inner.notice(message)
    }
    fn ask_questions(
        &mut self,
        request: &crate::agent::workflow::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        self.inner.ask_questions(request, cancel)
    }
    fn memory_capture_allowed(&mut self, cancel: &Arc<AtomicBool>) -> Result<bool, String> {
        self.inner.memory_capture_allowed(cancel)
    }
    fn add_cost(&mut self, cost: f64) {
        self.inner.add_cost(cost)
    }
    fn take_messages(&mut self) -> Vec<String> {
        self.inner.take_messages()
    }
    fn transcript_sink(
        &self,
    ) -> Option<std::sync::Arc<crate::agent::workers::child_transcript::ChildTranscript>> {
        self.inner.transcript_sink()
    }
    fn take_worker_settings(
        &mut self,
    ) -> Option<crate::agent::workers::live_tasks::control::WorkerSettings> {
        self.inner.take_worker_settings()
    }
    fn worker_settings_applied(
        &mut self,
        settings: &crate::agent::workers::live_tasks::control::WorkerSettings,
        resolved: &crate::config::types::Config,
    ) {
        self.inner.worker_settings_applied(settings, resolved)
    }
    fn worker_settings_failed(&mut self, error: &str) {
        self.inner.worker_settings_failed(error)
    }
    fn live_coordination(&self) -> bool {
        self.inner.live_coordination()
    }
    fn peer_message(&mut self, args: &Value) -> Result<Value, String> {
        self.inner.peer_message(args)
    }
    fn tool_started(&mut self, call_id: &str, name: &str, registry: &Registry) {
        self.inner.tool_started(call_id, name, registry)
    }
    fn tool_finished(&mut self, call_id: &str) {
        self.inner.tool_finished(call_id)
    }
}
