//! Controller-owned automatic maintenance. Workers prepare immutable snapshots and never journal cuts.
use super::{
    Ui,
    compaction::{
        self, CompactionConfig, CompactionIdentity, CompactionMethod, CompactionOptions,
        CompactionOutcome, CompactionReason, CompactionSnapshot, CompactionStrategy,
        PreparedCompaction,
    },
};
use crate::llm::{self, Chat, provider::Backend, usage::Scope};
use anyhow::{Result, ensure};
use std::{
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// An owned form of the ordinary child-request portable-guidance loop. It deliberately has no
/// access to the live Session or normal-turn interrupt; interceptions can only regenerate this
/// summary request. Native provider replay remains on WorkerBackends::main.
pub fn portable_summary_completion(
    config: crate::config::types::Config,
    layout: Option<crate::storage::paths::Layout>,
    root: std::path::PathBuf,
    control_state: serde_json::Value,
    hooks_config: Vec<crate::config::types::HookConfig>,
    hook_plugins: Option<Arc<crate::tools::plugin::Host>>,
    session_id: String,
    hermetic: bool,
) -> impl FnMut(&mut Backend, &Chat, &mut dyn Ui, Arc<AtomicBool>, u64) -> Result<String> + Send {
    let telemetry_config = config.clone();
    let selection = config.selection();
    let service_tier = config.effective_service_tier().to_owned();
    let extended_context = config.extended_context;
    let portable = config.portable_rules;
    let recovery = config.request_recovery;
    let mut state = super::rules::State::restore(&control_state);
    state.begin(
        &root,
        control_state["current_instruction"].as_str().unwrap_or(""),
    );
    state.set_regenerations(0);
    state.clear_callback();
    let mut regenerations = 0u32;
    move |backend, source, ui, cancel, timeout| {
        backend.require_session_identity()?;
        let catalog = if portable.enabled {
            layout
                .as_ref()
                .map(|layout| super::rules::Catalog::discover(layout, &root))
                .unwrap_or_default()
        } else {
            super::rules::Catalog::default()
        };
        for warning in &catalog.warnings {
            ui.notice(warning);
        }
        let expires = Instant::now()
            .checked_add(Duration::from_secs(timeout))
            .ok_or_else(|| anyhow::anyhow!("compaction summary deadline overflow"))?;
        let mut request = source.clone();
        let profile = backend.compaction_summary_profile(request.max_tokens);
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(llm::Interrupted.into());
            }
            let remaining = expires.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "compaction summary deadline expired");
            request
                .messages
                .retain(|message| message["dial_portable_rules"] != true);
            if let Some(guidance) = catalog.guidance(&mut state, "compaction") {
                request.messages.push(guidance);
            }
            let generation_cancel = Arc::new(AtomicBool::new(false));
            let mut monitor = catalog.monitor(&root, &state, "compaction");
            backend.next_request_profile(crate::llm::profiles::request_tiers_for(
                profile.clone(),
                &selection,
                &service_tier,
                extended_context,
            )?);
            let rewritten = super::hooks::provider_request(
                &hooks_config,
                hook_plugins.as_deref(),
                &root,
                &request,
                &cancel,
                &mut |notice| ui.notice(notice),
            )?;
            let started = Instant::now();
            let result = super::chat_with_ui_inner(
                backend,
                None,
                rewritten.as_ref().unwrap_or(&request),
                super::ChatOptions {
                    timeout: remaining.as_secs().max(1),
                    render_text: false,
                    context_tokens: None,
                    policy: None,
                    recovery: Some(&recovery),
                },
                generation_cancel.clone(),
                ui,
                &mut |_| {
                    if cancel.load(Ordering::SeqCst) {
                        generation_cancel.store(true, Ordering::SeqCst);
                    }
                },
                Some(&mut monitor),
            );
            match &result {
                Ok(reply) => crate::telemetry::request(
                    &telemetry_config,
                    hermetic,
                    &session_id,
                    backend.model_label(),
                    reply,
                    backend.reply_cost(reply),
                    started.elapsed(),
                ),
                Err(error) => crate::telemetry::request_failed(
                    &telemetry_config,
                    hermetic,
                    &session_id,
                    backend.model_label(),
                    started.elapsed(),
                    error,
                ),
            }
            if cancel.load(Ordering::SeqCst) {
                return Err(llm::Interrupted.into());
            }
            let hit = result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<super::rules::Hit>())
                .cloned();
            let paths = result.as_ref().ok().and_then(|reply| {
                catalog.pending_paths(&root, &state, "compaction", &reply.tool_calls)
            });
            if hit.is_some() || paths.is_some() {
                ensure!(
                    regenerations < portable.max_regenerations,
                    "portable guidance regeneration limit reached; compaction was not committed"
                );
                regenerations += 1;
                state.set_regenerations(regenerations);
                if let Some(hit) = hit {
                    state
                        .remember(&hit, "compaction")
                        .map_err(anyhow::Error::msg)?;
                    ui.notice(&hit.to_string());
                }
                if let Some(paths) = paths {
                    state.activate(paths).map_err(anyhow::Error::msg)?;
                }
                ui.inference_discarded(
                    "portable guidance intercepted the background compaction summary",
                );
                continue;
            }
            let reply = result?;
            ensure!(
                reply.finish_reason != "length"
                    && reply.tool_calls.is_empty()
                    && !reply.text.trim().is_empty(),
                "compaction response was truncated, empty or contained tool calls; original context is unchanged"
            );
            return Ok(reply.text);
        }
    }
}

pub const SPECULATION_LEAD_MIN_TOKENS: u64 = 8_192;
const SPECULATION_LEAD_MAX_TOKENS: u64 = 32_000;
const PROGRESS_CAPACITY: usize = 32;
const PROGRESS_BYTES: usize = 4_096;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAINTENANCE_TIMEOUT_SECS: u64 = 300;

fn maintenance_timeout(configured: u64) -> u64 {
    if configured == 0 {
        MAINTENANCE_TIMEOUT_SECS
    } else {
        configured.min(MAINTENANCE_TIMEOUT_SECS)
    }
}

pub fn speculation_lead(threshold: u64) -> u64 {
    (threshold / 8).clamp(SPECULATION_LEAD_MIN_TOKENS, SPECULATION_LEAD_MAX_TOKENS)
}

fn policy_methods(policy: &CompactionConfig) -> &[CompactionMethod] {
    match policy.strategy {
        CompactionStrategy::ContextFull => &policy.method_order,
        CompactionStrategy::Handoff => &[CompactionMethod::Handoff],
        CompactionStrategy::Shake => &[CompactionMethod::Shake],
        CompactionStrategy::Snapcompact => &[CompactionMethod::Snapcompact],
        CompactionStrategy::Off => &[],
    }
}

/// Separate sticky/cache identities for the side request and the live model stream.
pub fn side_request_namespace(session_id: &str) -> Result<String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let serial = NEXT
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
        .map_err(|_| anyhow::anyhow!("background compaction namespace counter overflow"))?;
    Ok(format!(
        "{session_id}:maintenance:{}:{serial}",
        std::process::id()
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkKind {
    Speculative,
    Idle,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Launch {
    Started,
    Busy,
    Disabled,
    UnsupportedLocal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpeculationAction {
    None,
    Start(CompactionMethod),
    Refresh(CompactionMethod),
    Defer,
    Claim,
    Blocking,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PollState {
    Inactive,
    Running,
    Armed,
    Failed(String),
    Cancelled,
    TimedOut,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettlementState {
    Prepared,
    Failed(String),
    Cancelled,
    TimedOut,
    Panicked,
}

/// The ledger is bound before launch, so accepted usage survives cancellation, stale results,
/// controller/session destruction and a late worker completion. Snapshotting this scope is for
/// presentation only: callers must not add its cumulative counts to their ledger again.
pub struct WorkerSettlement {
    pub session_id: String,
    pub branch_leaf: Option<String>,
    pub request_signature: String,
    pub provider_namespace: Option<String>,
    pub kind: WorkKind,
    pub state: SettlementState,
    pub usage_scope: Scope,
}

/// Publish settlement to the original session's execution ledger, including discarded requests.
/// A parked session retains this Body; the ordinary settled-runtime journal flush persists it.
pub fn settle_into_runtime(body: crate::runtime::Body) -> impl FnOnce(WorkerSettlement) + Send {
    move |settlement| {
        let (total, turn) = settlement.usage_scope.snapshot();
        let phase = match &settlement.state {
            SettlementState::Prepared => crate::runtime::Phase::Completed,
            SettlementState::Cancelled | SettlementState::TimedOut => {
                crate::runtime::Phase::Interrupted
            }
            SettlementState::Failed(_) | SettlementState::Panicked => crate::runtime::Phase::Failed,
        };
        body.settle(format!("Background compaction: {:?}", settlement.state), Some(serde_json::json!({
            "session_id": settlement.session_id, "branch_leaf": settlement.branch_leaf,
            "request_signature": settlement.request_signature, "kind": format!("{:?}", settlement.kind),
            "provider_namespace": settlement.provider_namespace,
            "state": format!("{:?}", settlement.state), "usage_total": total, "usage_turn": turn,
            "usage_coverage": "cumulative original-agent ledger; not an additional billable delta",
        })), phase);
    }
}

/// Native compaction always uses `main`; only portable summaries use the optional role backend.
/// These are side-request backends, not the resident backend borrowed by the ordinary turn.
pub struct WorkerBackends {
    pub main: Backend,
    pub summary: Option<Backend>,
}

/// Preserve the real shared ledger across a same-session model/config actor rebuild. Detached
/// cancelled requests can settle after the old Agent is gone; copying a scalar cost loses them.
pub(crate) struct RuntimeAccounting {
    session_id: String,
    scope: Scope,
    cost_base: f64,
}

struct Registration {
    cancel: Weak<AtomicBool>,
    kind: WorkKind,
}
/// Safe to give to an RPC/ACP input controller. This never cancels the normal turn or children.
#[derive(Clone, Default)]
pub struct CompactionControl {
    active: Arc<Mutex<Option<Registration>>>,
    last_input: Arc<Mutex<Option<Instant>>>,
}
impl CompactionControl {
    pub fn note_input(&self) {
        self.record_input(Instant::now());
    }
    fn record_input(&self, now: Instant) {
        *self.last_input.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
        self.cancel_idle();
    }
    pub(crate) fn input_time(&self) -> Option<Instant> {
        *self.last_input.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn cancel(&self) {
        if let Some(flag) = self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|run| run.cancel.upgrade())
        {
            flag.store(true, Ordering::SeqCst);
        }
    }
    pub fn cancel_idle(&self) {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flag) = active
            .as_ref()
            .filter(|run| run.kind == WorkKind::Idle)
            .and_then(|run| run.cancel.upgrade())
        {
            flag.store(true, Ordering::SeqCst);
        }
    }
    fn bind(&self, cancel: &Arc<AtomicBool>, kind: WorkKind) {
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = Some(Registration {
            cancel: Arc::downgrade(cancel),
            kind,
        });
    }
    fn clear(&self, cancel: &Arc<AtomicBool>) {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if active
            .as_ref()
            .is_some_and(|run| run.cancel.ptr_eq(&Arc::downgrade(cancel)))
        {
            *active = None;
        }
    }
}

/// Foreground local idle preparation uses the actor's compute lease, but not its normal-turn flag.
struct ForegroundIdle {
    control: CompactionControl,
    cancel: Arc<AtomicBool>,
    deadline: Deadline,
}
impl Drop for ForegroundIdle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.control.clear(&self.cancel);
        self.deadline.finish();
    }
}

#[derive(Default, Clone, Copy)]
pub struct IdleActivity {
    pub queued_prompt: bool,
    pub model_running: bool,
    pub tool_running: bool,
    /// Includes child/background tasks which can wake the owning turn, not just maintenance.
    pub live_worker: bool,
}
impl IdleActivity {
    pub fn busy(self) -> bool {
        self.queued_prompt || self.model_running || self.tool_running || self.live_worker
    }
}

pub struct IdleClock {
    last_activity: Instant,
}
impl Default for IdleClock {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}
impl IdleClock {
    pub fn new(now: Instant) -> Self {
        Self { last_activity: now }
    }
    pub fn activity(&mut self, now: Instant) {
        self.last_activity = now;
    }
    pub fn due(
        &mut self,
        policy: &CompactionConfig,
        now: Instant,
        tokens: u64,
        activity: IdleActivity,
    ) -> bool {
        if activity.busy() {
            self.activity(now);
            return false;
        }
        if !self.would_be_due(policy, now, tokens, activity) {
            return false;
        }
        // One attempt per delay, including failures and unsupported parallel backends. A controller
        // may perform the synchronous local fallback without spinning on an already-expired timer.
        self.activity(now);
        true
    }
    /// Non-consuming actor probe: acquire foreground compute only when a local idle pass is due.
    pub fn would_be_due(
        &self,
        policy: &CompactionConfig,
        now: Instant,
        tokens: u64,
        activity: IdleActivity,
    ) -> bool {
        !activity.busy()
            && policy.enabled
            && policy.idle_enabled
            && policy.strategy != CompactionStrategy::Off
            && (policy.strategy != CompactionStrategy::ContextFull
                || !policy.method_order.is_empty())
            && tokens >= policy.idle_threshold_tokens
            && now.saturating_duration_since(self.last_activity)
                >= Duration::from_secs(policy.idle_timeout_seconds)
    }
    fn remaining(&self, policy: &CompactionConfig, now: Instant) -> Option<Duration> {
        (policy.enabled
            && policy.idle_enabled
            && policy.strategy != CompactionStrategy::Off
            && (policy.strategy != CompactionStrategy::ContextFull
                || !policy.method_order.is_empty()))
        .then(|| {
            Duration::from_secs(policy.idle_timeout_seconds)
                .saturating_sub(now.saturating_duration_since(self.last_activity))
                .max(POLL_INTERVAL)
        })
    }
}

pub struct SpeculationContext<'a> {
    pub policy: &'a CompactionConfig,
    pub identity: CompactionIdentity<'a>,
    pub session_id: &'a str,
    pub window: u32,
    pub tokens: u64,
    pub mid_turn: bool,
    pub image_input: bool,
    /// Immutable prefix (system, developer, tools, transient guidance) of the next request.
    pub fixed_tokens: u64,
    /// False for local/shared compute or a hook that requires exact blocking ownership.
    pub parallel_supported: bool,
}

#[derive(Clone, Eq, PartialEq)]
struct NativeFailureKey {
    session_id: String,
    provider: String,
    model: String,
}
impl NativeFailureKey {
    fn matches(&self, session: &str, identity: CompactionIdentity<'_>) -> bool {
        self.session_id == session
            && self.provider == identity.provider
            && self.model == identity.model
    }
    fn snapshot(snapshot: &CompactionSnapshot) -> Self {
        Self {
            session_id: snapshot.session_id.clone(),
            provider: snapshot.model_selection.provider.clone(),
            model: snapshot.model_selection.model.clone(),
        }
    }
}

enum Progress {
    Notice(String),
    Diagnostic(String),
}
struct WorkerUi {
    progress: SyncSender<Progress>,
}
fn bounded_text(text: &str) -> String {
    let mut end = text.len().min(PROGRESS_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
impl Ui for WorkerUi {
    fn notice(&mut self, text: &str) {
        let _ = self.progress.try_send(Progress::Notice(bounded_text(text)));
    }
    fn background_notice(&mut self, text: &str) {
        self.notice(text);
    }
    fn diagnostic(&mut self, text: &str) {
        let _ = self
            .progress
            .try_send(Progress::Diagnostic(bounded_text(text)));
    }
}

/// The deadline watcher never holds Backend's transport or the controller's compute mutex.
/// Completion wakes it immediately instead of retaining a sleeping thread for the entire timeout.
struct Deadline {
    done: Arc<(Mutex<bool>, Condvar)>,
    timed_out: Arc<AtomicBool>,
    at: Instant,
}
impl Deadline {
    fn start(cancel: Arc<AtomicBool>, timeout: Duration) -> Result<Self> {
        let at = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| anyhow::anyhow!("compaction timeout overflow"))?;
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let timed_out = Arc::new(AtomicBool::new(false));
        let watcher_done = done.clone();
        let watcher_timeout = timed_out.clone();
        thread::Builder::new()
            .name("dial-compaction-deadline".into())
            .spawn(move || {
                let (mutex, changed) = &*watcher_done;
                let guard = mutex.lock().unwrap_or_else(|e| e.into_inner());
                let (guard, _) = changed
                    .wait_timeout_while(
                        guard,
                        at.saturating_duration_since(Instant::now()),
                        |done| !*done,
                    )
                    .unwrap_or_else(|e| e.into_inner());
                if !*guard {
                    watcher_timeout.store(true, Ordering::SeqCst);
                    cancel.store(true, Ordering::SeqCst);
                }
            })?;
        Ok(Self {
            done,
            timed_out,
            at,
        })
    }
    fn finish(&self) {
        let (done, changed) = &*self.done;
        *done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        changed.notify_all();
    }
}
impl Drop for Deadline {
    fn drop(&mut self) {
        self.finish();
    }
}

struct WorkerResult {
    prepared: Result<PreparedCompaction>,
    permanent_native_failure: bool,
}
struct Slot {
    snapshot: Arc<CompactionSnapshot>,
    options: CompactionOptions,
    kind: WorkKind,
    context_tokens_at_start: u64,
    cancel: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    progress: Receiver<Progress>,
    result: Receiver<WorkerResult>,
    worker: Option<JoinHandle<()>>,
    armed: Option<PreparedCompaction>,
}

pub struct CompactionRuntime {
    slot: Option<Slot>,
    // A cancelled transport may still be blocked. Retain the one-worker occupancy until it exits;
    // never launch another speculative worker alongside it and never synchronously join it.
    retiring: Option<JoinHandle<()>>,
    control: CompactionControl,
    idle: IdleClock,
    failed_native: Option<NativeFailureKey>,
    closed: bool,
}
impl Default for CompactionRuntime {
    fn default() -> Self {
        Self {
            slot: None,
            retiring: None,
            control: Default::default(),
            idle: Default::default(),
            failed_native: None,
            closed: false,
        }
    }
}
impl CompactionRuntime {
    pub fn control(&self) -> CompactionControl {
        self.control.clone()
    }
    pub fn has_slot(&self) -> bool {
        self.slot.is_some()
    }
    pub fn is_armed(&self) -> bool {
        self.slot.as_ref().is_some_and(|slot| slot.armed.is_some())
    }
    pub fn has_live_worker(&self) -> bool {
        self.retiring
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            || self.slot.as_ref().is_some_and(|slot| {
                slot.worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished())
            })
    }
    pub fn kind(&self) -> Option<WorkKind> {
        self.slot.as_ref().map(|slot| slot.kind)
    }
    pub fn note_user_input(&mut self, now: Instant) {
        self.control.record_input(now);
        self.idle.activity(now);
        if self.kind() == Some(WorkKind::Idle) {
            self.cancel();
        }
    }
    pub fn idle_due(
        &mut self,
        policy: &CompactionConfig,
        now: Instant,
        tokens: u64,
        mut activity: IdleActivity,
    ) -> bool {
        if self.closed {
            return false;
        }
        if let Some(input) = self.control.input_time() {
            self.idle.last_activity = self.idle.last_activity.max(input);
        }
        if activity.queued_prompt && self.kind() == Some(WorkKind::Idle) {
            self.cancel();
        }
        activity.live_worker |= self.has_live_worker();
        self.idle.due(policy, now, tokens, activity)
    }
    pub fn idle_pending(
        &self,
        policy: &CompactionConfig,
        now: Instant,
        tokens: u64,
        mut activity: IdleActivity,
    ) -> bool {
        activity.live_worker |= self.has_live_worker();
        let idle = IdleClock {
            last_activity: self
                .control
                .input_time()
                .map_or(self.idle.last_activity, |input| {
                    self.idle.last_activity.max(input)
                }),
        };
        !self.closed && idle.would_be_due(policy, now, tokens, activity)
    }
    pub fn next_wakeup(&self, policy: &CompactionConfig, now: Instant) -> Option<Duration> {
        let idle = IdleClock {
            last_activity: self
                .control
                .input_time()
                .map_or(self.idle.last_activity, |input| {
                    self.idle.last_activity.max(input)
                }),
        };
        if self.has_live_worker() {
            Some(POLL_INTERVAL)
        } else {
            idle.remaining(policy, now)
        }
    }
    pub fn on_committed(&mut self) {
        self.failed_native = None;
        self.idle.activity(Instant::now());
    }
    fn foreground_idle(&mut self, timeout_secs: u64) -> Result<ForegroundIdle> {
        self.reap_retiring();
        ensure!(
            !self.closed && self.slot.is_none() && self.retiring.is_none(),
            "foreground idle compaction requires exclusive maintenance ownership"
        );
        ensure!(
            timeout_secs > 0,
            "idle compaction requires a positive deadline"
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let deadline = Deadline::start(cancel.clone(), Duration::from_secs(timeout_secs))?;
        self.control.bind(&cancel, WorkKind::Idle);
        Ok(ForegroundIdle {
            control: self.control.clone(),
            cancel,
            deadline,
        })
    }
    pub fn reset(&mut self) {
        self.control.cancel();
        self.cancel();
        self.failed_native = None;
        self.idle.activity(Instant::now());
    }
    fn reap_retiring(&mut self) {
        if self.retiring.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(worker) = self.retiring.take() {
                let _ = worker.join();
            }
        }
    }
    fn release(&mut self, mut slot: Slot) {
        self.control.clear(&slot.cancel);
        if let Some(worker) = slot.worker.take() {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                self.retiring = Some(worker);
            }
        }
    }
    pub fn cancel(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.cancel.store(true, Ordering::SeqCst);
            self.release(slot);
        }
        self.reap_retiring();
    }
    pub fn shutdown(&mut self) {
        self.closed = true;
        self.control.cancel();
        self.cancel();
        // Dropping a JoinHandle detaches; the independent deadline/cancel and owned ledger survive.
        // Shutdown is bounded even when an upstream transport or settlement callback is blocked.
        drop(self.retiring.take());
    }

    pub fn speculation_action(&self, context: SpeculationContext<'_>) -> Result<SpeculationAction> {
        let policy = context.policy;
        if self.closed
            || !context.parallel_supported
            || !policy.enabled
            || !policy.async_enabled
            || policy.experimental_context_management
            || policy.strategy == CompactionStrategy::Off
            || (context.mid_turn && context.identity.agent.is_none() && !policy.mid_turn_enabled)
            || context.window <= 1
        {
            return Ok(SpeculationAction::None);
        }
        let skip_remote = self
            .failed_native
            .as_ref()
            .is_some_and(|key| key.matches(context.session_id, context.identity));
        let order = policy_methods(policy);
        let focused = policy
            .summary_instructions
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty());
        let method = order.iter().copied().find(|method| match method {
            CompactionMethod::Remote => {
                !skip_remote
                    && policy.remote_enabled
                    && (policy.remote_endpoint.is_some()
                        || policy.native_capability(context.identity).is_some())
            }
            CompactionMethod::Snapcompact => context.image_input && !focused,
            CompactionMethod::Shake => !focused,
            _ => true,
        });
        let Some(
            method
            @ (CompactionMethod::Remote | CompactionMethod::Handoff | CompactionMethod::Soft),
        ) = method
        else {
            return Ok(SpeculationAction::None);
        };
        let threshold = policy.effective_threshold(context.identity, context.window)?;
        let lead = speculation_lead(threshold);
        if context.tokens < threshold {
            if threshold - context.tokens > lead {
                return Ok(SpeculationAction::None);
            }
            if let Some(slot) = &self.slot {
                if slot.kind != WorkKind::Speculative || slot.armed.is_none() {
                    return Ok(SpeculationAction::None);
                }
                let growth = context.tokens.saturating_sub(slot.context_tokens_at_start);
                let native_anthropic = slot.armed.as_ref().is_some_and(|prepared| {
                    prepared
                        .prefix
                        .iter()
                        .any(|message| message["dial_anthropic_compaction"].is_object())
                });
                let keep_recent = policy.keep_recent_budget(
                    context.identity,
                    context.window,
                    context.fixed_tokens,
                )?;
                if growth > keep_recent.max(SPECULATION_LEAD_MIN_TOKENS) && !native_anthropic {
                    return Ok(SpeculationAction::Refresh(method));
                }
                return Ok(SpeculationAction::None);
            }
            return Ok(if self.retiring.is_some() {
                SpeculationAction::None
            } else {
                SpeculationAction::Start(method)
            });
        }
        if self.is_armed() {
            return Ok(SpeculationAction::Claim);
        }
        let grace = threshold.saturating_add(lead).min(
            u64::from(context.window).saturating_sub(u64::from(policy.reserve(context.window))),
        );
        if context.tokens >= grace {
            return Ok(SpeculationAction::Blocking);
        }
        if let Some(slot) = &self.slot {
            return Ok(
                if slot.kind == WorkKind::Speculative && !slot.cancel.load(Ordering::SeqCst) {
                    SpeculationAction::Defer
                } else {
                    SpeculationAction::Blocking
                },
            );
        }
        Ok(if self.retiring.is_some() {
            SpeculationAction::Blocking
        } else {
            SpeculationAction::Start(method)
        })
    }

    /// `complete` is the owner's real portable-rule/role-model callback. It must obey the supplied
    /// independent cancel and remaining deadline, and reject tool calls/truncated summary replies.
    /// `settled` may publish accounting, never mutate the live branch. It runs even after Drop.
    pub fn start<C, L>(
        &mut self,
        mut backends: WorkerBackends,
        mut snapshot: CompactionSnapshot,
        kind: WorkKind,
        context_tokens: u64,
        mut options: CompactionOptions,
        usage_scope: Scope,
        mut complete: C,
        settled: L,
    ) -> Result<Launch>
    where
        C: FnMut(&mut Backend, &Chat, &mut dyn Ui, Arc<AtomicBool>, u64) -> Result<String>
            + Send
            + 'static,
        L: FnOnce(WorkerSettlement) + Send + 'static,
    {
        self.reap_retiring();
        if self.closed
            || !snapshot.policy.enabled
            || snapshot.policy.strategy == CompactionStrategy::Off
            || snapshot.policy.experimental_context_management
            || (kind == WorkKind::Speculative && !snapshot.policy.async_enabled)
            || (kind == WorkKind::Idle && !snapshot.policy.idle_enabled)
        {
            return Ok(Launch::Disabled);
        }
        if self.slot.is_some() || self.retiring.is_some() {
            return Ok(Launch::Busy);
        }
        // Even loopback endpoints may share the resident server/global compute lease. No parallel
        // local launch is claimed; ordinary threshold or foreground-owned idle maintenance wins.
        if backends.main.local_inference()
            || backends
                .summary
                .as_ref()
                .is_some_and(Backend::local_inference)
        {
            return Ok(Launch::UnsupportedLocal);
        }
        snapshot.policy.validate()?;
        if kind == WorkKind::Speculative {
            if !matches!(
                options.method,
                Some(CompactionMethod::Remote | CompactionMethod::Handoff | CompactionMethod::Soft)
            ) {
                return Ok(Launch::Disabled);
            }
            options.require_method = true;
        }
        // Ordinary worker_timeout_secs=0 is unlimited. Automatic maintenance always owns a bounded
        // deadline, matching the native transport's five-minute ceiling; smaller limits still win.
        snapshot.timeout_secs = maintenance_timeout(snapshot.timeout_secs);
        ensure!(
            snapshot.branch_leaf.is_some(),
            "background compaction needs a durable branch owner"
        );
        options.reason = if kind == WorkKind::Idle {
            CompactionReason::Idle
        } else {
            CompactionReason::Speculative
        };
        if kind == WorkKind::Idle {
            options.suppress_continuation = true;
        }
        let request_signature = compaction::snapshot_signature(
            &snapshot.native_request,
            snapshot.context().identity,
            &snapshot.policy,
        )?;
        let cancel = Arc::new(AtomicBool::new(false));
        let deadline = Deadline::start(cancel.clone(), Duration::from_secs(snapshot.timeout_secs))?;
        let timed_out = deadline.timed_out.clone();
        let (progress_tx, progress_rx) = mpsc::sync_channel(PROGRESS_CAPACITY);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let snapshot = Arc::new(snapshot);
        let worker_snapshot = snapshot.clone();
        let worker_options = options.clone();
        let worker_cancel = cancel.clone();
        let ledger_scope = usage_scope
            .worker()
            .for_purpose(llm::usage::Purpose::Compaction);
        backends.main.set_usage_scope(Some(ledger_scope.clone()));
        if let Some(summary) = &mut backends.summary {
            summary.set_usage_scope(Some(ledger_scope.clone()));
        }
        let worker = thread::Builder::new()
            .name("dial-compaction".into())
            .spawn(move || {
                let mut ui = WorkerUi {
                    progress: progress_tx,
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut callback = |backend: &mut Backend, request: &Chat, ui: &mut dyn Ui| {
                        if worker_cancel.load(Ordering::SeqCst) {
                            return Err(llm::Interrupted.into());
                        }
                        let remaining = deadline.at.saturating_duration_since(Instant::now());
                        ensure!(
                            !remaining.is_zero(),
                            "background compaction deadline expired"
                        );
                        complete(
                            backends.summary.as_mut().unwrap_or(backend),
                            request,
                            ui,
                            worker_cancel.clone(),
                            remaining.as_secs().max(1),
                        )
                    };
                    compaction::compact_history(
                        &mut backends.main,
                        &worker_snapshot.source,
                        &worker_snapshot.policy,
                        worker_snapshot.context(),
                        &worker_options,
                        worker_cancel.clone(),
                        &mut ui,
                        &mut callback,
                        None,
                    )
                }));
                let panicked = result.is_err();
                let prepared =
                    result.unwrap_or_else(|_| Err(anyhow::anyhow!("compaction worker panicked")));
                let native_route = worker_options.method == Some(CompactionMethod::Remote)
                    && worker_snapshot.policy.remote_endpoint.is_none()
                    && worker_snapshot
                        .policy
                        .native_capability(worker_snapshot.context().identity)
                        .is_some();
                let permanent_native_failure = native_route
                    && !worker_cancel.load(Ordering::SeqCst)
                    && prepared
                        .as_ref()
                        .err()
                        .and_then(|error| {
                            error.downcast_ref::<compaction::NativeCompactionFailure>()
                        })
                        .is_some_and(|failure| !llm::is_transient(&failure.cause));
                let state = if deadline.timed_out.load(Ordering::SeqCst) {
                    SettlementState::TimedOut
                } else if worker_cancel.load(Ordering::SeqCst) {
                    SettlementState::Cancelled
                } else if panicked {
                    SettlementState::Panicked
                } else {
                    match &prepared {
                        Ok(_) => SettlementState::Prepared,
                        Err(error) => SettlementState::Failed(format!("{error:#}")),
                    }
                };
                // The Backend already recorded each logical/transport request into this owned scope.
                // Never withhold settlement because the result is stale, cancelled or its receiver died.
                let settlement = WorkerSettlement {
                    session_id: worker_snapshot.session_id.clone(),
                    branch_leaf: worker_snapshot.branch_leaf.clone(),
                    request_signature,
                    provider_namespace: worker_snapshot
                        .main_config
                        .provider_cache_namespace
                        .clone(),
                    kind,
                    state,
                    usage_scope: ledger_scope,
                };
                let settlement_ok =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| settled(settlement)))
                        .is_ok();
                deadline.finish();
                let prepared = if settlement_ok {
                    prepared
                } else {
                    Err(anyhow::anyhow!(
                        "compaction accounting settlement callback panicked"
                    ))
                };
                // A distinct one-result channel cannot be starved by progress. Neither send blocks.
                let _ = result_tx.try_send(WorkerResult {
                    prepared,
                    permanent_native_failure,
                });
            })?;
        self.control.bind(&cancel, kind);
        self.slot = Some(Slot {
            snapshot,
            options,
            kind,
            context_tokens_at_start: context_tokens,
            cancel,
            timed_out,
            progress: progress_rx,
            result: result_rx,
            worker: Some(worker),
            armed: None,
        });
        self.idle.activity(Instant::now());
        Ok(Launch::Started)
    }

    pub fn poll(&mut self, ui: &mut dyn Ui) -> PollState {
        self.reap_retiring();
        let Some(slot) = self.slot.as_mut() else {
            return PollState::Inactive;
        };
        for progress in slot.progress.try_iter().take(PROGRESS_CAPACITY) {
            match progress {
                Progress::Notice(text) => ui.background_notice(&text),
                Progress::Diagnostic(text) => ui.diagnostic(&text),
            }
        }
        if slot.cancel.load(Ordering::SeqCst) {
            let state = if slot.timed_out.load(Ordering::SeqCst) {
                PollState::TimedOut
            } else {
                PollState::Cancelled
            };
            self.cancel();
            return state;
        }
        if slot.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(worker) = slot.worker.take() {
                let _ = worker.join();
            }
        }
        if slot.armed.is_some() {
            return PollState::Armed;
        }
        match slot.result.try_recv() {
            Ok(result) => {
                if result.permanent_native_failure {
                    self.failed_native = Some(NativeFailureKey::snapshot(&slot.snapshot));
                }
                match result.prepared {
                    Ok(prepared) => {
                        slot.armed = Some(prepared);
                        PollState::Armed
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        if let Some(slot) = self.slot.take() {
                            self.release(slot);
                        }
                        PollState::Failed(message)
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => PollState::Running,
            Err(mpsc::TryRecvError::Disconnected) => {
                if let Some(slot) = self.slot.take() {
                    self.release(slot);
                }
                PollState::Failed("compaction worker ended without a result".into())
            }
        }
    }

    /// The controller supplies the atomic live ancestor/prefix/latest-projection gate. There is no
    /// public take-and-commit-later result: the owned candidate is consumed and checked in this call.
    /// Failed/stale gates return an error without a journal cut; callers can run blocking maintenance.
    pub fn claim_armed<F>(
        &mut self,
        reason: CompactionReason,
        live_backend: Option<&mut Backend>,
        commit: F,
    ) -> Result<Option<CompactionOutcome>>
    where
        F: FnOnce(
            &CompactionSnapshot,
            PreparedCompaction,
            &CompactionOptions,
            Option<&mut Backend>,
        ) -> Result<CompactionOutcome>,
    {
        if !self.is_armed() {
            return Ok(None);
        }
        let Some(mut slot) = self.slot.take() else {
            return Ok(None);
        };
        // Cancellation and ownership transfer are serialized with the external control handle.
        // Once the slot is claimed it is foreground maintenance, no longer a cancellable worker.
        let cancelled = {
            let mut active = self
                .control
                .active
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let cancelled = slot.cancel.load(Ordering::SeqCst);
            if active
                .as_ref()
                .is_some_and(|run| run.cancel.ptr_eq(&Arc::downgrade(&slot.cancel)))
            {
                *active = None;
            }
            cancelled
        };
        if cancelled {
            self.release(slot);
            return Ok(None);
        }
        let prepared = slot.armed.take().expect("armed compaction slot");
        let mut options = slot.options.clone();
        options.reason = reason;
        if reason == CompactionReason::Idle || slot.kind == WorkKind::Idle {
            options.suppress_continuation = true;
        }
        let result = commit(&slot.snapshot, prepared, &options, live_backend);
        self.release(slot);
        let outcome = result?;
        self.on_committed();
        Ok(Some(outcome))
    }
}
impl Drop for CompactionRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl super::Agent<'_> {
    pub fn background_compaction_control(&self) -> CompactionControl {
        self.compaction_runtime.control()
    }
    pub fn note_compaction_input(&mut self) {
        self.compaction_runtime.note_user_input(Instant::now());
    }
    pub fn cancel_background_compaction(&mut self) {
        self.compaction_runtime.reset();
    }
    pub fn compaction_wakeup(&self) -> Option<Duration> {
        self.compaction_runtime
            .next_wakeup(&self.config.compaction, Instant::now())
    }
    pub fn maintenance_has_live_work(&self) -> bool {
        self.live_tasks.control().any_running() || self.ext.has_live_work()
    }

    pub(crate) fn runtime_accounting(&self) -> RuntimeAccounting {
        RuntimeAccounting {
            session_id: self.session.id().to_owned(),
            scope: self.usage_scope.clone(),
            cost_base: self.usage_cost_base,
        }
    }

    pub(crate) fn restore_runtime_accounting(
        &mut self,
        accounting: RuntimeAccounting,
    ) -> Result<()> {
        ensure!(
            self.session.id() == accounting.session_id,
            "runtime accounting cannot move to another session owner"
        );
        self.usage_scope = accounting.scope;
        self.usage_cost_base = accounting.cost_base;
        self.bind_maintenance_scope();
        Ok(())
    }

    pub(super) fn poll_background_compaction(&mut self, ui: &mut dyn Ui) -> PollState {
        if self.interrupt.load(Ordering::SeqCst) {
            self.compaction_runtime.cancel();
        }
        let state = self.compaction_runtime.poll(ui);
        match &state {
            PollState::Failed(error) => ui.diagnostic(&format!(
                "Background compaction failed; original history is unchanged: {error}"
            )),
            PollState::TimedOut => {
                ui.diagnostic("Background compaction timed out; original history is unchanged.")
            }
            _ => {}
        }
        state
    }

    pub(super) fn start_background_compaction(
        &mut self,
        ui: &mut dyn Ui,
        template: Option<&Chat>,
        kind: WorkKind,
        method: Option<CompactionMethod>,
        context_tokens: u64,
        native_profile: Option<&crate::llm::profiles::ModelProfile>,
        live_backend: Option<&mut Backend>,
    ) -> Result<Launch> {
        let policy = &self.config.compaction;
        if self.hermetic
            || !policy.enabled
            || policy.experimental_context_management
            || policy.strategy == CompactionStrategy::Off
            || (kind == WorkKind::Speculative && !policy.async_enabled)
            || (kind == WorkKind::Idle && !policy.idle_enabled)
        {
            return Ok(Launch::Disabled);
        }
        if self.compaction_runtime.has_slot() || self.compaction_runtime.has_live_worker() {
            return Ok(Launch::Busy);
        }
        self.prepare_compaction_memory(ui, live_backend);
        let mut snapshot = self.compaction_snapshot(template)?;
        snapshot.native_profile = native_profile.cloned().or(snapshot.native_profile.take());
        let namespace = side_request_namespace(&snapshot.session_id)?;
        let mut main = super::make_backend(&snapshot.main_config)?
            .without_steering()
            .with_cache_namespace(&namespace, &snapshot.main_config)?;
        main.next_request_profile(crate::llm::profiles::request_tiers_for(
            snapshot.native_profile.clone(),
            &snapshot.main_config.selection(),
            snapshot.main_config.effective_service_tier(),
            snapshot.main_config.extended_context,
        )?);
        let portable = match kind {
            WorkKind::Speculative => method.is_some_and(|method| {
                matches!(method, CompactionMethod::Soft | CompactionMethod::Handoff)
            }),
            WorkKind::Idle => policy_methods(&snapshot.policy)
                .iter()
                .any(|method| matches!(method, CompactionMethod::Soft | CompactionMethod::Handoff)),
        };
        let summary = if portable {
            snapshot
                .role_config
                .as_ref()
                .map(|config| {
                    super::make_backend(config)?
                        .without_steering()
                        .with_cache_namespace(&format!("{namespace}:compaction-role"), config)
                })
                .transpose()?
        } else {
            None
        };
        if main.local_inference() || summary.as_ref().is_some_and(Backend::local_inference) {
            return Ok(Launch::UnsupportedLocal);
        }
        let completion_config = snapshot
            .role_config
            .clone()
            .unwrap_or_else(|| snapshot.main_config.clone());
        let complete = portable_summary_completion(
            completion_config,
            self.layout.clone(),
            self.workspace.root().to_path_buf(),
            self.session.control_state(),
            self.tool_hooks().to_vec(),
            self.hook_plugins(),
            snapshot.session_id.clone(),
            self.hermetic,
        );
        let scope = if kind == WorkKind::Idle {
            self.usage_scope.next_turn(0)?
        } else {
            self.usage_scope.clone()
        };
        let body = self.session.runtime().start(
            "maintenance",
            match kind {
                WorkKind::Speculative => "Speculative compaction",
                WorkKind::Idle => "Idle compaction",
            },
            snapshot.branch_leaf.clone(),
        );
        let result = self.compaction_runtime.start(
            WorkerBackends { main, summary },
            snapshot,
            kind,
            context_tokens,
            CompactionOptions {
                method,
                require_method: method.is_some(),
                ..Default::default()
            },
            scope,
            complete,
            settle_into_runtime(body.clone()),
        );
        if !matches!(&result, Ok(Launch::Started)) {
            body.settle(
                match &result {
                    Ok(reason) => format!("Background compaction not started: {reason:?}"),
                    Err(error) => format!("Background compaction launch failed: {error:#}"),
                },
                None,
                crate::runtime::Phase::Failed,
            );
        } else {
            ui.background_notice(
                "Preparing compaction in the background; live history is unchanged.",
            );
        }
        result
    }

    pub(super) fn claim_background_compaction(
        &mut self,
        ui: &mut dyn Ui,
        reason: CompactionReason,
        template: Option<&Chat>,
        live_backend: Option<&mut Backend>,
    ) -> Result<Option<CompactionOutcome>> {
        let mut runtime = std::mem::take(&mut self.compaction_runtime);
        let result = runtime.claim_armed(reason, live_backend, |snapshot, prepared, options, live_backend| {
            // This runtime-only field is skipped by Config serialization. It binds the frozen
            // native account/cache history generation in addition to the peer's durable-leaf gate.
            ensure!(self.auth_config.provider_cache_namespace == snapshot.main_config.provider_cache_namespace,
                "background compaction's native history namespace changed; original history is unchanged");
            self.commit_compaction(ui, snapshot, prepared, options, template, live_backend)
        });
        self.compaction_runtime = runtime;
        result
    }

    /// Actors acquire their existing foreground lease only for a due local pass. Constructing the
    /// role Backend here inspects the real route; it never loads a model or borrows a resident server.
    pub fn foreground_idle_pending(&self, busy: bool) -> Result<bool> {
        if self.memory_foreground_pending(busy)? {
            return Ok(true);
        }
        if self.hermetic
            || self.config.compaction.experimental_context_management
            || self.interrupt.load(Ordering::SeqCst)
            || !self.compaction_runtime.idle_pending(
                &self.config.compaction,
                Instant::now(),
                self.context_usage().0,
                IdleActivity {
                    queued_prompt: busy,
                    live_worker: self.maintenance_has_live_work(),
                    ..Default::default()
                },
            )
        {
            return Ok(false);
        }
        if self.memory_runtime.is_some()
            && matches!(self.config.memory.backend.as_str(), "hindsight")
            && self.auth_config.memory_uses_local_compute()
        {
            return Ok(true);
        }
        if self.backend.as_ref().is_some_and(Backend::local_inference) {
            return Ok(true);
        }
        if !policy_methods(&self.config.compaction)
            .iter()
            .any(|method| matches!(method, CompactionMethod::Soft | CompactionMethod::Handoff))
        {
            return Ok(false);
        }
        let role = self
            .role_selection("small")
            .map(|selection| self.auth_config.with_selection(selection));
        Ok(role
            .as_ref()
            .map(|config| super::make_backend(config).map(|backend| backend.local_inference()))
            .transpose()?
            .unwrap_or(false))
    }

    /// Only persistent actor idle loops call this. An idle result never starts/resumes a user turn.
    /// The actor must acquire foreground compute before this call when foreground_idle_pending is true.
    pub fn maintenance_tick(
        &mut self,
        ui: &mut dyn Ui,
        busy: bool,
    ) -> Result<Option<CompactionOutcome>> {
        self.memory_maintenance_tick(ui, busy);
        self.poll_background_compaction(ui);
        let now = Instant::now();
        if self.interrupt.load(Ordering::SeqCst) {
            self.compaction_runtime.idle.activity(now);
            return Ok(None);
        }
        let tokens = self.context_usage().0;
        let activity = IdleActivity {
            queued_prompt: busy || !ui.maintenance_ready(),
            live_worker: self.maintenance_has_live_work(),
            ..Default::default()
        };
        if activity.busy() {
            self.compaction_runtime
                .idle_due(&self.config.compaction, now, tokens, activity);
            return Ok(None);
        }
        if self.compaction_runtime.kind() == Some(WorkKind::Idle)
            && self.compaction_runtime.is_armed()
        {
            return self.claim_background_compaction(ui, CompactionReason::Idle, None, None);
        }
        if !self
            .compaction_runtime
            .idle_due(&self.config.compaction, now, tokens, activity)
        {
            return Ok(None);
        }
        if self.config.compaction.experimental_context_management {
            // The consumed timer bounds this reminder; accepted new_context tools own actual rollover.
            ui.background_notice("Idle context pressure: maintain durable context notes on the next real turn; no new turn or context rollover was started.");
            return Ok(None);
        }
        if self.compaction_runtime.is_armed() {
            match self.claim_background_compaction(ui, CompactionReason::Idle, None, None) {
                Ok(Some(outcome)) => return Ok(Some(outcome)),
                Ok(None) => {}
                Err(error) => ui.diagnostic(&format!("Discarded stale idle compaction: {error:#}")),
            }
        }
        match self.start_background_compaction(
            ui,
            None,
            WorkKind::Idle,
            None,
            tokens,
            None,
            None,
        )? {
            Launch::UnsupportedLocal => self.compact_foreground_idle(ui).map(Some),
            _ => Ok(None),
        }
    }
}

impl super::Agent<'_> {
    fn bind_maintenance_scope(&mut self) {
        if let Some(backend) = &mut self.backend {
            backend.set_usage_scope(Some(self.usage_scope.clone()));
        }
        for backend in self.role_backends.values_mut() {
            backend.set_usage_scope(Some(self.usage_scope.clone()));
        }
    }

    fn compact_foreground_idle(&mut self, ui: &mut dyn Ui) -> Result<CompactionOutcome> {
        self.compaction_runtime.cancel();
        let guard = self
            .compaction_runtime
            .foreground_idle(maintenance_timeout(self.config.worker_timeout_secs))?;
        let scope = self
            .usage_scope
            .next_turn(0)?
            .worker()
            .for_purpose(llm::usage::Purpose::Compaction);
        let original_scope = std::mem::replace(&mut self.usage_scope, scope.clone());
        let original_interrupt = std::mem::replace(&mut self.interrupt, guard.cancel.clone());
        self.bind_maintenance_scope();
        let owner = self.session.id().to_owned();
        let leaf = self.session.branch_leaf().map(str::to_owned);
        let body = self.session.runtime().start(
            "maintenance",
            "Idle compaction (foreground compute)",
            leaf.clone(),
        );
        let result = self.compact_with_options(
            ui,
            CompactionOptions {
                reason: CompactionReason::Idle,
                suppress_continuation: true,
                ..Default::default()
            },
        );
        self.interrupt = original_interrupt;
        self.usage_scope = original_scope;
        self.bind_maintenance_scope();
        let (total, turn) = scope.snapshot();
        let phase = if result.is_ok() {
            crate::runtime::Phase::Completed
        } else if guard.cancel.load(Ordering::SeqCst) {
            crate::runtime::Phase::Interrupted
        } else {
            crate::runtime::Phase::Failed
        };
        body.settle(match &result {
            Ok(outcome) => match outcome.method {
                Some(method) => format!("Idle context compacted via {method:?}"),
                None => format!("Idle maintenance completed ({:?})", outcome.kind),
            },
            Err(error) => format!("Idle compaction was not committed: {error:#}"),
        }, Some(serde_json::json!({
            "session_id": owner, "branch_leaf": leaf, "kind": "idle-foreground",
            "usage_total": total, "usage_turn": turn,
            "usage_coverage": "cumulative original-agent ledger; not an additional billable delta",
            "timed_out": guard.deadline.timed_out.load(Ordering::SeqCst),
        })), phase);
        drop(guard);
        self.compaction_runtime.idle.activity(Instant::now());
        if result.is_ok() {
            self.compaction_runtime.on_committed();
        }
        result
    }
}

impl Drop for super::Agent<'_> {
    fn drop(&mut self) {
        // Cancel before resident Backend/Session fields begin their own teardown. Joining a remote
        // transport here would hold the actor (and possibly the local compute lease) indefinitely.
        self.cancellation.cancel();
        self.compaction_runtime.shutdown();
        self.observe_session_end();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        config::types::{Config, HookConfig},
        llm::{fake, provider::ProviderKind},
    };
    use serde_json::json;

    #[test]
    fn compaction_summary_request_gets_before_provider_request() {
        let event = json!({"choices":[{"delta":{"content":"summary"},"finish_reason":"stop"}]});
        let server = fake::serve(vec![fake::Script::Sse(format!(
            "data: {event}\n\ndata: [DONE]\n\n"
        ))]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: format!("{}/v1", server.url),
                model: "test".into(),
                api_key: None,
            },
            std::path::PathBuf::new(),
            5,
        )
        .with_session_id("compaction-test")
        .with_profile(llm::profiles::ModelProfile {
            provider: "openai".into(),
            model: "test".into(),
            max_tokens: Some(1024),
            ..Default::default()
        });
        let rewrite = HookConfig {
            event: "before_provider_request".into(),
            matcher: None,
            command: vec![
                "sh".into(),
                "-c".into(),
                r#"printf '{"payload":{"messages":[{"role":"user","content":"rewritten"}],"tools":[],"temperature":0.0,"max_tokens":64,"seed":null}}'"#.into(),
            ],
            timeout_secs: 30,
            fail_closed: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut complete = portable_summary_completion(
            Config::default(),
            None,
            dir.path().to_path_buf(),
            json!({}),
            vec![rewrite],
            None,
            String::new(),
            false,
        );
        let source = Chat {
            messages: vec![json!({"role":"user","content":"original"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        };
        let summary = complete(
            &mut backend,
            &source,
            &mut super::super::NullUi,
            Arc::new(AtomicBool::new(false)),
            5,
        )
        .unwrap();
        assert_eq!(summary, "summary");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(fake::body(&requests[0])["max_tokens"], 64);
        assert_eq!(backend.model_profile().unwrap().max_tokens, Some(1024));
        let sent = fake::body(&requests[0])["messages"].to_string();
        assert!(
            sent.contains("rewritten") && !sent.contains("original"),
            "{sent}"
        );
    }
}
