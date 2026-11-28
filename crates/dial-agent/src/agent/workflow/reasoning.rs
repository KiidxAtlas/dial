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
pub(in crate::agent) struct GateHit {
    pub reason: String,
    pub digest: Option<String>,
    pub user_wrap_up: bool,
}
impl GateHit {
    pub(in crate::agent) fn new(reason: String, digest: Option<String>) -> Self {
        Self {
            reason,
            digest,
            user_wrap_up: false,
        }
    }

    pub(in crate::agent) fn user_wrap_up(digest: Option<String>) -> Self {
        Self {
            reason: "user requested immediate action (wrapped up thinking)".into(),
            digest,
            user_wrap_up: true,
        }
    }

    pub(in crate::agent) fn is_repetition(&self) -> bool {
        self.reason.starts_with("stream repeated ")
    }

    pub(in crate::agent) fn is_user_wrap_up(&self) -> bool {
        self.user_wrap_up
    }

    pub(in crate::agent) fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }
}
impl fmt::Display for GateHit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(f)
    }
}
impl std::error::Error for GateHit {}

/// Exact suffix cycles: four repeats and at least 1 KiB of replay for answer text and short
/// reasoning cycles, or three repeats for long reasoning cycles (`>= 160` bytes/period).
/// Fixed memory and scan cadence make detection independent of provider chunk boundaries.
#[derive(Default)]
struct Repetition {
    tail: Vec<u8>,
    pending: usize,
}

impl Repetition {
    fn push(&mut self, text: &str) -> bool {
        self.push_mode(text, false)
    }

    fn push_mode(&mut self, text: &str, thinking: bool) -> bool {
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
            let max_period = if thinking { n / 3 } else { n / 4 };
            for period in 32..=2048.min(max_period) {
                let repeats = if thinking && period >= 160 {
                    3
                } else {
                    4.max(1024usize.div_ceil(period))
                };
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

/// Bounded rolling-window detector for non-exact reasoning degeneration loops:
/// 1. Shingle novelty collapse: >= 60% of 7-word shingles in the most recent 160 words already
///    appeared earlier in the reasoning block (catches paraphrase/opener-drift spirals).
/// 2. Non-contiguous paragraph replay: >= 2 long paragraphs (>= 160 bytes, >= 24 words) repeat
///    earlier paragraphs at >= 75% 5-word Jaccard similarity (catches interleaved replays).
///
/// Used only on streamed reasoning, never on answer text or tool payloads.
#[derive(Default)]
struct ReasoningLoopDetector {
    window: String,
    pending: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningLoopKind {
    NoveltyCollapse(usize),
    ParagraphReplay,
}

impl ReasoningLoopDetector {
    const WINDOW_BYTES: usize = 12_288;
    const SCAN_CADENCE: usize = 128;
    const MIN_WORDS: usize = 320;
    const TAIL_WORDS: usize = 160;
    const SHINGLE_WORDS: usize = 7;
    const NOVELTY_REPEAT_PCT: usize = 60;
    const MIN_PARA_BYTES: usize = 160;
    const MIN_PARA_WORDS: usize = 24;
    const PARA_SHINGLE_WORDS: usize = 5;
    const PARA_JACCARD_PCT: usize = 75;
    const MIN_REPLAYED_PARAS: usize = 2;

    fn push(&mut self, text: &str) -> Option<ReasoningLoopKind> {
        let mut offset = 0;
        while offset < text.len() {
            let remaining_to_scan = Self::SCAN_CADENCE.saturating_sub(self.pending).max(1);
            let mut end = (offset + remaining_to_scan).min(text.len());
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
            let slice = &text[offset..end];
            self.window.push_str(slice);
            self.pending += slice.len();
            offset = end;
            if self.pending < Self::SCAN_CADENCE {
                continue;
            }
            self.pending = 0;
            if self.window.len() > Self::WINDOW_BYTES {
                let mut cut = self.window.len() - Self::WINDOW_BYTES;
                while cut < self.window.len() && !self.window.is_char_boundary(cut) {
                    cut += 1;
                }
                self.window.drain(..cut);
            }
            if let Some(hit) = self.inspect() {
                return Some(hit);
            }
        }
        None
    }

    fn inspect(&self) -> Option<ReasoningLoopKind> {
        if let Some(pct) = Self::novelty_collapse(&self.window) {
            return Some(ReasoningLoopKind::NoveltyCollapse(pct));
        }
        if Self::paragraph_replay(&self.window) {
            return Some(ReasoningLoopKind::ParagraphReplay);
        }
        None
    }

    fn tokenize(text: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut current = String::new();
        for ch in text.chars() {
            if ch.is_alphanumeric() {
                for lower in ch.to_lowercase() {
                    current.push(lower);
                }
            } else if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            words.push(current);
        }
        words
    }

    fn hash_shingle(words: &[String]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for word in words {
            for byte in word.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash
    }

    fn novelty_collapse(window: &str) -> Option<usize> {
        let words = Self::tokenize(window);
        if words.len() < Self::MIN_WORDS {
            return None;
        }
        let k = Self::SHINGLE_WORDS;
        let tail_start = words.len() - Self::TAIL_WORDS;
        let mut first_seen = std::collections::HashMap::<u64, usize>::new();
        let mut repeated = 0usize;
        let mut total = 0usize;
        for i in 0..=words.len() - k {
            let h = Self::hash_shingle(&words[i..i + k]);
            let prev = first_seen.get(&h).copied();
            if prev.is_none() {
                first_seen.insert(h, i);
            }
            if i >= tail_start {
                total += 1;
                if prev.is_some_and(|first| first + k <= i) {
                    repeated += 1;
                }
            }
        }
        if total > 0 && repeated * 100 >= total * Self::NOVELTY_REPEAT_PCT {
            Some(repeated * 100 / total)
        } else {
            None
        }
    }

    fn paragraph_replay(window: &str) -> bool {
        let mut seen: Vec<std::collections::HashSet<u64>> = Vec::new();
        let mut replays = 0usize;
        for para in window.split("\n\n").map(str::trim) {
            if para.len() < Self::MIN_PARA_BYTES {
                continue;
            }
            let words = Self::tokenize(para);
            if words.len() < Self::MIN_PARA_WORDS {
                continue;
            }
            let k = Self::PARA_SHINGLE_WORDS;
            let set: std::collections::HashSet<u64> = (0..=words.len() - k)
                .map(|i| Self::hash_shingle(&words[i..i + k]))
                .collect();
            if set.is_empty() {
                continue;
            }
            let matched = seen.iter().any(|prev| {
                let inter = set.intersection(prev).count();
                let union = set.len() + prev.len() - inter;
                union > 0 && inter * 100 >= union * Self::PARA_JACCARD_PCT
            });
            if matched {
                replays += 1;
                if replays >= Self::MIN_REPLAYED_PARAS {
                    return true;
                }
            } else {
                seen.push(set);
            }
        }
        false
    }
}

/// Maximum raw bytes kept from the start of a reasoning block for recovery prefix digests.
const PREFIX_CAPTURE_BYTES: usize = 560;
/// Minimum trimmed prefix characters worth preserving in a recovery note.
const PREFIX_MIN_CHARS: usize = 80;
/// Target maximum length of the preserved recovery prefix digest.
const PREFIX_MAX_BYTES: usize = 480;
struct Gate {
    started: Instant,
    first_output: bool,
    preparing: bool,
    generation_started: bool,
    reasoning_bytes: usize,
    reasoning_prefix: String,
    hit: Option<String>,
    user_wrap_up: bool,
    thinking_repetition: Repetition,
    thinking_loop: ReasoningLoopDetector,
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
            reasoning_prefix: String::new(),
            hit: None,
            user_wrap_up: false,
            thinking_repetition: Repetition::default(),
            thinking_loop: ReasoningLoopDetector::default(),
            text_repetition: Repetition::default(),
            tools_started: false,
        }
    }

    fn record_prefix(&mut self, text: &str) {
        if self.reasoning_prefix.len() >= PREFIX_CAPTURE_BYTES {
            return;
        }
        let budget = PREFIX_CAPTURE_BYTES - self.reasoning_prefix.len();
        let mut end = text.len().min(budget);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        self.reasoning_prefix.push_str(&text[..end]);
    }

    /// Compact sentence-bounded digest of the early reasoning before a cutoff/loop so recovery
    /// can proceed without re-deriving the problem from scratch.
    fn prefix_digest(&self) -> Option<String> {
        let trimmed = self.reasoning_prefix.trim();
        if trimmed.chars().count() < PREFIX_MIN_CHARS {
            return None;
        }
        let mut end = trimmed.len().min(PREFIX_MAX_BYTES);
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        let slice = &trimmed[..end];
        let clipped = if end < trimmed.len() {
            slice
                .rfind("\n\n")
                .or_else(|| slice.rfind(". ").map(|i| i + 1))
                .or_else(|| slice.rfind(".\n").map(|i| i + 1))
                .filter(|&i| i >= PREFIX_MIN_CHARS)
                .map(|i| &slice[..i])
                .unwrap_or_else(|| {
                    let word = slice.rfind(char::is_whitespace).unwrap_or(slice.len());
                    &slice[..word]
                })
        } else {
            slice
        };
        let normalized = clipped.split_whitespace().collect::<Vec<_>>().join(" ");
        (normalized.chars().count() >= PREFIX_MIN_CHARS).then_some(normalized)
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
        if thinking {
            if self.thinking_repetition.push_mode(text, true) {
                self.hit =
                    Some("stream repeated reasoning in an exact cycle (repeated block)".into());
            } else if let Some(kind) = self.thinking_loop.push(text) {
                self.hit = Some(match kind {
                    ReasoningLoopKind::NoveltyCollapse(pct) => format!(
                        "stream repeated reasoning (novelty collapsed: {pct}% repeated phrasing)"
                    ),
                    ReasoningLoopKind::ParagraphReplay => {
                        "stream repeated reasoning (replayed paragraphs)".into()
                    }
                });
            }
        } else if !self.tools_started && self.text_repetition.push(text) {
            self.hit = Some(
                "stream repeated answer text in an exact cycle (at least four repeats and 1024 bytes)"
                    .into(),
            );
        }
        self.hit.is_some()
    }
}

/// Maximum reasoning recoveries allowed in a single turn even when forward progress occurs
/// between them, so alternating progress/loop cycles still terminate cleanly.
const MAX_RECOVERIES_PER_TURN: u8 = 4;

/// Recovery budget for reasoning-only failures in a turn. One consecutive recovery is allowed
/// without forward progress; real tool progress resets the consecutive lock up to
/// `MAX_RECOVERIES_PER_TURN` total recoveries per turn.
#[derive(Default)]
pub(in crate::agent) struct Recovery {
    used: bool,
    total_recoveries: u8,
}
impl Recovery {
    /// Records real forward progress (a completed tool round without a gate hit), allowing a later
    /// step in a long turn to recover once more up to `MAX_RECOVERIES_PER_TURN`.
    pub fn note_progress(&mut self) {
        if self.total_recoveries < MAX_RECOVERIES_PER_TURN {
            self.used = false;
        }
    }

    pub fn intercept(&mut self, reason: &str) -> std::result::Result<String, String> {
        self.recover(reason, false, None)
    }

    pub fn intercept_with_digest(
        &mut self,
        reason: &str,
        digest: Option<&str>,
    ) -> std::result::Result<String, String> {
        self.recover(reason, false, digest)
    }

    /// Like `intercept`, but the retry must first record its thinking as a todo plan.
    pub fn intercept_planned(&mut self, reason: &str) -> std::result::Result<String, String> {
        self.recover(reason, true, None)
    }

    pub fn intercept_planned_with_digest(
        &mut self,
        reason: &str,
        digest: Option<&str>,
    ) -> std::result::Result<String, String> {
        self.recover(reason, true, digest)
    }

    fn recover(
        &mut self,
        reason: &str,
        plan: bool,
        digest: Option<&str>,
    ) -> std::result::Result<String, String> {
        if self.used || self.total_recoveries >= MAX_RECOVERIES_PER_TURN {
            return Err(format!(
                "Stopped: reasoning gate hit again ({reason}) after its one recovery attempt. No partial tool call was executed. Adjust [reasoning_guard] or the model's reasoning setting before retrying."
            ));
        }
        self.used = true;
        self.total_recoveries = self.total_recoveries.saturating_add(1);
        let summary = digest
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                format!("\nEarlier reasoning before cutoff (do not re-derive from scratch): {s}")
            })
            .unwrap_or_default();
        if plan {
            return Ok(format!(
                "Reasoning gate: {reason}. The generation was discarded and no tool from it ran. Write your thinking down as a todo plan before anything else: call todo with 1-3 concrete unresolved questions (investigation=true) plus the known next steps. Only todo is offered until a plan exists. Then act on the first item; do not restart broad planning in hidden reasoning.{summary}"
            ));
        }
        Ok(format!(
            "Reasoning gate: {reason}. The generation was discarded and no tool from it ran. Take the next small, useful tool action now, or answer directly. Limit reasoning to the concrete uncertainty that changes that action; do not restart broad planning.{summary}"
        ))
    }
}
/// The watcher also handles silence and user cancellation when no stream callbacks arrive.
/// Answer/argument deltas disarm the pre-action deadline, but not the reasoning-byte cap, and never
/// execute a tool; normal completion is still required by the caller.
/// Streamed callbacks for one guarded chat call.
pub(in crate::agent) struct Sinks<'a> {
    pub on_text: &'a mut dyn FnMut(&str) -> bool,
    pub on_notice: &'a mut dyn FnMut(&str),
    pub on_event: &'a mut dyn FnMut(&StreamEvent) -> bool,
}

pub(in crate::agent) struct GuardSignals {
    pub interrupt: Arc<AtomicBool>,
    pub wrap_up: Option<Arc<AtomicBool>>,
}

pub(in crate::agent) fn chat_with_wrap_up(
    backend: &mut Backend,
    adapter: Option<&Path>,
    request: &Chat,
    timeout: u64,
    signals: GuardSignals,
    config: &ReasoningGuardConfig,
    sinks: Sinks<'_>,
) -> Result<Reply> {
    let GuardSignals { interrupt, wrap_up } = signals;
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
        let (watch_interrupt, watch_wrap_up, watch_state, watch_cancel) =
            (&interrupt, &wrap_up, &state, &cancel);
        let watcher = scope.spawn(move || {
            loop {
                match wait.recv_timeout(Duration::from_millis(20)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if watch_interrupt.load(Ordering::SeqCst) {
                    watch_cancel.store(true, Ordering::SeqCst);
                    break;
                }
                if watch_wrap_up
                    .as_ref()
                    .is_some_and(|flag| flag.swap(false, Ordering::SeqCst))
                {
                    let mut gate = watch_state.lock();
                    if gate.hit.is_none() {
                        gate.hit =
                            Some("user requested immediate action (wrapped up thinking)".into());
                        gate.user_wrap_up = true;
                    }
                    watch_cancel.store(true, Ordering::SeqCst);
                    break;
                }
                if !watch_state.lock().check(config) {
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
                    // Activity keeps the silence watchdog alive, not the pre-action deadline.
                    StreamEvent::Activity => gate.check(config),
                    StreamEvent::HiddenText(_) => {
                        gate.begin_generation();
                        gate.check(config)
                    }
                    StreamEvent::Reasoning(text) => {
                        gate.begin_generation();
                        gate.reasoning_bytes = gate.reasoning_bytes.saturating_add(text.len());
                        gate.record_prefix(text);
                        !gate.repeated(text, true) && gate.check(config)
                    }
                    StreamEvent::ToolArguments { delta, .. } => {
                        gate.tools_started = true;
                        gate.output(config, !delta.trim().is_empty())
                    }
                    StreamEvent::ToolCallComplete { arguments, .. } => {
                        gate.tools_started = true;
                        gate.output(config, !arguments.trim().is_empty())
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
    if gate.user_wrap_up {
        return Err(GateHit::user_wrap_up(gate.prefix_digest()).into());
    }
    if let Some(reason) = &gate.hit {
        // A repetition hit's prefix is the looping block itself; replaying it would re-inject the
        // failed content, so only non-repetitive cutoffs keep a digest.
        let digest = if GateHit::new(reason.clone(), None).is_repetition() {
            None
        } else {
            gate.prefix_digest()
        };
        return Err(GateHit::new(reason.clone(), digest).into());
    }
    result
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod plan_tests;

#[cfg(test)]
mod repetition_tests;
