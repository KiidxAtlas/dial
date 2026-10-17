//! Bound a generation before its first answer/tool argument, including providers with hidden
//! reasoning. Cancellation is request-local: a gate hit must never look like a user interrupt.
use crate::{
    config::types::ReasoningGuardConfig,
    llm::{self, Chat, Reply, StreamEvent, provider::Backend},
};
use anyhow::Result;
use parking_lot::Mutex;
use std::{
    fmt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(super) struct GateHit(pub String);
impl fmt::Display for GateHit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for GateHit {}

impl GateHit {
    pub(super) fn is_repetition(&self) -> bool {
        self.0.starts_with("stream repeated ")
    }
}

/// Exact suffix cycles only: four repeats and at least 1 KiB of replay before interception.
/// Fixed memory and scan cadence make detection independent of provider chunk boundaries.
#[derive(Default)]
struct Repetition {
    tail: Vec<u8>,
    pending: usize,
}

impl Repetition {
    fn push(&mut self, text: &str) -> bool {
        for &byte in text.as_bytes() {
            self.tail.push(byte);
            self.pending += 1;
            if self.pending < 128 {
                continue;
            }
            self.pending = 0;
            if self.tail.len() > 8192 {
                self.tail.drain(..self.tail.len() - 8192);
            }
            let n = self.tail.len();
            for period in 32..=2048.min(n / 4) {
                let repeats = 4.max(1024usize.div_ceil(period));
                if repeats * period > n {
                    continue;
                }
                let block = &self.tail[n - period..];
                if block.iter().any(|b| !b.is_ascii_whitespace())
                    && (2..=repeats)
                        .all(|i| &self.tail[n - i * period..n - (i - 1) * period] == block)
                {
                    return true;
                }
            }
        }
        false
    }
}

struct Gate {
    started: Instant,
    first_output: bool,
    preparing: bool,
    generation_started: bool,
    reasoning_bytes: usize,
    hit: Option<String>,
    thinking_repetition: Repetition,
    text_repetition: Repetition,
    tools_started: bool,
}

impl Gate {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            first_output: false,
            preparing: false,
            generation_started: false,
            reasoning_bytes: 0,
            hit: None,
            thinking_repetition: Repetition::default(),
            text_repetition: Repetition::default(),
            tools_started: false,
        }
    }

    fn check(&mut self, config: &ReasoningGuardConfig) -> bool {
        if self.hit.is_some() {
            return false;
        }
        // Disabling the guard drops only the time deadline. A positive byte cap still applies;
        // zero explicitly leaves reasoning size unlimited. Repetition recovery is independent.
        if !config.enabled {
            if config.max_reasoning_bytes > 0 && self.reasoning_bytes >= config.max_reasoning_bytes
            {
                self.hit = Some(format!(
                    "{} bytes of streamed reasoning in one generation",
                    config.max_reasoning_bytes
                ));
            }
            return self.hit.is_none();
        }
        if config.max_reasoning_bytes > 0 && self.reasoning_bytes >= config.max_reasoning_bytes {
            self.hit = Some(format!(
                "{} bytes of streamed reasoning in one generation",
                config.max_reasoning_bytes
            ));
        } else if !self.preparing
            && config.max_pre_action_secs > 0
            && !self.first_output
            && self.started.elapsed() >= Duration::from_secs(config.max_pre_action_secs)
        {
            self.hit = Some(format!(
                "no answer text or tool arguments began within {} seconds",
                config.max_pre_action_secs
            ));
        }
        self.hit.is_none()
    }

    fn progress(&mut self, phase: llm::InferencePhase) {
        match phase {
            llm::InferencePhase::Loading | llm::InferencePhase::Processing
                if !self.generation_started =>
            {
                // Model startup and prefill have their own provider timeout, not a reasoning budget.
                self.preparing = true;
            }
            llm::InferencePhase::Generating => self.begin_generation(),
            _ => {}
        }
    }

    fn begin_generation(&mut self) {
        if !self.generation_started {
            if self.preparing {
                self.started = Instant::now();
            }
            self.preparing = false;
            self.generation_started = true;
        }
    }

    fn output(&mut self, config: &ReasoningGuardConfig, nonempty: bool) -> bool {
        self.begin_generation();
        if !self.check(config) {
            return false;
        }
        self.first_output |= nonempty;
        true
    }

    fn repeated(&mut self, text: &str, thinking: bool) -> bool {
        let detector = if thinking {
            &mut self.thinking_repetition
        } else {
            &mut self.text_repetition
        };
        if (thinking || !self.tools_started) && detector.push(text) {
            self.hit = Some(format!(
                "stream repeated {} in an exact cycle (at least four repeats and 1024 bytes)",
                if thinking { "reasoning" } else { "answer text" }
            ));
        }
        self.hit.is_some()
    }
}

/// One recovery for all reasoning-only failures in a turn, even across successful calls.
#[derive(Default)]
pub(super) struct Recovery {
    used: bool,
}
impl Recovery {
    pub fn intercept(&mut self, reason: &str) -> std::result::Result<String, String> {
        self.recover(reason, false)
    }

    /// Like `intercept`, but the retry must first record its thinking as a todo plan.
    pub fn intercept_planned(&mut self, reason: &str) -> std::result::Result<String, String> {
        self.recover(reason, true)
    }

    fn recover(&mut self, reason: &str, plan: bool) -> std::result::Result<String, String> {
        if self.used {
            return Err(format!(
                "Stopped: reasoning gate hit again ({reason}) after its one recovery attempt. No partial tool call was executed. Adjust [reasoning_guard] or the model's reasoning setting before retrying."
            ));
        }
        self.used = true;
        if plan {
            return Ok(format!(
                "Reasoning gate: {reason}. The generation was discarded and no tool from it ran. Write your thinking down as a todo plan before anything else: call todo with 1-3 concrete unresolved questions (investigation=true) plus the known next steps. Only todo is offered until a plan exists. Then act on the first item; do not restart broad planning in hidden reasoning."
            ));
        }
        Ok(format!(
            "Reasoning gate: {reason}. The generation was discarded and no tool from it ran. Take the next small, useful tool action now, or answer directly. Limit reasoning to the concrete uncertainty that changes that action; do not restart broad planning."
        ))
    }
}

/// The watcher also handles silence and user cancellation when no stream callbacks arrive.
/// Answer/argument deltas disarm the pre-action deadline, but not the reasoning-byte cap, and never
/// execute a tool; normal completion is still required by the caller.
/// Streamed callbacks for one guarded chat call.
pub(super) struct Sinks<'a> {
    pub on_text: &'a mut dyn FnMut(&str) -> bool,
    pub on_notice: &'a mut dyn FnMut(&str),
    pub on_event: &'a mut dyn FnMut(&StreamEvent) -> bool,
}

pub(super) fn chat(
    backend: &mut Backend,
    adapter: Option<&Path>,
    request: &Chat,
    timeout: u64,
    interrupt: Arc<AtomicBool>,
    config: &ReasoningGuardConfig,
    sinks: Sinks<'_>,
) -> Result<Reply> {
    let Sinks {
        on_text,
        on_notice,
        on_event,
    } = sinks;
    backend.next_reasoning_guard(config);
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
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if watch_interrupt.load(Ordering::SeqCst) || !watch_state.lock().check(config) {
                    watch_cancel.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
        let mut text = |text: &str| {
            let allowed = {
                let mut gate = state.lock();
                !gate.repeated(text, false) && gate.output(config, !text.trim().is_empty())
            };
            if !allowed {
                cancel.store(true, Ordering::SeqCst);
            }
            allowed && !interrupt.load(Ordering::SeqCst) && on_text(text)
        };
        let mut event = |event: &StreamEvent| {
            let allowed = {
                let mut gate = state.lock();
                match event {
                    StreamEvent::Retry { waiting } => {
                        if gate.hit.is_some() {
                            false
                        } else {
                            *gate = Gate::new();
                            gate.preparing = *waiting;
                            true
                        }
                    }
                    StreamEvent::Progress(progress) => {
                        gate.progress(progress.phase);
                        gate.check(config)
                    }
                    StreamEvent::Reasoning(text) => {
                        gate.begin_generation();
                        gate.reasoning_bytes = gate.reasoning_bytes.saturating_add(text.len());
                        !gate.repeated(text, true) && gate.check(config)
                    }
                    StreamEvent::ToolArguments { delta, .. } => {
                        gate.tools_started = true;
                        gate.output(config, !delta.trim().is_empty())
                    }
                }
            };
            if !allowed {
                cancel.store(true, Ordering::SeqCst);
            }
            allowed && !interrupt.load(Ordering::SeqCst) && on_event(event)
        };
        let result = backend.chat_with_control_events(
            adapter,
            request,
            crate::llm::Limits {
                timeout_secs: timeout,
                interrupt: cancel.clone(),
            },
            Some(&mut text),
            on_notice,
            &mut event,
        );
        let _ = done.send(());
        let _ = watcher.join();
        result
    });
    if interrupt.load(Ordering::SeqCst) {
        return Err(llm::Interrupted.into());
    }
    let mut gate = state.lock();
    // A final reply can race the watchdog. Apply the deadline before releasing any of its calls.
    if result.is_ok() {
        gate.check(config);
    }
    if let Some(reason) = &gate.hit {
        return Err(GateHit(reason.clone()).into());
    }
    result
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod plan_tests;

#[cfg(test)]
mod repetition_tests;
