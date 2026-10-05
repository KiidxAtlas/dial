//! Bound a generation before its first answer/tool argument, including providers with hidden
//! reasoning. Cancellation is request-local: a gate hit must never look like a user interrupt.
use crate::{config::types::ReasoningGuardConfig, llm::{self, Chat, Reply, StreamEvent, provider::Backend}};
use anyhow::Result;
use parking_lot::Mutex;
use std::{fmt, path::Path, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, thread, time::{Duration, Instant}};

#[derive(Debug)]
pub(super) struct GateHit(pub String);
impl fmt::Display for GateHit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}
impl std::error::Error for GateHit {}

struct Gate {
    started: Instant,
    first_output: bool,
    reasoning_bytes: usize,
    hit: Option<String>,
}

impl Gate {
    fn new() -> Self { Self { started: Instant::now(), first_output: false, reasoning_bytes: 0, hit: None } }

    fn check(&mut self, config: &ReasoningGuardConfig) -> bool {
        if self.hit.is_some() { return false; }
        if !config.enabled { return true; }
        if self.reasoning_bytes >= config.max_reasoning_bytes {
            self.hit = Some(format!("{} bytes of streamed reasoning in one generation", config.max_reasoning_bytes));
        } else if !self.first_output && self.started.elapsed() >= Duration::from_secs(config.max_pre_action_secs) {
            self.hit = Some(format!("no answer text or tool arguments began within {} seconds", config.max_pre_action_secs));
        }
        self.hit.is_none()
    }

    fn output(&mut self, config: &ReasoningGuardConfig, nonempty: bool) -> bool {
        if !self.check(config) { return false; }
        self.first_output |= nonempty;
        true
    }
}

/// One recovery for all reasoning-only failures in a turn, even across successful calls.
#[derive(Default)]
pub(super) struct Recovery { used: bool }
impl Recovery {
    pub fn intercept(&mut self, reason: &str) -> std::result::Result<String, String> {
        if self.used {
            return Err(format!("Stopped: reasoning gate hit again ({reason}) after its one recovery attempt. No partial tool call was executed. Adjust [reasoning_guard] or the model's reasoning setting before retrying."));
        }
        self.used = true;
        Ok(format!("Reasoning gate: {reason}. The generation was discarded and no tool from it ran. Take the next small, useful tool action now, or answer directly. Limit reasoning to the concrete uncertainty that changes that action; do not restart broad planning."))
    }
}

/// The watcher also handles silence and user cancellation when no stream callbacks arrive.
/// Answer/argument deltas disarm the pre-action deadline, but not the reasoning-byte cap, and never
/// execute a tool; normal completion is still required by the caller.
pub(super) fn chat(
    backend: &mut Backend, adapter: Option<&Path>, request: &Chat, timeout: u64,
    interrupt: Arc<AtomicBool>, config: &ReasoningGuardConfig,
    on_text: &mut dyn FnMut(&str) -> bool, on_notice: &mut dyn FnMut(&str),
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    let cancel = Arc::new(AtomicBool::new(false));
    let state = Mutex::new(Gate::new());
    let (done, wait) = mpsc::channel::<()>();
    let result = thread::scope(|scope| {
        // Own the sender in this scope so unwinding a provider/callback also wakes the watcher.
        let done = done;
        let (watch_interrupt, watch_state, watch_cancel) = (&interrupt, &state, &cancel);
        let watcher = scope.spawn(move || {
            loop {
                match wait.recv_timeout(Duration::from_millis(20)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {},
                }
                if watch_interrupt.load(Ordering::SeqCst) || !watch_state.lock().check(config) {
                    watch_cancel.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
        let mut text = |text: &str| {
            let allowed = state.lock().output(config, !text.trim().is_empty());
            if !allowed { cancel.store(true, Ordering::SeqCst); }
            allowed && !interrupt.load(Ordering::SeqCst) && on_text(text)
        };
        let mut event = |event: &StreamEvent| {
            let allowed = {
                let mut gate = state.lock();
                match event {
                    StreamEvent::Reasoning(text) => {
                        gate.reasoning_bytes = gate.reasoning_bytes.saturating_add(text.len());
                        gate.check(config)
                    },
                    StreamEvent::ToolArguments { delta, .. } => gate.output(config, !delta.trim().is_empty()),
                }
            };
            if !allowed { cancel.store(true, Ordering::SeqCst); }
            allowed && !interrupt.load(Ordering::SeqCst) && on_event(event)
        };
        let result = backend.chat_with_control_events(adapter, request, timeout, Some(&mut text), on_notice, cancel.clone(), &mut event);
        let _ = done.send(());
        let _ = watcher.join();
        result
    });
    if interrupt.load(Ordering::SeqCst) { return Err(llm::Interrupted.into()); }
    let mut gate = state.lock();
    // A final reply can race the watchdog. Apply the deadline before releasing any of its calls.
    if result.is_ok() { gate.check(config); }
    if let Some(reason) = &gate.hit { return Err(GateHit(reason.clone()).into()); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_never_resets_the_deadline_and_empty_deltas_do_not_disarm_it() {
        let config = ReasoningGuardConfig::default();
        let mut gate = Gate::new();
        assert!(gate.output(&config, false));
        gate.started -= Duration::from_secs(61);
        assert!(!gate.check(&config));
        assert!(!gate.output(&config, true), "late arguments cannot bypass the gate");
    }

    #[test]
    fn answer_text_disarms_only_deadline_not_reasoning_byte_budget() {
        let config = ReasoningGuardConfig::default();
        let mut gate = Gate::new();
        assert!(gate.output(&config, true));
        gate.started -= Duration::from_secs(61);
        assert!(gate.check(&config), "long real answers remain allowed");
        gate.reasoning_bytes = config.max_reasoning_bytes;
        assert!(!gate.check(&config), "a preamble cannot disable the reasoning budget");
        let mut gate = Gate::new();
        gate.reasoning_bytes = config.max_reasoning_bytes;
        assert!(!gate.check(&config));
        let mut gate = Gate::new();
        gate.reasoning_bytes = usize::MAX;
        assert!(gate.check(&ReasoningGuardConfig { enabled: false, ..config }));
    }

    #[test]
    fn reasoning_recovery_is_bounded_across_causes() {
        let mut recovery = Recovery::default();
        assert!(recovery.intercept("time limit").unwrap().contains("Take the next small"));
        assert!(recovery.intercept("token limit").unwrap_err().contains("one recovery attempt"));
    }
}
