//! The UI surface contract the agent reports through.

use crate::agent::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Allow matching calls for the rest of this session.
    AllowAlways,
    /// Allow matching calls in this project from now on; the rule is saved with the project state.
    AllowProject,
    Deny,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RequestPhase {
    #[default]
    Waiting,
    Loading,
    Processing,
    Generating,
    Retrying,
    Cancelling,
}
impl RequestPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting for model",
            Self::Loading => "loading model",
            Self::Processing => "processing prompt",
            Self::Generating => "generating",
            Self::Retrying => "retrying request",
            Self::Cancelling => "cancelling request",
        }
    }
}

pub trait Ui {
    /// Persistence failures stop inference before any completed calls can be admitted.
    fn execution_error(&self) -> Option<String> {
        None
    }
    /// Whether this surface draws workers from shared runtime bodies instead of text notices.
    fn renders_workers(&self) -> bool {
        false
    }
    fn runtime_tree(&self) -> Option<crate::runtime::Tree> {
        None
    }
    fn runtime(&mut self, _tree: &crate::runtime::Tree) {}
    /// Shared result body, delivered before its legacy display callback.
    fn result_node(&mut self, _body: &crate::runtime::Body) {}
    fn text_delta(&mut self, _text: &str) {}
    /// Call-correlated callbacks let surfaces update an in-flight row after streamed arguments or
    /// results arrive. Legacy implementations retain a useful default projection.
    fn tool_call_with_id(&mut self, _call_id: &str, name: &str, summary: &str) {
        self.tool_call(name, summary);
    }
    fn tool_call_with_args(&mut self, call_id: &str, name: &str, summary: &str, _args: &Value) {
        self.tool_call_with_id(call_id, name, summary);
    }
    fn tool_presentation(
        &mut self,
        _call_id: &str,
        _view: &crate::tools::surface::presentation::Presentation,
    ) {
    }
    fn tool_result_with_id(&mut self, _call_id: &str, name: &str, output: &ToolOutput) {
        self.tool_result(name, output);
    }
    fn tool_call(&mut self, _name: &str, _summary: &str) {}
    fn tool_result(&mut self, _name: &str, _output: &ToolOutput) {}
    fn notice(&mut self, _message: &str) {}
    /// A typed controller event. Plain surfaces show its sentence as a notice; rich surfaces
    /// place it in a lane (context line, divider, alert) by its kind.
    fn event(&mut self, event: &crate::agent::ui::ui_event::UiEvent) {
        self.notice(&event.text);
    }
    /// Checkpointed paths for this turn. Rich surfaces already show their review footer.
    fn changed_files(&mut self, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        let mut notice = format!("Changed files this turn: {}", paths.len());
        for path in paths.iter().take(20) {
            notice.push_str("\n  ");
            notice.push_str(path);
        }
        if paths.len() > 20 {
            notice.push_str(&format!("\n  ... and {} more", paths.len() - 20));
        }
        self.notice(&notice);
    }
    /// Internal bookkeeping (policy selection, index refresh), hidden on ordinary surfaces.
    /// A diagnostics surface may opt in; actionable warnings must use `notice`.
    fn diagnostic(&mut self, _message: &str) {}
    /// Display an already-authorized image reference; plain/structured surfaces may omit previews.
    fn image(&mut self, _reference: &Value) {}
    /// Effective request controls, for live status views; never a correctness claim.
    fn request_policy(&mut self, _control: &Value) {}
    /// Visible observer activity; never substituted for tool or verification results.
    fn laya_activity(&mut self, status: &str, message: &str) {
        self.notice(&format!("Laya · {status}: {message}"));
    }
    /// Context occupancy for display; estimates do not affect billing or compaction.
    fn context_usage(&mut self, _tokens: u64, _estimated: bool) {}
    fn usage(&mut self, _prompt: Option<u64>, _output: Option<u64>, _seconds: f64) {}
    fn efficiency(&mut self, _report: &Value) {}
    fn model_progress(&mut self, _progress: &llm::ModelProgress) {}
    fn tool_output(&mut self, _name: &str, _text: &str) {}
    /// Latest progress snapshot for a running tool; replaces the previous one in place.
    fn tool_progress(&mut self, _name: &str, _text: &str) {}
    fn background_notice(&mut self, _message: &str) {}
    fn reasoning_delta(&mut self, _text: &str) {}
    /// Observational progress only: incomplete arguments are never dispatched as tool calls.
    fn tool_arguments_delta(&mut self, _index: u64, _id: &str, _name: &str, _delta: &str) {}
    fn cached_usage(&mut self, _read: Option<u64>, _write: Option<u64>) {}
    fn extension_panel(&mut self, title: &str, markdown: &str) {
        self.notice(&format!("{title}\n{markdown}"));
    }
    fn role(&mut self, _role: &str, _model: &str) {}
    fn approve(&mut self, _tool: &str, _summary: &str) -> Decision {
        Decision::Deny
    }
    fn approve_with_cancel(&mut self, tool: &str, summary: &str, cancel: &AtomicBool) -> Decision {
        if cancel.load(Ordering::SeqCst) {
            Decision::Deny
        } else {
            self.approve(tool, summary)
        }
    }
    /// Approval for one tool call. The default ignores call identity; surfaces that correlate
    /// approvals with call cards override it.
    fn approve_call(
        &mut self,
        _call_id: &str,
        tool: &str,
        summary: &str,
        cancel: Option<&AtomicBool>,
    ) -> Decision {
        match cancel {
            Some(flag) => self.approve_with_cancel(tool, summary, flag),
            None => self.approve(tool, summary),
        }
    }
    /// Wait for an answer while the current agent turn remains active.
    fn ask_user(&mut self, _title: &str, _question: &str, _options: &[String]) -> Option<String> {
        None
    }
    /// Job identity is separate from the pending foreground tool; replace denotes a PTY snapshot.
    fn background_output(&mut self, name: &str, text: &str, _replace: bool) {
        self.background_notice(&format!("{name}: {text}"));
    }
    /// Typed actual human input; cancellation is not a selected recommendation.
    fn ask_questions(
        &mut self,
        _request: &crate::agent::workflow::questions::QuestionRequest,
        _cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        crate::agent::workflow::questions::QuestionOutcome::Unsupported(
            "Ask tool requires a connected human input surface".into(),
        )
    }
    /// Distinguish an interactive cancellation from a UI that cannot collect input.
    fn interactive_input(&self) -> bool {
        false
    }
    fn approval_reason(&mut self) -> Option<String> {
        None
    }
    /// Called while an inference/tool worker is pending so interactive surfaces can repaint and pump input.
    fn heartbeat(&mut self) {}
    /// Elapsed time without visible model output while its request is still pending.
    fn model_wait(&mut self, _silence: Duration) {}
    fn request_phase(&mut self, _phase: RequestPhase) {}
    fn completion(&mut self, _summary: &Value) {}
    /// Mark only a failed inference attempt as discarded; completed work is untouched.
    fn inference_discarded(&mut self, reason: &str) {
        self.notice(reason);
    }
    /// Messages the user typed while the turn ran. Polled between steps; the agent appends them as user
    /// messages before the next model request so the model can change course without waiting for the turn to end.
    fn take_steering(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// Busy input whose intent is undecided. Unlike explicit steering, this must not cancel work
    /// until the agent has decided whether to steer or start an independent worker.
    fn take_pending_messages(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// Restore unhandled busy input after cancellation, ahead of later queued messages.
    fn restore_pending_messages(&mut self, _messages: Vec<String>) {}
    /// An undecided message was accepted for routing; surfaces can show it exactly once.
    fn pending_message_accepted(&mut self, _text: &str) {}
    /// Maintenance must yield to queued foreground work without consuming that work.
    fn maintenance_ready(&mut self) -> bool {
        true
    }
    /// A tool call finished running; `seconds` is its wall time. Structured surfaces (JSON output) use it.
    fn tool_timing(&mut self, _name: &str, _seconds: f64) {}
    /// The task list changed. Interactive surfaces keep it visible; structured outputs emit it as an event.
    fn todo(&mut self, _todo: &TodoList) {}
}

pub struct NullUi;
impl Ui for NullUi {}
