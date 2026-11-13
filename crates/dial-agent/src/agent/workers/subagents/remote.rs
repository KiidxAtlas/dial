use super::{Approver, MutationScope, Outcome, update_model_route};
use crate::agent::safety::approval;
use crate::agent::safety::approval::Denial;
use crate::llm::ToolCall;
use crate::tools::integrations::ext::Registry;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

pub(super) enum Message {
    Policy(ToolCall, approval::PolicyExplanation),
    CapturePolicy {
        cancel: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<bool, String>>,
    },
    Question {
        request: crate::agent::workflow::questions::QuestionRequest,
        cancel: Arc<AtomicBool>,
        reply: mpsc::Sender<crate::agent::workflow::questions::QuestionOutcome>,
    },
    Approve {
        call: ToolCall,
        summary: String,
        needs_approval: bool,
        provenance: approval::CallProvenance,
        cancel: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<(), Denial>>,
    },
    NativeMutation {
        original: ToolCall,
        tool: String,
        paths: Vec<PathBuf>,
        unbounded: bool,
        scope: MutationScope,
        cancel: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Done(usize, Box<Outcome>),
    ToolStarted(
        usize,
        String,
        Weak<dyn crate::tools::integrations::ext::Tool>,
    ),
    ToolFinished(usize, String),
    Notice(String),
    Cost(f64),
}

/// Approves from a worker thread by asking the calling thread and waiting for its answer.
pub(super) struct Remote(
    pub(super) mpsc::Sender<Message>,
    pub(super) Option<crate::runtime::Body>,
    pub(super) usize,
    pub(super) Arc<AtomicBool>,
);

impl Approver for Remote {
    fn model_route(&mut self, route: &Value) {
        if let Some(body) = &self.1 {
            update_model_route(body, route);
        }
    }
    fn memory_capture_allowed(&mut self, cancel: &Arc<AtomicBool>) -> Result<bool, String> {
        let (reply, answer) = mpsc::channel();
        if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
            return Err("memory capture cancelled".into());
        }
        self.0
            .send(Message::CapturePolicy {
                cancel: cancel.clone(),
                reply,
            })
            .map_err(|_| "memory capture owner disconnected".to_owned())?;
        loop {
            if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
                return Err("memory capture cancelled".into());
            }
            match answer.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("memory capture owner disconnected".into());
                }
            }
        }
    }
    fn ask_questions(
        &mut self,
        request: &crate::agent::workflow::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        use crate::agent::workflow::questions::QuestionOutcome;
        if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
            return QuestionOutcome::Cancelled;
        }
        let (reply, answer) = mpsc::channel();
        if self
            .0
            .send(Message::Question {
                request: request.clone(),
                cancel: self.3.clone(),
                reply,
            })
            .is_err()
        {
            return QuestionOutcome::Disconnected;
        }
        loop {
            if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
                return QuestionOutcome::Cancelled;
            }
            match answer.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(outcome) => {
                    return if cancel.load(Ordering::SeqCst) || self.3.load(Ordering::SeqCst) {
                        QuestionOutcome::Cancelled
                    } else {
                        outcome
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return QuestionOutcome::Disconnected,
            }
        }
    }
    fn tool_started(&mut self, call_id: &str, name: &str, registry: &Registry) {
        if let Some(tool) = registry.get(name) {
            let _ = self.0.send(Message::ToolStarted(
                self.2,
                call_id.to_owned(),
                Arc::downgrade(tool),
            ));
        }
        if let Some(body) = &self.1 {
            let steps = body.read(|n| {
                n.result
                    .data
                    .as_ref()
                    .and_then(|d| d["steps"].as_u64())
                    .unwrap_or(0) as u32
            });
            body.tool_started(name, steps);
        }
    }
    fn tool_finished(&mut self, call_id: &str) {
        let _ = self
            .0
            .send(Message::ToolFinished(self.2, call_id.to_owned()));
    }
    fn progress(&mut self, steps: u32, phase: &str, text: &str) {
        if let Some(body) = &self.1 {
            body.progress(phase, steps);
            if !text.is_empty() {
                body.append(text);
            }
        }
    }
    fn policy_explanation(&mut self, call: &ToolCall, explanation: &approval::PolicyExplanation) {
        let _ = self
            .0
            .send(Message::Policy(call.clone(), explanation.clone()));
    }
    fn notice(&mut self, message: &str) {
        let _ = self.0.send(Message::Notice(message.to_owned()));
    }
    fn add_cost(&mut self, cost: f64) {
        let _ = self.0.send(Message::Cost(cost));
    }
    fn authorize(
        &mut self,
        call: &ToolCall,
        summary: &str,
        needs_approval: bool,
    ) -> Result<(), Denial> {
        let cancel = self.3.clone();
        self.authorize_with_cancel(call, summary, needs_approval, &cancel)
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
        let interrupted = || {
            if self.3.load(Ordering::SeqCst) {
                // Propagate whole-job interruption into this callback operation's owner flag,
                // so the parent's pending approve_with_cancel observes either source.
                cancel.store(true, Ordering::SeqCst);
            }
            cancel.load(Ordering::SeqCst)
        };
        if interrupted() {
            return Err(Denial::User);
        }
        let (reply, answer) = mpsc::channel();
        let ask = Message::Approve {
            call: call.clone(),
            summary: summary.to_owned(),
            needs_approval,
            provenance,
            cancel: cancel.clone(),
            reply,
        };
        if self.0.send(ask).is_err() {
            return Err(Denial::User);
        }
        loop {
            if interrupted() {
                return Err(Denial::User);
            }
            match answer.recv_timeout(std::time::Duration::from_millis(80)) {
                Ok(result) => {
                    return if interrupted() {
                        Err(Denial::User)
                    } else {
                        result
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Denial::User),
            }
        }
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
        let interrupted = || {
            if self.3.load(Ordering::SeqCst) {
                cancel.store(true, Ordering::SeqCst);
            }
            cancel.load(Ordering::SeqCst)
        };
        if interrupted() {
            return Err("native mutation cancelled".into());
        }
        let (reply, answer) = mpsc::channel();
        if self
            .0
            .send(Message::NativeMutation {
                original: original.clone(),
                tool: tool.to_owned(),
                paths: paths.to_vec(),
                unbounded,
                scope,
                cancel: cancel.clone(),
                reply,
            })
            .is_err()
        {
            return Err("native mutation controller unavailable".into());
        }
        loop {
            if interrupted() {
                return Err("native mutation cancelled".into());
            }
            match answer.recv_timeout(std::time::Duration::from_millis(80)) {
                Ok(result) => {
                    return if interrupted() {
                        Err("native mutation cancelled".into())
                    } else {
                        result
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("native mutation controller unavailable".into());
                }
            }
        }
    }
}
