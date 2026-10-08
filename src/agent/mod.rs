//! The agent runtime: one conversation, native tool calling, sessions, approvals, checkpoints,
//! compaction and (optionally) trajectory recording for the self-improvement loop.
//!
//! The same loop serves three callers: the interactive REPL and print mode (full context: memory,
//! skills, project files, persistent session) and the sealed gate (`run`: hermetic, ephemeral,
//! auto-approved, strict sandbox, nothing read from the user's environment).
pub mod approval;
mod assistance;
pub mod calls;
mod child;
mod completion;
mod context_meter;
pub mod hooks;
pub use completion::TurnOutcome;
pub mod context;
mod evidence;
pub mod guard;
mod investigation;
mod isolation;
pub mod laya;
mod live_tasks;
pub mod output_schema;
mod progress;
pub mod prompt;
mod reasoning;
mod recovery_checkpoint;
pub(crate) mod repair_evidence;
pub mod solve;
mod state_ui;
pub mod subagents;
mod task_packet;
pub mod todo;
mod training;
mod verification;
mod visual;
pub use live_tasks::LiveTaskControl;

use crate::{
    config::types::{Approval, Config, Event, SandboxMode, Version},
    learn::{verify::Check, worker},
    llm::{
        self, Chat, ToolCall,
        provider::{Backend, ProviderKind},
    },
    storage::{
        checkpoint::Checkpoints,
        codebase::Codebase,
        paths::Layout,
        provenance::ProvenanceMemory,
        session::{self, Session},
        skills,
        store::Store,
    },
    tools::{
        self, ToolOutput, Workspace,
        ext::{LifecycleEvent, Registry, ToolContext},
    },
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};
use todo::TodoList;

pub enum Decision {
    Allow,
    AllowAlways,
    Deny,
}

pub trait Ui {
    fn runtime_tree(&self) -> Option<crate::runtime::Tree> {
        None
    }
    fn runtime(&mut self, _tree: &crate::runtime::Tree) {}
    /// Shared result body, delivered before its legacy display callback.
    fn result_node(&mut self, _body: &crate::runtime::Body) {}
    fn text_delta(&mut self, _text: &str) {}
    fn tool_call(&mut self, _name: &str, _summary: &str) {}
    fn tool_result(&mut self, _name: &str, _output: &ToolOutput) {}
    fn notice(&mut self, _message: &str) {}
    /// Internal bookkeeping (policy selection, index refresh). Defaults to a notice; the TUI keeps it out of the transcript.
    fn diagnostic(&mut self, message: &str) {
        self.notice(message);
    }
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
    fn model_progress(&mut self, _progress: &llm::ModelProgress) {}
    fn tool_output(&mut self, _name: &str, _text: &str) {}
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
    fn approval_reason(&mut self) -> Option<String> {
        None
    }
    /// Called while an inference/tool worker is pending so interactive surfaces can repaint and pump input.
    fn heartbeat(&mut self) {}
    /// Elapsed time without visible model output while its request is still pending.
    fn model_wait(&mut self, _silence: Duration) {}
    /// Messages the user typed while the turn ran. Polled between steps; the agent appends them as user
    /// messages before the next model request so the model can change course without waiting for the turn to end.
    fn take_steering(&mut self) -> Vec<String> {
        Vec::new()
    }
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

enum ChatEvent {
    Progress(llm::ModelProgress),
    Text(String),
    Reasoning(String),
    ToolArguments {
        index: u64,
        id: String,
        name: String,
        delta: String,
    },
    Notice(String),
    Finished(Result<llm::Reply>),
}

fn receive_with_heartbeat<T>(rx: &Receiver<T>, ui: &mut dyn Ui) -> Result<T> {
    loop {
        match rx.recv_timeout(Duration::from_millis(80)) {
            Ok(event) => return Ok(event),
            Err(RecvTimeoutError::Timeout) => ui.heartbeat(),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("background operation ended without a result")
            }
        }
    }
}

fn chat_with_ui(
    backend: &mut Backend,
    adapter: Option<&Path>,
    request: &Chat,
    timeout: u64,
    interrupt: Arc<AtomicBool>,
    render_text: bool,
    context_tokens: Option<u64>,
    ui: &mut dyn Ui,
    policy: Option<&crate::config::types::ReasoningGuardConfig>,
    on_idle: &mut dyn FnMut(&mut dyn Ui),
) -> Result<llm::Reply> {
    let mut context = context_tokens.map(context_meter::StreamingContext::new);
    if let Some(tokens) = context_tokens {
        ui.context_usage(tokens, true);
    }
    let (tx, rx) = mpsc::channel();
    let mut progress = backend
        .local_inference()
        .then(progress::RequestProgress::new);
    if let Some(progress) = &progress {
        ui.model_progress(&progress.value);
    }
    ui.model_wait(Duration::ZERO);
    thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name("dial-model".to_owned())
            .spawn_scoped(scope, move || {
                let mut on_text = |text: &str| {
                    tx.send(ChatEvent::Text(text.to_owned())).is_ok()
                        && !interrupt.load(Ordering::SeqCst)
                };
                let mut on_notice = |notice: &str| {
                    let _ = tx.send(ChatEvent::Notice(notice.to_owned()));
                };
                let mut on_event = |event: &llm::StreamEvent| {
                    match event {
                        llm::StreamEvent::Progress(value) => {
                            let _ = tx.send(ChatEvent::Progress(value.clone()));
                        }
                        llm::StreamEvent::Reasoning(text) => {
                            let _ = tx.send(ChatEvent::Reasoning(text.clone()));
                        }
                        llm::StreamEvent::ToolArguments {
                            index,
                            id,
                            name,
                            delta,
                        } => {
                            let _ = tx.send(ChatEvent::ToolArguments {
                                index: *index,
                                id: id.clone(),
                                name: name.clone(),
                                delta: delta.clone(),
                            });
                        }
                    }
                    !interrupt.load(Ordering::SeqCst)
                };
                let result = if let Some(policy) = policy {
                    reasoning::chat(
                        backend,
                        adapter,
                        request,
                        timeout,
                        Arc::clone(&interrupt),
                        policy,
                        &mut on_text,
                        &mut on_notice,
                        &mut on_event,
                    )
                } else {
                    backend.chat_with_control_events(
                        adapter,
                        request,
                        timeout,
                        Some(&mut on_text),
                        &mut on_notice,
                        Arc::clone(&interrupt),
                        &mut on_event,
                    )
                };
                let _ = tx.send(ChatEvent::Finished(result));
            })
            .map_err(|error| anyhow::anyhow!("could not start model worker: {error}"))?;
        let mut last_progress = Instant::now();
        let mut last_tick = Instant::now();
        let result = loop {
            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(ChatEvent::Progress(value)) => {
                    last_progress = Instant::now();
                    if let Some(context) = &mut context {
                        let (tokens, estimated) = context.telemetry(&value);
                        ui.context_usage(tokens, estimated);
                    }
                    if let Some(progress) = &mut progress {
                        progress.observe(value, last_progress);
                        ui.model_progress(&progress.value);
                    }
                }
                Ok(ChatEvent::Text(text)) => {
                    last_progress = Instant::now();
                    if let Some(progress) = &mut progress {
                        progress.text(&text, last_progress);
                        ui.model_progress(&progress.value);
                    }
                    if render_text {
                        if let Some(context) = &mut context {
                            ui.context_usage(context.observe(&text), context.estimated());
                        }
                        ui.text_delta(&text);
                    }
                }
                Ok(ChatEvent::Notice(notice)) => {
                    last_progress = Instant::now();
                    ui.notice(&notice);
                }
                Ok(ChatEvent::Reasoning(text)) => {
                    last_progress = Instant::now();
                    if let Some(progress) = &mut progress {
                        progress.text(&text, last_progress);
                        ui.model_progress(&progress.value);
                    }
                    if render_text {
                        if let Some(context) = &mut context {
                            ui.context_usage(context.observe(&text), context.estimated());
                        }
                        ui.reasoning_delta(&text);
                    }
                }
                Ok(ChatEvent::ToolArguments {
                    index,
                    id,
                    name,
                    delta,
                }) => {
                    last_progress = Instant::now();
                    if let Some(progress) = &mut progress {
                        progress.text(&delta, last_progress);
                        ui.model_progress(&progress.value);
                    }
                    if render_text {
                        if let Some(context) = &mut context {
                            ui.context_usage(context.observe(&delta), context.estimated());
                        }
                        ui.tool_arguments_delta(index, &id, &name, &delta);
                    }
                }
                Ok(ChatEvent::Finished(result)) => {
                    if let (Some(progress), Ok(reply)) = (&mut progress, &result) {
                        if reply.completion_tokens.is_some() {
                            let mut reported =
                                llm::ModelProgress::new(llm::InferencePhase::Generating);
                            reported.output_tokens = reply.completion_tokens;
                            reported.prompt_tokens = reply.prompt_tokens;
                            progress.observe(reported, Instant::now());
                            ui.model_progress(&progress.value);
                        }
                    }
                    if let (Some(context), Ok(reply)) = (&context, &result) {
                        let (tokens, estimated) =
                            context.finish(reply.prompt_tokens, reply.completion_tokens);
                        ui.context_usage(tokens, estimated);
                    }
                    break result;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    break Err(anyhow::anyhow!("model worker ended without a result"));
                }
            }
            // Pump input even when a role's hidden text continuously fills the event queue.
            if last_tick.elapsed() >= Duration::from_millis(80) {
                ui.model_wait(last_progress.elapsed());
                ui.heartbeat();
                on_idle(ui);
                last_tick = Instant::now();
            }
        };
        if worker.join().is_err() {
            return Err(anyhow::anyhow!("model worker panicked"));
        }
        result
    })
}

fn interrupted_tool() -> ToolOutput {
    ToolOutput {
        data: None,
        text: "interrupted by the user before this ran".into(),
        is_error: true,
    }
}

/// Run only a prevalidated consecutive group. No agent/session/UI state enters a worker.
fn parallel_tools(
    workspace: &Workspace,
    config: &Config,
    ext: &Registry,
    calls: &[ToolCall],
    interrupt: &AtomicBool,
    ui: &mut dyn Ui,
) -> Vec<(ToolOutput, f64)> {
    thread::scope(|scope| {
        let mut receivers = Vec::with_capacity(calls.len());
        for call in calls {
            let (tx, rx) = mpsc::sync_channel(1);
            receivers.push(rx);
            scope.spawn(move || {
                let started = Instant::now();
                let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if interrupt.load(Ordering::SeqCst) {
                        return interrupted_tool();
                    }
                    if tools::BUILTIN.contains(&call.name.as_str()) {
                        tools::execute(workspace, &call.name, &call.arguments, 120)
                    } else if let Some(tool) = ext.get(&call.name) {
                        tool.call(&ToolContext { workspace, config }, &call.arguments)
                    } else {
                        ToolOutput {
                            data: None,
                            text: "parallel tool unavailable".into(),
                            is_error: true,
                        }
                    }
                }))
                .unwrap_or_else(|_| ToolOutput {
                    data: None,
                    text: "tool worker panicked".into(),
                    is_error: true,
                });
                let _ = tx.send((output, started.elapsed().as_secs_f64()));
            });
        }
        receivers
            .into_iter()
            .map(|rx| {
                receive_with_heartbeat(&rx, ui).unwrap_or_else(|error| {
                    (
                        ToolOutput {
                            data: None,
                            text: format!("{error:#}"),
                            is_error: true,
                        },
                        0.0,
                    )
                })
            })
            .collect()
    })
}

pub struct TurnReport {
    pub text: String,
    pub outcome: TurnOutcome,
    pub steps: u32,
    pub interrupted: bool,
    pub trajectory: Option<i64>,
}

/// Holds the model backend between runs so a local server stays resident.
pub struct Runtime {
    backend: Option<Backend>,
}

impl Runtime {
    pub fn new() -> Self {
        Self { backend: None }
    }

    pub fn backend(&mut self, config: &Config) -> Result<&mut Backend> {
        if self.backend.is_none() {
            self.backend = Some(make_backend(config)?);
        }
        self.backend.as_mut().context("backend missing")
    }

    /// Move the resident backend out; a later `backend()` rebuilds it on demand.
    pub fn take_backend(&mut self, config: &Config) -> Result<Backend> {
        if let Some(backend) = self.backend.take() {
            return Ok(backend);
        }
        make_backend(config)
    }

    pub fn release(&mut self) {
        if let Some(backend) = self.backend.as_mut() {
            backend.release();
        }
        self.backend = None;
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

pub fn make_backend(config: &Config) -> Result<Backend> {
    config.require_model()?;
    let kind = match (&config.provider, &config.endpoint) {
        (Some(provider), _) if provider == crate::llm::chatgpt::PROVIDER => {
            if let Some(endpoint) = &config.endpoint {
                ensure!(
                    endpoint.base_url == crate::llm::chatgpt::BASE_URL
                        && endpoint.api_key_env.is_none(),
                    "ChatGPT subscription uses its official Responses endpoint and OAuth credentials only"
                );
            }
            ProviderKind::ChatGpt {
                model: config.model.clone(),
                user_dir: config.user_dir.clone(),
            }
        }
        (Some(provider), Some(endpoint))
            if provider == "anthropic"
                && endpoint.base_url == crate::llm::anthropic::BASE_URL
                && crate::llm::anthropic_oauth::signed_in(&config.user_dir) =>
        {
            ProviderKind::AnthropicSubscription {
                model: config.model.clone(),
                user_dir: config.user_dir.clone(),
            }
        }
        (Some(provider), Some(endpoint)) if provider == "anthropic" => {
            ProviderKind::AnthropicEndpoint {
                base_url: endpoint.base_url.clone(),
                model: config.model.clone(),
                api_key: crate::llm::catalog::resolve_credential(
                    &config.user_dir,
                    &endpoint.base_url,
                    endpoint.api_key_env.as_deref(),
                )?,
            }
        }
        (_, None) => ProviderKind::Local {
            model_dir: crate::llm::catalog::resolve_local_model(&config.model)?,
        },
        (_, Some(endpoint)) => ProviderKind::Endpoint {
            base_url: endpoint.base_url.clone(),
            model: config.model.clone(),
            api_key: crate::llm::catalog::resolve_credential(
                &config.user_dir,
                &endpoint.base_url,
                endpoint.api_key_env.as_deref(),
            )?,
        },
    };
    let layout = Layout {
        project_root: config.project_root.clone(),
        project_dir: config.project_root.join(".dial"),
        user_dir: config.user_dir.clone(),
        state_dir: config.state_dir.clone(),
    };
    let mut backend = Backend::new(
        kind,
        config.python.clone(),
        config.worker_timeout_secs.min(600),
    )
    .with_remote_idle_timeout(config.remote_model_idle_timeout_secs)
    .with_allow_insecure(config.endpoint.as_ref().is_some_and(|e| e.allow_insecure))
    .with_metadata(llm::catalog::model_metadata(&layout, &config.selection()))
    .with_steering(config);
    if let Some(profile) = config.active_model_profile() {
        backend = backend.with_profile(profile.clone());
    } else if config.provider.as_deref() == Some(llm::chatgpt::PROVIDER)
        && let Some(profile) = llm::chatgpt::model_profile(&config.user_dir, &config.model)?
    {
        backend = backend.with_profile(profile);
    }
    Ok(backend)
}

struct CheckpointObserver(Arc<Checkpoints>);

impl tools::WriteObserver for CheckpointObserver {
    fn before_write(&self, abs: &Path) -> Result<()> {
        self.0.before_write(abs)
    }
}

struct PromptParts {
    soul: Option<String>,
    overrides: (Option<String>, Option<String>),
    context: Vec<context::ContextFile>,
    skills: String,
    memory: String,
}

#[derive(Default, Debug, serde::Serialize)]
pub struct AgentMetrics {
    pub cached_tokens: u64,
    /// Main-agent responses in this runtime; transport retries and workers are not included.
    pub responses: u64,
    pub usage_reported_responses: u64,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub compactions: u64,
    pub tool_contract_errors: u64,
    pub artifacts_retrieved: u64,
    pub dynamic_tools_used: u64,
    pub truncated_recoveries: u64,
    /// Automatic focused-verification repair rounds actually executed.
    pub repair_attempts: u64,
    pub reasoning_interceptions: u64,
    pub turns_completed: u64,
}

/// Per-turn bookkeeping for what `todo done` checks at step boundaries.
#[derive(Default)]
struct StepChecks {
    visual: visual::Review,
    /// A successful finish call can request review without accepting the turn's outcome.
    finish_accepted: bool,
    /// The checkpoint turn whose changed files the checks cover.
    turn: Option<u64>,
    /// Successful file edits this turn, and how many of them the last check run covered.
    mutations: u64,
    checked: u64,
    /// Number of checkpointed edits whose bounded diff has been surfaced for review.
    diff_surfaced: u64,
    /// Non-read-only shell attempts outside checkpoint observation.
    uncheckpointed_shell_commands: u64,
    disclosed_shell_commands: u64,
    /// Approval-requiring extension attempts that may have effects outside checkpoint observation.
    uncheckpointed_extension_calls: u64,
    disclosed_extension_calls: u64,
    uncheckpointed_extension_tools: std::collections::BTreeSet<String>,
    /// Exact configured checks whose most recent run failed; retain them across new edits.
    failed_checks: Vec<verification::FocusedCheck>,
    failed: bool,
    /// A successful non-control tool call since the current step started (or since the turn began).
    evidence: bool,
    /// A text-only answer emitted since this step began; not verification of edits or findings.
    answer_delivered: bool,
}
impl StepChecks {
    fn clean(&self) -> bool {
        self.checked > 0
            && self.checked == self.mutations
            && !self.failed
            && self.uncheckpointed_shell_commands == 0
            && self.uncheckpointed_extension_calls == 0
    }
    fn uncheckpointed_pending(&self) -> bool {
        self.uncheckpointed_shell_commands > self.disclosed_shell_commands
            || self.uncheckpointed_extension_calls > self.disclosed_extension_calls
    }
    fn has_uncheckpointed_operations(&self) -> bool {
        self.uncheckpointed_shell_commands > 0 || self.uncheckpointed_extension_calls > 0
    }
    fn include_failed_checks(&self, checks: &mut Vec<verification::FocusedCheck>) {
        for failed in &self.failed_checks {
            if !checks.iter().any(|check| check.command == failed.command) {
                checks.push(failed.clone());
            }
        }
    }
    fn status(&self) -> Value {
        let phase = if self.failed {
            "checks_failed"
        } else if self.uncheckpointed_pending() {
            "uncheckpointed_operations_pending_review"
        } else if self.has_uncheckpointed_operations() {
            "uncheckpointed_operations_disclosed_not_verified"
        } else if self.mutations == 0 {
            "no_edits"
        } else if self.checked < self.mutations {
            "edited_unchecked"
        } else if self.diff_surfaced < self.mutations {
            "checks_passed_diff_surfaced_review_pending"
        } else {
            "checks_passed_diff_surfaced"
        };
        json!({
            "phase":phase,
            "mutations":self.mutations,
            "checked_mutations":self.checked,
            "diff_surfaced_mutations":self.diff_surfaced,
            "uncheckpointed_shell_commands":self.uncheckpointed_shell_commands,
            "disclosed_shell_commands":self.disclosed_shell_commands,
            "uncheckpointed_extension_calls":self.uncheckpointed_extension_calls,
            "disclosed_extension_calls":self.disclosed_extension_calls,
            "uncheckpointed_extension_tools":self.uncheckpointed_extension_tools,
            "failed_checks":self.failed_checks.iter().map(|check| &check.name).collect::<Vec<_>>()
        })
    }
}

pub struct Agent<'a> {
    config: &'a Config,
    /// Owned so a turn can move it into `run_loop` without conflicting with `&mut self`.
    backend: Option<Backend>,
    /// Cached backend for a local `subagent` role (remote roles get a fresh backend per child).
    subagent_backend: Option<Backend>,
    /// Lazily created, cached side backends for role-routed requests (compaction, …).
    role_backends: BTreeMap<String, Backend>,
    /// Context the serving endpoint reports as currently loaded; may be far below the configured window.
    context_cap: Option<u32>,
    layout: Option<Layout>,
    session: Session,
    workspace: Workspace,
    /// Pluggable tools (web, MCP, LSP, …); empty for hermetic agents.
    ext: Registry,
    todo: TodoList,
    memory: Option<Arc<ProvenanceMemory>>,
    codebase: Option<Arc<Codebase>>,
    experience: Option<Arc<crate::storage::experience::Experience>>,
    assistance: assistance::State,
    checkpoints: Option<Arc<Checkpoints>>,
    store: Option<&'a Store>,
    version: Version,
    system: String,
    parts: PromptParts,
    approval: Approval,
    always: Vec<approval::PermissionRule>,
    plan_mode: bool,
    session_cost: Option<f64>,
    pending_plan: Option<String>,
    interrupt: Arc<AtomicBool>,
    hermetic: bool,
    /// Overrides the trajectory domain (self-play records as `selfplay:<family>`).
    domain_override: Option<String>,
    trajectory: Option<(i64, String, String)>,
    last_trajectory: Option<i64>,
    /// Actual inference requests paired with eligible targets from the current turn.
    training_capture: training::Capture,
    logging: bool,
    /// Outcomes of recognised check commands (tests, builds, linters) run during the turn.
    verifications: Vec<bool>,
    step_checks: StepChecks,
    cur_step: u32,
    pub last_prompt_tokens: u64,
    context_meter: context_meter::ContextMeter,
    /// How many conversation messages the request behind `last_prompt_tokens` carried.
    reported_messages: usize,
    step_probe: Option<crate::learn::probe::StepProbe>,
    enabled_tools: HashSet<String>,
    metrics: AgentMetrics,
    usage_scope: llm::usage::Scope,
    workflow_metrics: std::cell::RefCell<crate::learn::workflow::Metrics>,
    workflow_started: Instant,
    task_policy: crate::learn::task_policy::Policy,
    request_phase: crate::adaptive::State,
    repair_tracker: repair_evidence::Tracker,
    repair_sequence: u64,
    task_packet: task_packet::PacketCache,
    reasoning_recovery: reasoning::Recovery,
    live_tasks: live_tasks::LiveTasks,
    superseded: evidence::Supersession,
    legacy_memory_migrated: bool,
}

impl<'a> Agent<'a> {
    /// Full interactive/print agent for the project in `layout`.
    pub fn interactive(
        config: &'a Config,
        backend: Backend,
        layout: Layout,
        session: Session,
        subagent_backend: Option<Backend>,
        store: Option<&'a Store>,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Self> {
        let mut version = match store {
            Some(store) => store.active()?,
            None => Version {
                id: 0,
                parent: None,
                kind: "config".into(),
                harness: config.harness.clone(),
                adapter: None,
                status: "active".into(),
            },
        };
        let legacy_memory_migrated =
            crate::config::types::migrate_memory_tools(&mut version.harness);
        config.validate_harness(&version.harness)?;
        version.harness = config.effective_harness(&version.harness)?;
        let memory = if config.memory.enabled {
            Some(Arc::new(ProvenanceMemory::open(&layout)?))
        } else {
            None
        };
        let codebase = if config.codebase.enabled {
            Some(Arc::new(Codebase::open(&layout)?))
        } else {
            None
        };
        let checkpoints = Arc::new(Checkpoints::new(
            layout.checkpoints_dir().join(session.id()),
            layout.project_root.clone(),
        )?);
        let mode = version.harness.edit_mode.unwrap_or(config.edit_mode);
        layout.ensure_state_outside_project()?;
        let workspace = configure(
            Workspace::new(&layout.project_root)?,
            config,
            mode,
            Some(&layout),
            Some(checkpoints.clone()),
        )?
        .with_artifacts(tools::artifact::ArtifactStore::new(
            &layout.state_dir.join("artifacts"),
        )?);
        let skills = skills::discover(&layout);
        let parts = PromptParts {
            soul: context::soul(&layout),
            overrides: context::system_overrides(&layout),
            context: context::discover(&layout),
            skills: skills::catalog_block(&skills),
            memory: String::new(),
        };
        layout.ensure_state_outside_project()?;
        let always = approval::load(&layout.state_dir.join("permissions.toml"))?;
        let mut agent = Self {
            step_probe: None,
            config,
            backend: Some(backend),
            experience: if config.assistance.enabled {
                crate::storage::experience::Experience::open(&layout)
                    .ok()
                    .map(Arc::new)
            } else {
                None
            },
            assistance: assistance::State::restore(&session.control_state()),
            layout: Some(layout),
            session,
            workspace,
            ext: Registry::standard(config),
            todo: TodoList::default(),
            memory,
            codebase,
            checkpoints: Some(checkpoints),
            store,
            approval: config.approval,
            always,
            interrupt,
            plan_mode: false,
            session_cost: None,
            pending_plan: None,
            hermetic: false,
            domain_override: None,
            trajectory: None,
            last_trajectory: None,
            training_capture: training::Capture::default(),
            logging: true,
            verifications: Vec::new(),
            step_checks: StepChecks::default(),
            cur_step: 0,
            last_prompt_tokens: 0,
            context_meter: context_meter::ContextMeter::default(),
            reported_messages: 0,
            version,
            system: String::new(),
            parts,
            subagent_backend,
            role_backends: BTreeMap::new(),
            context_cap: None,
            enabled_tools: HashSet::new(),
            metrics: AgentMetrics::default(),
            usage_scope: llm::usage::Scope::default().next_turn(subagents::MAX_STEPS),
            reasoning_recovery: reasoning::Recovery::default(),
            live_tasks: live_tasks::LiveTasks::default(),
            superseded: evidence::Supersession::default(),
            legacy_memory_migrated,
            workflow_metrics: Default::default(),
            workflow_started: Instant::now(),
            request_phase: crate::adaptive::State::begin(
                crate::learn::task_policy::Kind::General,
                "",
            ),
            task_policy: crate::learn::task_policy::Policy::for_prompt(
                "",
                config.reasoning_guard.max_pre_action_secs,
            ),
            repair_tracker: Default::default(),
            repair_sequence: 0,
            task_packet: Default::default(),
        };
        if let Some(memory) = &agent.memory {
            for tool in
                crate::storage::provenance::tools(memory.clone(), agent.session.id().to_owned())
            {
                agent.ext.register(tool);
            }
        }
        if let Some(cache) = &agent.codebase {
            agent.ext.register(crate::storage::codebase::tool(
                cache.clone(),
                agent.session.id().to_owned(),
            ));
        }
        agent.restore_control();
        agent.system = agent.build_system();
        Ok(agent)
    }

    /// Hermetic agent for sealed evaluation and self-play: no user memory, skills, context files or
    /// persistence of its own. With a `store` it records trajectories (self-play only).
    fn hermetic(
        config: &'a Config,
        backend: Backend,
        version: &Version,
        workspace: &Path,
        store: Option<&'a Store>,
        domain: Option<String>,
    ) -> Result<Self> {
        let mut version = version.clone();
        // Bound evaluation cost: a stuck candidate must fail, not run for an hour.
        version.harness.max_steps = version.harness.capped_steps(24);
        let legacy_memory_migrated =
            crate::config::types::migrate_memory_tools(&mut version.harness);
        config.validate_harness(&version.harness)?;
        let mut strict = config.clone();
        strict.sandbox.mode = SandboxMode::Strict;
        let mode = version.harness.edit_mode.unwrap_or(config.edit_mode);
        let workspace = configure_strict(Workspace::new(workspace)?, &strict, mode)?;
        let mut agent = Self {
            step_probe: None,
            config,
            backend: Some(backend.without_steering()),
            experience: None,
            assistance: Default::default(),
            layout: None,
            session: Session::ephemeral(),
            workspace,
            ext: Registry::new(),
            todo: TodoList::default(),
            memory: None,
            codebase: None,
            checkpoints: None,
            store,
            approval: Approval::Auto,
            always: Vec::new(),
            plan_mode: false,
            session_cost: None,
            pending_plan: None,
            interrupt: Arc::new(AtomicBool::new(false)),
            hermetic: true,
            domain_override: domain,
            trajectory: None,
            last_trajectory: None,
            training_capture: training::Capture::default(),
            logging: true,
            verifications: Vec::new(),
            step_checks: StepChecks::default(),
            cur_step: 0,
            last_prompt_tokens: 0,
            context_meter: context_meter::ContextMeter::default(),
            reported_messages: 0,
            version,
            workflow_metrics: Default::default(),
            workflow_started: Instant::now(),
            request_phase: crate::adaptive::State::begin(
                crate::learn::task_policy::Kind::General,
                "",
            ),
            task_policy: crate::learn::task_policy::Policy::for_prompt(
                "",
                config.reasoning_guard.max_pre_action_secs,
            ),
            repair_tracker: Default::default(),
            repair_sequence: 0,
            task_packet: Default::default(),
            system: String::new(),
            parts: PromptParts {
                soul: None,
                overrides: (None, None),
                context: Vec::new(),
                skills: String::new(),
                memory: String::new(),
            },
            subagent_backend: None,
            role_backends: BTreeMap::new(),
            context_cap: None,
            enabled_tools: HashSet::new(),
            metrics: AgentMetrics::default(),
            usage_scope: llm::usage::Scope::default().next_turn(subagents::MAX_STEPS),
            reasoning_recovery: reasoning::Recovery::default(),
            live_tasks: live_tasks::LiveTasks::default(),
            superseded: evidence::Supersession::default(),
            legacy_memory_migrated,
        };
        agent.system = agent.build_system();
        Ok(agent)
    }

    fn build_system(&self) -> String {
        let mut system = prompt::system_prompt(&prompt::Inputs {
            cwd: self.workspace.root(),
            harness: &self.version.harness,
            edit_mode: self.edit_mode(),
            soul: self.parts.soul.clone(),
            overrides: self.parts.overrides.clone(),
            context: &self.parts.context,
            skills: self.parts.skills.clone(),
            memory: self.parts.memory.clone(),
            tools: &self.offered(),
        });
        if !self.hermetic {
            system.push_str("\nDiscover specialist capabilities with tool_search, then tool_load their exact names. Loaded schemas are available only in the next model round, never the same call batch. Use working_state to preserve goals, decisions, constraints and open work with evidence links. artifact_read recovers retained output across compaction. task action=start creates independent live runs; status/wait/message/cancel control them. Children cannot spawn descendants.");
        }
        if !self.hermetic {
            system.push_str("\nBefore claiming a capability is unavailable because its schema is absent, check tool_search. For live web research, discover web_search and web_fetch, load them with tool_load, then search and fetch relevant result URLs in subsequent rounds. Cite source URLs and report search or network failures honestly; never invent results. Discovery and loading do not bypass approval or sandbox restrictions.");
        }
        if self.plan_mode {
            system.push_str("\n\nPlan mode: explore using read-only tools and produce a concrete plan. Do not mutate files or execute code. Use exit_plan with your complete plan to request user approval before implementation.");
        }
        system
    }

    fn edit_mode(&self) -> crate::config::types::EditModeConfig {
        self.version
            .harness
            .edit_mode
            .unwrap_or(self.config.edit_mode)
    }

    /// Pick up a version the improvement loop admitted since this session started (between turns only,
    /// so a turn never changes harness mid-flight).
    fn refresh_version(&mut self, ui: &mut dyn Ui, prompt: &str) -> Result<()> {
        let Some(store) = self.store else {
            return Ok(());
        };
        if self.hermetic || self.version.id == 0 {
            return Ok(());
        }
        let mut active = crate::learn::profiles::select(store, store.active()?, prompt)?;
        if active.id == self.version.id {
            return Ok(());
        }
        let migrated = crate::config::types::migrate_memory_tools(&mut active.harness);
        if migrated {
            ui.notice("Migrated legacy memory tool selection to provenance memory tools for this runtime; admitted history is unchanged.");
        }
        self.config.validate_harness(&active.harness)?;
        active.harness = self.config.effective_harness(&active.harness)?;
        crate::learn::improve::verify_adapter(store, &active)?;
        self.version = active;
        self.workspace = self
            .workspace
            .clone()
            .with_edit_mode(tools::EditMode::from(self.edit_mode()));
        self.system = self.build_system();
        ui.notice(&format!("now running accepted version v{} (selected for this task from measured evaluation results)", self.version.id));
        Ok(())
    }

    pub fn session(&self) -> &Session {
        &self.session
    }
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }
    pub fn version(&self) -> &Version {
        &self.version
    }
    pub fn steering_runtime(&self) -> serde_json::Value {
        self.backend
            .as_ref()
            .map(Backend::steering_runtime)
            .unwrap_or_else(|| serde_json::json!({"state":"unconfirmed"}))
    }

    pub fn adapter_monitor(&self) -> Option<crate::llm::provider::AdapterMonitor> {
        self.backend.as_ref().map(Backend::adapter_monitor)
    }

    /// Called between turns: unload resident models only after all child workers have finished.
    /// Keep backends and session history so the next turn can lazily reload the accepted version.
    pub fn release_idle_models(&mut self) -> bool {
        if self.backend.is_none() {
            return false;
        }
        let Ok(jobs) = self.live_tasks.status(None) else {
            return false;
        };
        if jobs
            .as_array()
            .is_none_or(|jobs| jobs.iter().any(|job| job["status"] == "running"))
        {
            return false;
        }
        if let Some(backend) = &mut self.backend {
            backend.release();
        }
        if let Some(backend) = &mut self.subagent_backend {
            backend.release();
        }
        for backend in self.role_backends.values_mut() {
            backend.release();
        }
        true
    }
    pub fn todo(&self) -> &TodoList {
        &self.todo
    }
    pub fn restore_todo(&mut self, snapshot: &Value) -> Result<()> {
        self.todo.restore(snapshot).map_err(anyhow::Error::msg)
    }
    pub fn checkpoints(&self) -> Option<&Arc<Checkpoints>> {
        self.checkpoints.as_ref()
    }
    pub fn set_approval(&mut self, approval: Approval) {
        self.approval = approval;
    }
    pub fn approval(&self) -> Approval {
        self.approval
    }
    pub fn swap_session(&mut self, session: Session) {
        self.live_tasks.cancel_all();
        self.session = session;
        self.last_prompt_tokens = 0;
        self.context_meter = context_meter::ContextMeter::default();
        self.reported_messages = 0;
        self.restore_control();
        if let Some(memory) = &self.memory {
            for tool in
                crate::storage::provenance::tools(memory.clone(), self.session.id().to_owned())
            {
                self.ext.register(tool);
            }
        }
        if let Some(cache) = &self.codebase {
            self.ext.register(crate::storage::codebase::tool(
                cache.clone(),
                self.session.id().to_owned(),
            ));
        }
        self.system = self.build_system();
    }
    pub fn backend_description(&self) -> String {
        self.backend
            .as_ref()
            .map_or("none".into(), Backend::describe)
    }
    /// The window to plan against: what is configured, limited by what the server really loaded.
    pub fn context_window(&self) -> u32 {
        match self.context_cap {
            Some(loaded) => self
                .config
                .requested_context_window()
                .map_or(loaded, |limit| limit.min(loaded)),
            None => self.config.context_window,
        }
    }
    pub fn tool_names(&self) -> Vec<String> {
        self.offered()
    }
    pub fn available_tool_names(&self) -> Vec<String> {
        self.catalog()
    }
    pub fn session_cost(&self) -> Option<f64> {
        self.session_cost
    }
    pub fn set_session_cost(&mut self, cost: Option<f64>) {
        self.session_cost = cost;
    }
    pub fn metrics(&self) -> &AgentMetrics {
        &self.metrics
    }
    pub fn usage(&self) -> (llm::usage::Groups, llm::usage::Groups) {
        self.usage_scope.snapshot()
    }

    /// An isolated benchmark RPC child may compare an adapter without admitting it into a live store.
    pub(crate) fn set_benchmark_adapter(&mut self, path: &Path) -> Result<()> {
        ensure!(
            self.store.is_none() && !self.hermetic,
            "benchmark adapters require an isolated RPC session"
        );
        ensure!(
            self.config.endpoint.is_none() && self.config.selection().provider == "mlx",
            "benchmark adapters require the local MLX backend"
        );
        let path = path.canonicalize().context("resolving benchmark adapter")?;
        ensure!(
            path.join("adapter_config.json").is_file()
                && path.join("adapters.safetensors").is_file(),
            "benchmark adapter must contain adapter_config.json and adapters.safetensors"
        );
        self.version.adapter = Some(path);
        Ok(())
    }
    pub fn register_tool(&mut self, tool: Arc<dyn tools::ext::Tool>) -> Result<()> {
        self.validate_tool_registration(tool.as_ref())?;
        self.ext.register(tool);
        self.system = self.build_system();
        Ok(())
    }
    pub fn register_tools(&mut self, tools: Vec<Arc<dyn tools::ext::Tool>>) -> Result<()> {
        let mut names = HashSet::new();
        for tool in &tools {
            self.validate_tool_registration(tool.as_ref())?;
            ensure!(
                names.insert(tool.name()),
                "duplicate tool name in extension batch"
            );
        }
        drop(names);
        for tool in tools {
            self.ext.register(tool);
        }
        self.system = self.build_system();
        Ok(())
    }
    fn validate_tool_registration(&self, tool: &dyn tools::ext::Tool) -> Result<()> {
        ensure!(
            tool.extension_api_version() == tools::ext::EXTENSION_API_VERSION,
            "unsupported extension API version"
        );
        let name = tool.name();
        ensure!(
            !tools::BUILTIN.contains(&name)
                && !tools::EXTRA_TOOLS.contains(&name)
                && !matches!(name, "exit_plan" | "task_message"),
            "extension cannot replace a core/control tool"
        );
        ensure!(
            !self.ext.contains(name)
                && !self.config.tools.contains_key(name)
                && !self.version.harness.tools.iter().any(|n| n == name),
            "tool name already registered"
        );
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
            "invalid extension tool name"
        );
        ensure!(
            tool.schema()["function"]["name"] == name,
            "extension schema name does not match tool name"
        );
        Ok(())
    }
    fn restore_control(&mut self) {
        let state = self.session.control_state();
        self.enabled_tools = state["enabled_tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        self.todo = TodoList::default();
        if let Some(snapshot) = state.get("todo") {
            let _ = self.todo.restore(snapshot);
        }
        // The workspace's record of shown lines lives in memory; rebuild it from this session's results so
        // a resumed or rebuilt agent can still edit lines the model was shown before.
        let messages = self.session.messages();
        let names: std::collections::HashMap<&str, &str> = messages
            .iter()
            .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
            .filter_map(|c| Some((c["id"].as_str()?, c["function"]["name"].as_str()?)))
            .collect();
        tools::hashline::rehydrate(
            &self.workspace,
            messages
                .iter()
                .filter(|m| m["role"] == "tool")
                .filter_map(|m| {
                    Some((
                        *names.get(m["tool_call_id"].as_str()?)?,
                        m["content"].as_str()?,
                    ))
                }),
        );
    }
    fn catalog(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .version
            .harness
            .tools
            .iter()
            .cloned()
            .chain(tools::EXTRA_TOOLS.iter().map(|s| s.to_string()))
            .chain(self.ext.names().map(str::to_owned))
            .chain(self.config.tools.keys().cloned())
            .collect();
        names.retain(|n| {
            tools::BUILTIN.contains(&n.as_str())
                || matches!(
                    n.as_str(),
                    "todo"
                        | "finish"
                        | "tool_search"
                        | "tool_load"
                        | "artifact_read"
                        | "working_state"
                )
                || (self.layout.is_some()
                    && matches!(
                        n.as_str(),
                        "task" | "skill" | "skill_manage" | "session_search"
                    ))
                || self.ext.contains(n)
                || self.config.tools.contains_key(n)
        });
        if !self.hermetic && self.layout.is_some() {
            if self.config.solve.enabled {
                names.push("solve".into());
            }
            if self.config.escalation.enabled {
                names.push("escalate".into());
            }
        }
        if self.experience.is_some() {
            names.extend(["experience".into(), "edit_preview".into()]);
        }
        if let Some(allowed) = self
            .config
            .active_model_profile()
            .and_then(|p| p.tools.as_ref())
        {
            names.retain(|name| allowed.contains(name));
        }
        if self
            .config
            .active_model_profile()
            .is_some_and(|p| p.capabilities.tools == Some(false))
        {
            names.clear();
        }
        if self.hermetic {
            names.retain(|n| tools::BUILTIN.contains(&n.as_str()) || n == "todo");
        }
        names.sort();
        names.dedup();
        names
    }
    pub fn plan_mode(&self) -> bool {
        self.plan_mode
    }
    pub fn set_plan_mode(&mut self, enabled: bool) {
        self.plan_mode = enabled && !self.hermetic;
        self.system = self.build_system();
    }
    pub fn permission_rules(&self) -> &[approval::PermissionRule] {
        &self.always
    }
    pub fn set_permission_rules(&mut self, rules: Vec<approval::PermissionRule>) {
        self.always = rules;
    }
    pub fn add_permission_rule(
        &mut self,
        rule: approval::PermissionRule,
        persist: bool,
    ) -> Result<()> {
        rule.validate()?;
        let mut rules = self.always.clone();
        rules.push(rule);
        if persist {
            let layout = self
                .layout
                .as_ref()
                .context("persistent permissions unavailable here")?;
            layout.ensure_state_outside_project()?;
            approval::save(&layout.state_dir.join("permissions.toml"), &rules)?;
        }
        self.always = rules;
        Ok(())
    }
    pub fn remove_permission_rule(&mut self, index: usize, persist: bool) -> Result<()> {
        ensure!(
            index < self.always.len(),
            "permission rule index out of range"
        );
        let mut rules = self.always.clone();
        rules.remove(index);
        if persist {
            let layout = self
                .layout
                .as_ref()
                .context("persistent permissions unavailable here")?;
            layout.ensure_state_outside_project()?;
            approval::save(&layout.state_dir.join("permissions.toml"), &rules)?;
        }
        self.always = rules;
        Ok(())
    }
    pub fn approve_user_shell(&mut self, command: &str, ui: &mut dyn Ui) -> bool {
        let call = ToolCall {
            id: String::new(),
            name: "bash".into(),
            arguments: json!({"command":command}),
            malformed_arguments: false,
        };
        self.authorize_call(&call, ui).is_ok()
    }

    /// Local servers serve a context of their own choosing, often far below Dial's default: LM Studio
    /// loads on demand, llama.cpp splits `-c` across slots, Ollama defaults small and truncates silently.
    /// Their native APIs report the served size (see `catalog::loaded_context`); hosted and unknown
    /// servers are left alone, and an unloaded model keeps the last known cap rather than forgetting it.
    fn probe_context(&mut self) {
        if self.hermetic {
            return;
        }
        let Some(endpoint) = &self.config.endpoint else {
            return;
        };
        let token = crate::llm::catalog::resolve_credential(
            &self.config.user_dir,
            &endpoint.base_url,
            endpoint.api_key_env.as_deref(),
        )
        .ok()
        .flatten();
        if let Some(loaded) = crate::llm::catalog::loaded_context(
            &endpoint.base_url,
            &self.config.model,
            token.as_deref(),
        ) {
            self.context_cap = Some(loaded);
        }
    }

    /// Tools actually offered: the harness selection minus anything unavailable in this context.
    fn offered(&self) -> Vec<String> {
        let catalog = self.catalog();
        let mut names: Vec<String> = self
            .version
            .harness
            .tools
            .iter()
            .chain(self.enabled_tools.iter())
            .filter(|n| catalog.contains(n))
            .cloned()
            .collect();
        // Completion is a control tool, including for older admitted harnesses that predate it.
        if !self.hermetic && catalog.iter().any(|name| name == "finish") {
            names.push("finish".into());
        }
        if catalog.iter().any(|name| name == "codebase_memory") {
            names.push("codebase_memory".into());
        }
        for tool in ["experience", "edit_preview", "tool_search", "tool_load"] {
            if catalog.iter().any(|n| n == tool) {
                names.push(tool.into());
            }
        }
        names.retain(|n| {
            !self.plan_mode
                || matches!(
                    n.as_str(),
                    "task"
                        | "bg"
                        | "lsp"
                        | "ast_edit"
                        | "working_state"
                        | "tool_search"
                        | "tool_load"
                )
                || approval::is_read_only(&self.ext, n, &json!({}))
        });
        if self.plan_mode {
            names.push("exit_plan".into());
        }
        names.sort();
        names.dedup();
        if self.experience.is_some()
            && self.config.assistance.compact_tools
            && self.config.uses_local_compute()
        {
            assistance::compact(
                &mut names,
                &self.enabled_tools,
                self.config.assistance.initial_tool_limit,
                &self.assistance.scores,
            );
        }
        // Browser previews need a server that survives a tool call. Add bg after compacting so
        // local models retain it too; exposing a tool starts no process and preserves opt-outs.
        if names.iter().any(|name| name == "browser")
            && catalog.iter().any(|name| name == "bg")
            && !names.iter().any(|name| name == "bg")
        {
            names.push("bg".into());
            names.sort();
        }
        names
    }

    fn schemas(&self, names: &[String]) -> Result<Vec<Value>> {
        let mode = tools::EditMode::from(self.edit_mode());
        let mut schemas = Vec::new();
        for name in names {
            schemas.push(match name.as_str() {
                n if tools::BUILTIN.contains(&n) => tools::schemas(&[n], mode)?.remove(0),
                n if self.ext.contains(n) => self.ext.get(n).map(|tool| tool.schema()).context("extension tool vanished")?,
                "experience" => assistance::schema(),
                "edit_preview" => crate::tools::preview::schema(),
                "todo" => todo::schema(),
                "finish" => completion::schema(),
                "task" => subagents::schema(self.config),
                "solve" => solve::schema(),
                "escalate" => solve::escalation_schema(),
                "skill" => skills::skill_tool::schema(),
                "skill_manage" => skills::manage_tool::schema(),
                "session_search" => session::session_search_tool::schema(),
                "artifact_read" => tools::artifact::schema(),
                "working_state" => evidence::working_schema(),
                "tool_search" => json!({"type":"function","function":{"name":"tool_search","description":"Discover specialist tools by capability or name. Discovery does not enable calls: load exact names and await the next model round.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}}),
                "tool_load" => json!({"type":"function","function":{"name":"tool_load","description":"Enable specialist tools for subsequent rounds; optionally unload names after a workflow to reduce schema overhead. Changes take effect next round, never in this batch.","parameters":{"type":"object","properties":{"names":{"type":"array","maxItems":32,"items":{"type":"string"}},"unload":{"type":"array","maxItems":32,"items":{"type":"string"}}},"additionalProperties":false}}}),
                "exit_plan" => json!({"type":"function","function":{"name":"exit_plan","description":"Ask the user to approve this plan and leave plan mode.","parameters":{"type":"object","properties":{"plan":{"type":"string","minLength":1}},"required":["plan"],"additionalProperties":false}}}),
                other => json!({"type":"function","function":{"name":other,
                    "description":"Operator-approved tool. Pass the request as `input`.",
                    "parameters":{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}}}),
            });
            if self.experience.is_some()
                && self.config.assistance.compact_tools
                && self.config.uses_local_compute()
            {
                let description = match name.as_str() {
                    "write" => Some(
                        "Write complete UTF-8 source to a workspace path; guarded syntax and normal approvals apply.",
                    ),
                    "bash" => Some(
                        "Run a shell command in the workspace. Commands execute code and use normal approvals; inspect exit status.",
                    ),
                    "glob" => Some("Find workspace paths with an ignore-aware glob."),
                    "grep" => Some(
                        "Search workspace source with a regular expression; provide path scope when known.",
                    ),
                    _ => None,
                };
                if let Some(description) = description {
                    schemas.last_mut().unwrap()["function"]["description"] = json!(description);
                }
            }
            if self.plan_mode {
                let schema = schemas.last_mut().expect("schema was just inserted");
                let properties = &mut schema["function"]["parameters"]["properties"];
                match name.as_str() {
                    "bg" => properties["action"]["enum"] = json!(["list", "output", "wait"]),
                    "ast_edit" => {
                        properties["dry_run"] =
                            json!({"type":"boolean","enum":[true],"default":true})
                    }
                    "task" => {
                        let agents: Vec<&str> = subagents::AGENTS
                            .iter()
                            .filter(|a| !a.mutates)
                            .map(|a| a.name)
                            .chain(self.config.subagents.keys().map(String::as_str))
                            .collect();
                        properties["agent"]["enum"] = json!(agents);
                        properties["tasks"]["items"]["properties"]["agent"]["enum"] = json!(agents);
                    }
                    _ => {}
                }
            }
        }
        Ok(schemas)
    }

    fn record(&self, step: u32, kind: &str, mut data: Value) -> Result<()> {
        if let Some(object) = data.as_object_mut() {
            object.insert(
                "wall_ms".into(),
                json!(self.workflow_started.elapsed().as_millis() as u64),
            );
        }
        self.workflow_metrics.borrow_mut().observe(kind, &data);
        if let Some(probe) = &self.step_probe {
            let risk = probe.risk(&crate::learn::probe::features(kind, step, &data));
            if let Some(object) = data.as_object_mut() {
                object.insert("predicted_failure_risk".into(), json!(risk));
            }
        }
        if let (Some(store), Some((id, domain, task))) = (self.store, &self.trajectory) {
            store.event(&Event {
                trajectory: *id,
                version: self.version.id,
                domain: domain.clone(),
                task: task.clone(),
                step,
                kind: kind.into(),
                data,
                score: None,
            })?;
        }
        Ok(())
    }

    fn save_request_policy(
        &mut self,
        selection: Option<Value>,
        result: Value,
        ui: &mut dyn Ui,
    ) -> Result<()> {
        if self.hermetic {
            return Ok(());
        }
        let mut control = self.session.control_state();
        if let Some(selection) = selection {
            let mut history = control["adaptive_history"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if control["adaptive_policy"].is_object() {
                history.push(control["adaptive_policy"].clone());
            }
            if history.len() > 12 {
                history.drain(..history.len() - 12);
            }
            control["adaptive_history"] = json!(history);
            control["adaptive_policy"] = json!({"selection":selection,"model":self.config.model,"version":self.version.id,"result":result});
        } else {
            control["adaptive_policy"]["result"] = result;
        }
        ui.request_policy(&control);
        self.session.set_control_state(control)
    }

    /// Append to the durable session; training capture happens at the inference boundary.
    fn push(&mut self, message: Value) -> Result<()> {
        self.session.append(message)?;
        Ok(())
    }

    /// One user message through to a final answer (or interruption / step limit).
    pub fn turn(&mut self, user_text: &str, ui: &mut dyn Ui) -> Result<TurnReport> {
        self.turn_with_state(user_text, ui, true)
    }
    /// RPC admission already cleared its interrupt before enqueueing; preserve a subsequent abort.
    pub fn turn_preserving_interrupt(
        &mut self,
        user_text: &str,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        self.turn_with_state(user_text, ui, false)
    }
    fn turn_with_state(
        &mut self,
        user_text: &str,
        ui: &mut dyn Ui,
        reset_interrupt: bool,
    ) -> Result<TurnReport> {
        self.usage_scope = self.usage_scope.next_turn(subagents::MAX_STEPS);
        if let Some(backend) = &mut self.backend {
            backend.set_usage_scope(Some(self.usage_scope.clone()));
        }
        for backend in self.role_backends.values_mut() {
            backend.set_usage_scope(Some(self.usage_scope.clone()));
        }
        let mut observed = state_ui::StateUi::new(ui, self.session.runtime(), None);
        self.live_tasks.bind(self.session.runtime());
        let result = self.turn_inner(user_text, &mut observed, reset_interrupt);
        observed.finish(!result.as_ref().is_ok_and(|turn| !turn.interrupted));
        match (result, self.session.flush_runtime()) {
            (Err(error), Err(journal)) => {
                Err(error.context(format!("execution journal also failed: {journal:#}")))
            }
            (result, Ok(())) => result,
            (Ok(_), Err(journal)) => Err(journal),
        }
    }
    fn turn_inner(
        &mut self,
        user_text: &str,
        ui: &mut dyn Ui,
        reset_interrupt: bool,
    ) -> Result<TurnReport> {
        ensure!(
            !user_text.trim().is_empty() && user_text.len() <= 262_144,
            "message must be 1..262144 bytes"
        );
        if reset_interrupt {
            self.interrupt.store(false, Ordering::SeqCst);
        }
        if !self.tool_hooks().is_empty() {
            let (verdict, notices) = hooks::fire(
                &self.config.hooks,
                self.workspace.root(),
                "user_prompt_submit",
                None,
                json!({"prompt":user_text,"session_id":self.session.id()}),
            );
            notices.iter().for_each(|n| ui.notice(n));
            match verdict {
                hooks::Verdict::Block { reason } => {
                    bail!("message blocked by a user_prompt_submit hook: {reason}")
                }
                hooks::Verdict::Continue { output } => {
                    if !output.is_empty() {
                        ui.notice(&format!("hook: {output}"));
                    }
                }
            }
        }
        if std::mem::take(&mut self.legacy_memory_migrated) {
            ui.notice("Migrated legacy memory tool selection to provenance memory tools for this runtime; admitted history is unchanged.");
        }
        self.task_policy = crate::learn::task_policy::Policy::for_prompt(
            user_text,
            self.config.reasoning_guard.max_pre_action_secs,
        );
        self.request_phase = crate::adaptive::State::begin(self.task_policy.kind, user_text);
        self.workflow_started = Instant::now();
        *self.workflow_metrics.borrow_mut() = Default::default();
        self.refresh_version(ui, user_text)?;
        // User-managed skill imports/toggles take effect between turns, including RPC and CLI changes.
        if let Some(layout) = &self.layout {
            let catalog = skills::catalog_block(&skills::discover(layout));
            if catalog != self.parts.skills {
                self.parts.skills = catalog;
                self.system = self.build_system();
            }
        }
        for notice in self.ext.take_notices() {
            ui.notice(&notice);
        }
        self.probe_context();
        self.step_probe = self
            .store
            .map(crate::learn::probe::load)
            .transpose()?
            .flatten()
            .filter(crate::learn::probe::informative);
        self.session.repair_incomplete_tools()?;
        let message = if let Some(layout) = &self.layout {
            let (message, warnings) = context::expand_message(&layout.project_root, user_text);
            for warning in warnings {
                ui.notice(&warning);
            }
            message
        } else {
            json!({"role":"user","content":user_text})
        };
        let checkpoint_turn = self
            .checkpoints
            .as_ref()
            .map(|checkpoints| checkpoints.begin_turn(&prompt::truncate_chars(user_text, 60)))
            .transpose()?;
        if !self.hermetic {
            let mut control = self.session.control_state();
            control["reliability"] = json!({
                "stats":crate::reliability::Stats::default(),
                "verification":"no checks recorded this turn",
                "interface":"standard",
                "automatic_passing_checks":0,
                "automatic_failing_checks":0,
                "edit_loop":{"phase":"no_edits","mutations":0,"checked_mutations":0,"diff_surfaced_mutations":0,"uncheckpointed_shell_commands":0,"disclosed_shell_commands":0,"uncheckpointed_extension_calls":0,"disclosed_extension_calls":0}
            });
            self.session.set_control_state(control.clone())?;
            ui.request_policy(&control);
        }
        self.step_checks = StepChecks {
            turn: checkpoint_turn,
            visual: if self.hermetic {
                visual::Review::default()
            } else {
                visual::Review::new(user_text)
            },
            ..StepChecks::default()
        };
        if !self.hermetic
            && message.get("dial_images").is_some()
            && self
                .backend
                .as_ref()
                .is_some_and(Backend::accepts_image_input)
        {
            self.step_checks.visual.captured((0, 0), false);
        }
        for reference in message["dial_images"].as_array().into_iter().flatten() {
            ui.image(reference);
        }
        self.repair_tracker.clear();
        self.repair_sequence = 0;
        self.ext.fire_lifecycle(&LifecycleEvent {
            event: "turn_started".into(),
            session_id: Some(self.session.id().to_owned()),
            turn_id: checkpoint_turn.map(|turn| turn.to_string()),
            tool_name: None,
            is_error: None,
        });
        self.trajectory = match self.store {
            Some(store) if self.version.id != 0 => {
                let task = self.domain_override.as_ref().map_or_else(
                    || format!("session:{}", self.session.id()),
                    |_| "selfplay".to_owned(),
                );
                let domain = self
                    .domain_override
                    .clone()
                    .unwrap_or_else(|| self.config.use_case.clone());
                let id = store.begin_trajectory(self.version.id, &domain, &task, user_text)?;
                Some((id, domain, task))
            }
            _ => None,
        };
        self.assistance.begin(user_text);
        self.training_capture.clear();
        self.verifications.clear();
        self.reasoning_recovery = reasoning::Recovery::default();
        self.logging = self.store.is_some();
        let mut message = message;
        // Recalled notes and verified repair patterns are decided once, for this message, and stored with it.
        // Recomputing them on every request (or putting them in the system prompt, as hints once were) would
        // change the text in front of the whole conversation and discard the provider's prompt cache.
        let mut context = self.experience_context(user_text, ui);
        if !self.hermetic && visual::useful(user_text) {
            context.push_str("\n\n");
            context.push_str(visual::INITIAL_GUIDANCE);
        }
        if let Some(memory) = &self.memory {
            // The lookup is a search query, not the prompt: a pasted file or log must not fail the turn.
            let query = prompt::truncate_chars(user_text, RECALL_QUERY_CHARS);
            let recalled = memory.context_lookup(
                &query,
                self.config.memory.context_limit,
                self.config.memory.context_max_bytes,
            )?;
            context.push_str(&recalled);
        }
        if let Some(saved) =
            self.codebase_context(&prompt::truncate_chars(user_text, RECALL_QUERY_CHARS), ui)
        {
            if !context.is_empty() {
                context.push_str("\n\n");
            }
            context.push_str(&saved);
        }
        if let Some(hints) = self.repair_context(user_text, ui) {
            if !context.is_empty() {
                context.push_str("\n\n");
            }
            context.push_str(&hints);
        }
        if !self.hermetic
            && let Some(store) = self.store
        {
            let repairs =
                store.recent_executable_repairs(&self.workspace.root().to_string_lossy(), 16)?;
            let terms: Vec<String> = user_text
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|term| term.len() >= 4)
                .take(32)
                .map(str::to_lowercase)
                .collect();
            let related: Vec<_> = repairs
                .into_iter()
                .filter(|(_, record)| {
                    let searchable = format!(
                        "{} {} {}",
                        record["before"]["objective"],
                        record["before"]["command"],
                        record["changes"]
                    )
                    .to_lowercase();
                    terms.iter().any(|term| searchable.contains(term))
                })
                .take(3)
                .collect();
            let mut hash_budget = 2 * 1024 * 1024;
            let matching: Vec<i64> = related
                .iter()
                .filter_map(|(trajectory, payload)| {
                    let record: repair_evidence::RepairRecord =
                        serde_json::from_value(payload.clone()).ok()?;
                    record
                        .matches_current(&self.workspace, &mut hash_budget)
                        .then_some(*trajectory)
                })
                .collect();
            if let Some(repairs) = repair_evidence::retrieval_text(&related, 4096) {
                if !context.is_empty() {
                    context.push_str("\n\n");
                }
                context.push_str(&repairs);
                context.push_str(&format!("\nExact failed-state file hashes currently match source trajectories {matching:?}. Other records are analogous or unchecked, not directly applicable. Re-run the executable check; file hashes do not verify the environment."));
            }
        }
        if !context.is_empty() {
            message["dial_recall"] = json!(context);
        }
        self.push(message)?;
        let mut state = self.session.control_state();
        if state["original_goal"].is_null() {
            state["original_goal"] = json!(prompt::truncate_chars(user_text, 4096));
        }
        state["turn_outcome"] = Value::Null;
        state["current_instruction"] = json!(prompt::truncate_chars(user_text, 4096));
        state["steering_active"] = json!(false);
        self.session.set_control_state(state)?;
        let review_start = self.session.messages().len().saturating_sub(1);
        if let Some(advice) = self.laya_advice("investigation", user_text, ui) {
            self.push(json!({"role":"user","content":format!("Laya retrieval advice (untrusted, advisory; deterministic investigation limits remain in force): {advice}")}))?;
        }
        let names = self.offered();
        let mut schemas = self.schemas(&names)?;
        self.last_trajectory = self.trajectory.as_ref().map(|(id, _, _)| *id);
        let max_steps = if !self.hermetic && self.config.harness.max_steps == 0 {
            0
        } else {
            self.version.harness.max_steps
        };
        if !self.hermetic
            && self.config.verification.enabled
            && matches!(
                self.task_policy.kind,
                crate::learn::task_policy::Kind::Editing
                    | crate::learn::task_policy::Kind::Debugging
            )
        {
            let baseline = self
                .config
                .verification
                .commands
                .iter()
                .filter(|check| {
                    self.config
                        .verification
                        .behavioral_checks
                        .contains(&check.name)
                })
                .take(8)
                .map(|check| verification::FocusedCheck {
                    name: check.name.clone(),
                    command: check.command.clone(),
                    timeout_secs: check.timeout_secs,
                })
                .collect::<Vec<_>>();
            if !baseline.is_empty() {
                ui.notice("Recording the configured behavioral baseline before edits.");
                let failures = self.run_checks(&baseline, ui);
                let baseline_context = if failures.is_empty() {
                    "Configured behavioral baseline passed before editing. Preserve these behaviors and rerun relevant checks after the change.".to_owned()
                } else {
                    format!(
                        "Behavioral baseline before editing (these failures predate this turn's edits):\n{}\nReproduce and fix failures within the requested scope; report unrelated failures without broadening the task.",
                        failures.join("\n")
                    )
                };
                self.record(
                    0,
                    "behavioral_baseline",
                    json!({"checks":baseline.len(),"failures":failures.len()}),
                )?;
                self.push(json!({"role":"user","content":baseline_context}))?;
            }
        }
        let mut backend = self.backend.take().context("backend is already in use")?;
        let mut result = self.run_loop(&mut backend, &names, &mut schemas, max_steps, ui);
        // Checks that already passed at the last `todo done`, with no edit since, need not run again.
        let already_verified = self.step_checks.clean();
        if self.config.verification.enabled
            && !already_verified
            && let (Ok(report), Some(turn)) = (&result, checkpoint_turn)
            && report.outcome == TurnOutcome::Completed
        {
            let mut checks = self.focused_checks(turn);
            self.step_checks.include_failed_checks(&mut checks);
            let mut failures = self.run_checks(&checks, ui);
            for attempt in 0..self.config.verification.repair_attempts {
                if failures.is_empty() || self.interrupt.load(Ordering::SeqCst) {
                    break;
                }
                self.metrics.repair_attempts += 1;
                self.record(0, "repair_round", json!({"attempt":attempt+1}))?;
                self.push(json!({"role":"user","content":format!(
                            "Automatic focused verification failed (repair attempt {}). Fix the diagnostics, then rerun the checks:\n{}",
                            attempt + 1, failures.join("\n")
                        )}))?;
                let repair_steps = if max_steps == 0 { 8 } else { max_steps.min(8) };
                let repaired =
                    self.run_loop(&mut backend, &names, &mut schemas, repair_steps, ui)?;
                if repaired.interrupted {
                    result = Ok(repaired);
                    break;
                }
                result = Ok(repaired);
                checks = self.focused_checks(turn);
                self.step_checks.include_failed_checks(&mut checks);
                failures = self.run_checks(&checks, ui);
            }
            if self.interrupt.load(Ordering::SeqCst) {
                let steps = result.as_ref().map_or(0, |report| report.steps);
                result = Ok(TurnReport {
                    text: String::new(),
                    outcome: TurnOutcome::Interrupted,
                    steps,
                    interrupted: true,
                    trajectory: None,
                });
            } else if !failures.is_empty() {
                let diagnostic = format!(
                    "Focused verification still fails after bounded repair: {}",
                    failures.join("; ")
                );
                ui.notice(&diagnostic);
                result = Err(anyhow::anyhow!(diagnostic));
            }
        }
        self.ext.fire_lifecycle(&LifecycleEvent {
            event: "turn_finished".into(),
            session_id: Some(self.session.id().to_owned()),
            turn_id: checkpoint_turn.map(|turn| turn.to_string()),
            tool_name: None,
            is_error: Some(result.is_err()),
        });
        self.backend = Some(backend);
        self.session.repair_incomplete_tools()?;
        if !self.hermetic
            && self.config.laya.enabled
            && !self.interrupt.load(Ordering::SeqCst)
            && self.step_checks.mutations > 0
            && ui.maintenance_ready()
        {
            let messages = self.session.messages();
            let mut recent = messages
                .iter()
                .skip(review_start)
                .filter(|m| m["role"] == "tool" || m["role"] == "assistant")
                .rev()
                .take(12)
                .map(|m| prompt::truncate_chars(&m.to_string(), 700))
                .collect::<Vec<_>>();
            recent.reverse();
            let claim = result
                .as_ref()
                .map(|r| r.text.as_str())
                .unwrap_or("Task did not complete successfully.");
            let packet = format!(
                "Task: {}\nMutation count: {}\nObserved checks passed flags (not proof of coverage): {:?}\nChronological recent tool evidence:\n{}\nProposed completion: {}",
                prompt::truncate_chars(user_text, 700),
                self.step_checks.mutations,
                self.verifications,
                recent.join("\n"),
                prompt::truncate_chars(claim, 1800)
            );
            let _ = self.laya_advice("post-edit", &packet, ui);
        }
        if let Ok(report) = &result {
            let mut state = self.session.control_state();
            state["turn_outcome"] = json!(report.outcome);
            self.session.set_control_state(state)?;
        }
        if let Some(plan) = self.pending_plan.take() {
            self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
        }
        if let (Ok(report), false) = (&result, self.tool_hooks().is_empty()) {
            let payload = json!({"session_id":self.session.id(),"outcome":format!("{:?}", report.outcome),"answer":prompt::truncate_chars(&report.text, 4000)});
            let (_, notices) = hooks::fire(
                &self.config.hooks,
                self.workspace.root(),
                "stop",
                None,
                payload,
            );
            notices.iter().for_each(|n| ui.notice(n));
        }
        if result
            .as_ref()
            .is_ok_and(|report| report.outcome == TurnOutcome::Completed)
        {
            self.finish_experience(ui);
            self.save_investigation_findings(ui);
            // The default architecture model is local MLX. With a cloud main model no compute is held for
            // the turn, so synthesis is skipped rather than making every turn wait for training.
            let arch_local = self.config.for_role("architecture").endpoint.is_none();
            if !self.hermetic
                && self.config.codebase.synthesize_architecture
                && (!arch_local || self.config.uses_local_compute())
                && self.config.for_role("architecture").context_window >= 4096
                && !self.interrupt.load(Ordering::SeqCst)
                && ui.maintenance_ready()
                && let Err(error) = self.synthesize_architecture(false, ui)
            {
                ui.notice(&format!("Architecture update deferred: {error:#}"));
            }
        }
        if let Some((id, _, _)) = self.trajectory.take() {
            if let Some(store) = self.store {
                match &result {
                    Ok(report) => {
                        store.finish_trajectory(id, Some(&report.text), None)?;
                        if report.outcome == TurnOutcome::Completed
                            && let Err(error) = self.persist_turn(store, id)
                        {
                            ui.notice(&format!("training observation was not saved: {error:#}"));
                        }
                    }
                    Err(error) => store.finish_trajectory(id, None, Some(&format!("{error:#}")))?,
                }
            }
            if let Ok(report) = &result {
                return Ok(TurnReport {
                    trajectory: Some(id),
                    ..result_clone(report)
                });
            }
        }
        result
    }

    fn save_investigation_findings(&self, ui: &mut dyn Ui) {
        let Some(cache) = self.codebase.clone() else {
            return;
        };
        let findings: Vec<_> = self
            .todo
            .tasks()
            .iter()
            .filter(|task| task.investigation && task.status == todo::Status::Completed)
            .filter_map(|task| {
                Some((
                    task.content.clone(),
                    task.finding.clone()?,
                    task.evidence.clone(),
                ))
            })
            .rev()
            .take(3)
            .collect();
        if findings.is_empty() {
            return;
        }
        let session = self.session.id().to_owned();
        thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let saved = findings
                    .iter()
                    .filter(|(question, finding, refs)| {
                        cache
                            .remember_investigation(question, finding, refs, &session)
                            .unwrap_or(false)
                    })
                    .count();
                let _ = tx.send(saved);
            });
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(count) => {
                        if count > 0 {
                            ui.notice(&format!("Saved {count} source-linked investigation findings for future sessions."));
                        }
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => ui.heartbeat(),
                    Err(_) => break,
                }
            }
        });
    }

    /// Retain observations, not self-assigned rewards. A model-selected shell command can spoof a
    /// successful test invocation; only a separate verifier or user verdict supplies training labels.
    fn persist_turn(&self, store: &Store, trajectory: i64) -> Result<()> {
        store.save_messages(trajectory, &self.training_capture.payload())
    }

    fn note_verification(&mut self, call: &ToolCall, output: &ToolOutput, ui: &mut dyn Ui) {
        let Some(command) = call.arguments["command"].as_str() else {
            return;
        };
        let configured = self
            .config
            .verification
            .commands
            .iter()
            .find(|check| check.command == command);
        if configured.is_none()
            && !is_check_command(command)
            && !call.id.starts_with("focused-check:")
        {
            return;
        }
        let kind = if configured.is_some_and(|check| {
            self.config
                .verification
                .behavioral_checks
                .contains(&check.name)
        }) {
            repair_evidence::CheckKind::Behavioral
        } else {
            repair_evidence::CheckKind::SyntaxOnly
        };
        let exit = verification::exit_code(&output.text);
        let passed = !output.is_error && exit == Some(0);
        if !self.hermetic
            && matches!(kind, repair_evidence::CheckKind::Behavioral)
            && let Some(cache) = self.codebase.clone()
        {
            let command = command.to_owned();
            let _ = thread::scope(|scope| -> Result<_> {
                let (tx, rx) = mpsc::sync_channel(1);
                scope.spawn(move || {
                    let _ = tx.send(cache.record_behavior_check(&command, passed));
                });
                receive_with_heartbeat(&rx, ui)
            });
        }
        self.assistance.check(
            command,
            passed
                && !verification::zero_tests(&output.text)
                && matches!(kind, repair_evidence::CheckKind::Behavioral),
        );
        self.verifications.push(passed);
        let _ = self.record(
            self.cur_step,
            "verification",
            json!({
                "command":truncate(command, 200),"exit":exit,"passed":passed,"category":kind
            }),
        );
        if self.hermetic {
            return;
        }
        let (Some(store), Some(checkpoints), Some(turn), Some(artifacts), Some(trajectory)) = (
            self.store,
            self.checkpoints.as_ref(),
            self.step_checks.turn,
            self.workspace.artifacts(),
            self.trajectory.as_ref().map(|(id, _, _)| *id),
        ) else {
            return;
        };
        self.repair_sequence += 1;
        let captured = (|| -> Result<()> {
            let mut changes = checkpoints.repair_changes(turn)?;
            // Private checkpoint files are outside the read tool's workspace. Publish immutable
            // source bytes through artifact_read rather than hand the model unusable filesystem paths.
            for change in &mut changes {
                for reference in [&mut change.before_ref, &mut change.after_ref]
                    .into_iter()
                    .flatten()
                {
                    let mut source = std::fs::File::open(reference.as_str())?;
                    let mut spool = artifacts.spool()?;
                    std::io::copy(&mut source, &mut spool)?;
                    *reference = spool.finish()?;
                }
            }
            let evidence_ref = artifacts.retain(&output.text)?;
            let run = repair_evidence::CheckRun {
                command: command.to_owned(),
                cwd: self.workspace.root().to_string_lossy().into_owned(),
                objective: configured
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "Syntax, build, or undeclared check only".into()),
                kind,
                tool_call_id: format!("{}:{}:{}", self.session.id(), call.id, self.repair_sequence),
                evidence_ref,
                exit_code: if output.is_error && exit == Some(0) {
                    None
                } else {
                    exit
                },
                sequence: self.repair_sequence,
            };
            if let Some(record) = self.repair_tracker.observe(run, changes) {
                store.executable_repair(
                    trajectory,
                    self.version.id,
                    &serde_json::to_value(record)?,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = captured {
            self.repair_tracker.clear();
            let _ = self.record(
                self.cur_step,
                "repair_evidence_unavailable",
                json!({"error":format!("{error:#}")}),
            );
        }
    }

    /// Source hashing and indexing never run on the interactive input/render thread.
    fn laya_advice(&mut self, phase: &str, evidence: &str, ui: &mut dyn Ui) -> Option<String> {
        if self.hermetic || !self.config.laya.enabled || self.interrupt.load(Ordering::SeqCst) {
            return None;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if self.session.control_state()["laya"]["retry_after"]
            .as_u64()
            .is_some_and(|retry| now < retry)
        {
            return None;
        }
        if phase != "investigation"
            && matches!(
                self.session.control_state()["laya"]["status"].as_str(),
                Some("unavailable" | "loading")
            )
        {
            return None;
        }
        ui.laya_activity(
            "checking",
            if phase == "investigation" {
                "Choosing a retrieval scope · advisory"
            } else {
                "Reviewing post-edit evidence · advisory"
            },
        );
        let config = self.config.laya.clone();
        let cancel = self.interrupt.clone();
        let evidence = evidence.to_owned();
        let phase_owned = phase.to_owned();
        let result = thread::scope(|scope| -> Result<_> {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let _ = tx.send(laya::predict(&config, &phase_owned, &evidence, cancel));
            });
            receive_with_heartbeat(&rx, ui)?
        });
        let mut state = self.session.control_state();
        match result {
            Ok(decision) => {
                let summary = laya::summary(phase, &decision.answers);
                let record = json!({"status":"ready","mode":"advisory","phase":phase,"checkpoint":decision.health.checkpoint,"device":decision.health.device,"answers":decision.answers,"latency_ms":decision.elapsed_ms,"calibration":"raw; not calibrated in Dial","summary":summary});
                let _ = self.record(self.cur_step, "laya_decision", record.clone());
                state["laya"] = record;
                let _ = self.session.set_control_state(state);
                ui.laya_activity(
                    "advice",
                    &format!(
                        "{summary}\nCheckpoint: {} · {} ms",
                        decision.health.checkpoint, decision.elapsed_ms
                    ),
                );
                Some(summary)
            }
            Err(error) => {
                let reason = if self.interrupt.load(Ordering::SeqCst) {
                    "Cancelled".to_owned()
                } else {
                    format!("{error:#}")
                };
                let retry_after =
                    if reason.contains("exceeded its") || reason.contains("Laya is busy") {
                        Some(now.saturating_add(60))
                    } else {
                        None
                    };
                let status = if reason.contains("exceeded its") {
                    "delayed"
                } else if reason.contains("Laya is busy") {
                    "busy"
                } else if reason.contains("loading its checkpoint") {
                    "loading"
                } else {
                    "unavailable"
                };
                state["laya"] = json!({"status":status,"phase":phase,"reason":reason,"retry_after":retry_after});
                let _ = self.session.set_control_state(state);
                let _ = self.record(
                    self.cur_step,
                    "laya_unavailable",
                    json!({"phase":phase,"reason":reason}),
                );
                ui.laya_activity(
                    status,
                    &format!(
                        "{reason}. Coding remains available; deterministic checks stay active."
                    ),
                );
                None
            }
        }
    }

    pub fn laya_status(&mut self, probe: bool, start: bool, ui: &mut dyn Ui) -> Result<String> {
        let mut text = format!(
            "Laya: {} · advisory decisions\nService: {}\nRequest timeout: {}s\n",
            if self.config.laya.enabled {
                "enabled"
            } else {
                "disabled (enable Decision advice in /settings → Laya)"
            },
            self.config.laya.endpoint,
            self.config.laya.timeout_secs
        );
        if probe || start {
            ui.laya_activity(
                if start { "loading" } else { "checking" },
                if start {
                    "Starting or waiting for Dial's managed service · Esc cancels waiting"
                } else {
                    "Checking the existing service; no prediction or model loading"
                },
            );
            let config = self.config.laya.clone();
            let cancel = self.interrupt.clone();
            let health = thread::scope(|scope| -> Result<_> {
                let (tx, rx) = mpsc::sync_channel(1);
                scope.spawn(move || {
                    let _ = tx.send(if start {
                        laya::ensure_service(&config, cancel, true)
                    } else {
                        laya::health(&config, cancel)
                    });
                });
                receive_with_heartbeat(&rx, ui)?
            });
            match health {
                Ok(health)=>text.push_str(&format!("Live service: {}\nCheckpoint: {}\nDevice: {}\n",health.status,health.checkpoint,health.device)),
                Err(error)=>text.push_str(&format!("Live service unavailable: {error:#}\nUse /laya start, or install the independent runtime with dial laya install --checkpoint PATH.\n")),
            }
        }
        let latest = &self.session.control_state()["laya"];
        if !latest.is_null() {
            text.push_str(&format!(
                "Last session observation (not a live probe): {}\n",
                serde_json::to_string_pretty(latest)?
            ));
        }
        text.push_str("Laya does not write architecture summaries or certify behavior. /laya check probes the service; /understanding architecture inspects memory.\n");
        Ok(text)
    }

    fn codebase_context(&mut self, query: &str, ui: &mut dyn Ui) -> Option<String> {
        let cache = self.codebase.as_ref()?.clone();
        let workspace = self.workspace.clone();
        let cancel = self.interrupt.clone();
        // Initial context is an index; explicit retrieval retains its own budget.
        let max_bytes = self.config.codebase.context_max_bytes.min(8192);
        let result = thread::scope(|scope| -> Result<_> {
            let (tx, rx) = mpsc::sync_channel(1);
            let worker = scope.spawn(move || {
                let result = cache.refresh(&workspace, &cancel).and_then(|stats| {
                    let architecture = cache.architecture_context(query, max_bytes / 2)?;
                    let context = cache.context(
                        query,
                        max_bytes.saturating_sub(architecture.len()).max(1024),
                    )?;
                    Ok((stats, format!("{architecture}{context}")))
                });
                let _ = tx.send(result);
            });
            let result = receive_with_heartbeat(&rx, ui);
            if worker.join().is_err() {
                bail!("codebase context worker panicked");
            }
            result?
        });
        match result {
            Ok((stats, context)) => {
                if stats.indexed > 0 || stats.removed > 0 {
                    ui.diagnostic(&format!("Project understanding: indexed {} changed/new sources, reused {}, removed {}{}.",
                        stats.indexed, stats.reused, stats.removed, if stats.partial { "; scan coverage is partial" } else { "" }));
                }
                Some(context)
            }
            Err(_) if self.interrupt.load(Ordering::SeqCst) => None,
            Err(error) => {
                ui.notice(&format!("saved codebase context unavailable: {error:#}"));
                None
            }
        }
    }

    /// Bounded maintenance of project knowledge using the already-selected backend.
    /// It has no tools, cannot edit files, and never becomes a verified training example.
    pub fn synthesize_architecture(&mut self, force: bool, ui: &mut dyn Ui) -> Result<()> {
        let cache = self
            .codebase
            .clone()
            .context("codebase memory is disabled")?;
        let role_config = self.config.for_role("architecture");
        let separate =
            Some(role_config.selection()).filter(|selection| *selection != self.config.selection());
        let context_window = if separate.is_some() {
            role_config.context_window
        } else {
            self.context_window()
        };
        ensure!(
            context_window >= 4096,
            "architecture synthesis needs a model context window of at least 4096 tokens"
        );
        let workspace = self.workspace.clone();
        let cancel = self.interrupt.clone();
        let budget = (context_window as usize).clamp(4096, 24000);
        let synthesis = thread::scope(|scope| -> Result<_> {
            let (tx, rx) = mpsc::sync_channel(1);
            let cache = cache.clone();
            scope.spawn(move || {
                let _ = tx.send(
                    cache
                        .refresh(&workspace, &cancel)
                        .and_then(|_| cache.architecture_synthesis(budget, force)),
                );
            });
            receive_with_heartbeat(&rx, ui)?
        })?;
        let Some(synthesis) = synthesis else {
            if force {
                ui.notice(
                    "No eligible indexed source evidence is available for architecture synthesis.",
                );
            }
            return Ok(());
        };
        if self.interrupt.load(Ordering::SeqCst) {
            return Ok(());
        }
        ui.notice("Updating saved architecture from bounded source evidence (one model call, up to 60 seconds; Esc cancels).");
        let request = Chat {
            messages: vec![
                json!({"role":"system","content":crate::storage::codebase::architecture::SYSTEM}),
                json!({"role":"user","content":synthesis.prompt}),
            ],
            tools: vec![],
            temperature: 0.0,
            max_tokens: (context_window / 3).clamp(512, 2048),
            seed: None,
        };
        ensure!(self.backend.is_some(), "backend is already in use");
        // Construct the role backend before taking the main one so setup failures leave coding usable.
        let role_backend = if separate.is_some() {
            Some(match self.role_backends.remove("architecture") {
                Some(backend) => backend,
                None => make_backend(&role_config)?,
            })
        } else {
            None
        };
        // Setup failures must not suppress maintenance for unchanged sources after the user fixes
        // model availability. Mark an attempt only once a backend has been constructed.
        cache.mark_architecture_attempt(&synthesis)?;
        let mut main = self.backend.take();
        let mut backend = if let Some(backend) = role_backend {
            main.as_mut().expect("checked main backend").release();
            backend
        } else {
            main.take().expect("checked main backend")
        };
        if let Some(selection) = &separate {
            ui.role("architecture", &selection.display_name());
        }
        let adapter = if separate.is_some() {
            None
        } else {
            self.version.adapter.clone()
        };
        let external = self.interrupt.clone();
        let local = Arc::new(AtomicBool::new(false));
        let mut yielded = false;
        let reply = thread::scope(|scope| {
            let (done, wait) = mpsc::channel::<()>();
            let local_watch = local.clone();
            scope.spawn(move || {
                let start = Instant::now();
                loop {
                    if external.load(Ordering::SeqCst) || start.elapsed() >= Duration::from_secs(60)
                    {
                        local_watch.store(true, Ordering::SeqCst);
                        break;
                    }
                    if !matches!(
                        wait.recv_timeout(Duration::from_millis(40)),
                        Err(RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
            });
            let yield_cancel = local.clone();
            let reply = chat_with_ui(
                &mut backend,
                adapter.as_deref(),
                &request,
                60,
                local,
                false,
                None,
                ui,
                Some(&self.config.reasoning_guard),
                &mut |ui| {
                    if !force && !ui.maintenance_ready() {
                        yielded = true;
                        yield_cancel.store(true, Ordering::SeqCst);
                    }
                },
            );
            drop(done);
            reply
        });
        if let Ok(reply) = &reply
            && let Some(cost) = backend.reply_cost(reply)
        {
            self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost);
        }
        if separate.is_some() {
            backend.release();
            self.role_backends.insert("architecture".into(), backend);
            self.backend = main;
            ui.role("main", &self.config.selection().display_name());
        } else {
            self.backend = Some(backend);
        }
        if yielded {
            cache.defer_architecture_attempt(&synthesis)?;
            ui.notice("Architecture maintenance yielded to queued work.");
            return Ok(());
        }
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                cache.defer_architecture_attempt(&synthesis)?;
                return Err(error);
            }
        };
        self.record(0,"architecture_synthesis",json!({"completion_tokens":reply.completion_tokens,"prompt_tokens":reply.prompt_tokens}))?;
        ensure!(
            reply.tool_calls.is_empty(),
            "architecture synthesis unexpectedly requested tools"
        );
        let model = role_config.model.clone();
        let saved = thread::scope(|scope| -> Result<_> {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let _ = tx.send(cache.save_architecture(&synthesis, &reply.text, &model));
            });
            receive_with_heartbeat(&rx, ui)?
        })?;
        ui.notice(&format!("Saved {saved} architecture sections with source references. Inspect /understanding architecture."));
        Ok(())
    }

    fn run_loop(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        schemas: &mut Vec<Value>,
        max_steps: u32,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        let mut names = names.to_vec();
        let mut reviewed_paths = std::collections::BTreeSet::new();
        let mut prechecked_paths = std::collections::BTreeSet::new();
        let mut revalidated_paths = std::collections::BTreeSet::new();
        let h = self.config.effective_harness(&self.version.harness)?;
        let adapter = self.version.adapter.clone();
        let mut pending: Option<Pending> = None;
        let mut overflow_retried = false;
        let mut compaction_failed = false;
        let mut max_tokens = h.max_tokens;
        let mut length_nudges = 0u32;
        let mut recovering_output = false;
        let mut unusable_replies = 0u32;
        let mut reasoning_budget = None;
        let mut repeats = guard::RepeatGuard::default();
        let mut recovery_checkpoint = recovery_checkpoint::RecoveryCheckpoint::default();
        let mut investigation = investigation::InvestigationGuard::default();
        let mut unplanned_inspections = 0usize;
        let mut planning_notice = false;
        let measured = if self.hermetic
            || adapter.is_some()
            || self.config.steering.enabled
            || crate::steering::specification(self.config, &self.config.model)
                .ok()
                .flatten()
                .is_some()
        {
            None
        } else {
            crate::reliability::measured(self.config)
        };
        let mut reliability: crate::reliability::Stats =
            serde_json::from_value(self.session.control_state()["reliability"]["stats"].clone())
                .unwrap_or_default();
        let turn_started = self.workflow_started;
        if !self.hermetic {
            let mut control = self.session.control_state();
            control["reliability"]["measured"] = json!(measured);
            self.session.set_control_state(control.clone())?;
            ui.request_policy(&control);
        }
        let mut policy_signature = String::new();
        let mut stall = guard::StallGuard::default();
        let mut completion = completion::CompletionGuard::default();
        let mut narration = guard::NarrationGuard::default();
        let mut steering_pending = false;
        let mut budget = guard::StepBudget::new(
            max_steps,
            if self.hermetic {
                0
            } else {
                guard::StepBudget::FREE_RETRIES
            },
        );
        let mut iteration = 0u32;
        let turn_cost_start = self.session_cost.unwrap_or(0.0);
        while budget.begin() {
            let step = iteration;
            iteration = iteration.saturating_add(1);
            if !self.hermetic {
                let spent = self.session_cost.unwrap_or(0.0);
                let caps = &self.config.budget;
                let hit = match (caps.max_session_cost_usd, caps.max_turn_cost_usd) {
                    (Some(cap), _) if spent >= cap => Some(format!(
                        "session cost ${spent:.4} reached the ${cap:.4} budget"
                    )),
                    (_, Some(cap)) if spent - turn_cost_start >= cap => Some(format!(
                        "this turn cost ${:.4}, reaching its ${cap:.4} budget",
                        spent - turn_cost_start
                    )),
                    _ => None,
                };
                if let Some(why) = hit {
                    return self.stop_turn(
                        ui,
                        format!("Stopped: {why}. Raise [budget] in the configuration to continue."),
                        step,
                    );
                }
            }
            if self.take_steering(ui)? {
                steering_pending = true;
            }
            if std::mem::take(&mut steering_pending) {
                completion.reset_for_steering();
                recovery_checkpoint = recovery_checkpoint::RecoveryCheckpoint::default();
                investigation = Default::default();
                repeats = Default::default();
                stall = Default::default();
                narration = Default::default();
                pending = None;
                unusable_replies = 0;
                recovering_output = false;
                length_nudges = 0;
                max_tokens = h.max_tokens;
                reasoning_budget = None;
                unplanned_inspections = 0;
                planning_notice = false;
                reliability.consecutive_contract_failures = 0;
                names = self.offered();
                *schemas = self.schemas(&names)?;
            }
            self.pump_subagents(ui);
            self.cur_step = step;
            if self.interrupt.load(Ordering::SeqCst) {
                return Ok(TurnReport {
                    text: String::new(),
                    outcome: TurnOutcome::Interrupted,
                    steps: step,
                    interrupted: true,
                    trajectory: None,
                });
            }
            if !compaction_failed
                && self.projected_prompt_tokens() > (f64::from(self.context_window()) * 0.8) as u64
            {
                // A failed summary must not kill the turn: the model may still fit. Try once per turn.
                if let Err(error) = self.compact_using(backend, ui) {
                    compaction_failed = true;
                    ui.notice(&format!(
                        "context compaction could not make enough room ({error:#}); continuing"
                    ));
                }
            }
            let mut messages = vec![json!({"role":"system","content":self.system.clone()})];
            self.session.extend_messages(&mut messages);
            // Everything before the newest messages must stay byte-identical from step to step so a
            // provider or local server can reuse its prompt cache; see `evidence::project`.
            let pressure =
                self.projected_prompt_tokens() > (f64::from(self.context_window()) * 0.5) as u64;
            let policy = evidence::Policy {
                warm_tokens: CACHE_WARM_SUFFIX_TOKENS,
                pressure,
                protect_tokens: (self.context_window() as usize / 5).max(PRUNE_PROTECT_MIN_TOKENS),
            };
            let verification_objective = if self.config.verification.behavioral_checks.is_empty() {
                "No operator-declared behavioral check. Syntax/build success alone does not establish task correctness.".to_owned()
            } else {
                format!(
                    "Behavioral checks: {}",
                    self.config.verification.behavioral_checks.join(", ")
                )
            };
            let history = self
                .session
                .path()
                .map(|p| p.to_string_lossy().into_owned());
            let pre_action_secs = if step == 0 {
                self.task_policy.initial_reasoning_secs
            } else {
                self.config.reasoning_guard.max_pre_action_secs
            };
            let selection = crate::adaptive::select(
                self.config,
                &self.request_phase,
                crate::adaptive::Request {
                    kind: self.task_policy.kind,
                    temperature: h.temperature,
                    tokens: reasoning_budget.unwrap_or(max_tokens),
                    pre_action_secs,
                    base_profile: backend.model_profile(),
                    recovering_output: recovering_output || reasoning_budget.is_some(),
                    hermetic: self.hermetic,
                },
            );
            let packet = if self.hermetic {
                None
            } else {
                self.task_packet.project(
                    &mut messages,
                    &self.todo,
                    &self.session.control_state(),
                    self.session.id(),
                    history.as_deref(),
                    &verification_objective,
                    (self.context_window() as usize / 4)
                        .clamp(1024, 4096)
                        .min(selection.context_bytes),
                )
            };
            evidence::project(&mut messages, &mut self.superseded, &policy);
            let sent_messages = messages.len() - 1;
            // Keep dynamic task context at the request tail, never in the cached system prefix.
            let focus = messages
                .last()
                .filter(|m| m["role"] == "tool")
                .filter(|_| packet.is_none())
                .and_then(|_| self.todo.focus());
            let mut task_context = focus.unwrap_or_default();
            if let Some(packet) = packet {
                task_context.push_str(&format!(
                    "\n[Current task evidence packet; harness context]\n{packet}"
                ));
            }
            if self.session.control_state()["steering_active"] == true {
                task_context.push_str("\n[User steering from Dial] Follow the latest user instruction before taking another action. New user corrections take precedence over conflicting older plans, todos, original goals and summaries. Keep compatible unfinished work, but revise or drop conflicting todos instead of continuing the old task. The user's full messages are authoritative; task packets are context, not permission to ignore them.");
            }
            if !task_context.is_empty() {
                messages.push(json!({"role":"user","content":task_context}));
            }
            // Short questions need no plan. Extended inspection must enter the real todo workflow,
            // not produce another prose promise. Keep this instruction at the request tail.
            let planning_required = !self.hermetic
                && unplanned_inspections >= self.task_policy.inspection_limit
                && !self.todo.has_open()
                && schemas.iter().any(|s| s["function"]["name"] == "todo");
            if planning_required {
                if !planning_notice {
                    ui.notice("Investigation checkpoint: create a todo plan to resume tools. The next request offers only todo.");
                }
                messages.push(json!({"role":"user","content":
                    "Investigation checkpoint: the initial inspection allowance has been used without an active plan. If the request is already answered, answer directly. Otherwise your next tool call must create actual todos: use todo set for an empty list, or append to preserve existing tasks. Add 1-3 concrete unresolved questions with investigation=true, then the known implementation and verification steps. Do not invent a large speculative plan. As a result reveals necessary follow-on work, append that task before pursuing it; start it explicitly if it is a prerequisite. Close investigation questions with finding and evidence. A prose plan is not a todo update."}));
                schemas.retain(|s| s["function"]["name"] == "todo");
                names.retain(|name| name == "todo");
            }
            planning_notice = planning_required;
            recovery_checkpoint.apply(schemas, &mut names, &mut messages);
            let single_call = !self.hermetic
                && crate::reliability::single_call(self.config, measured.as_ref(), &reliability);
            let contract = crate::reliability::Contract::new(std::mem::take(schemas))?;
            let mut offered_schemas = contract.schemas;
            if single_call {
                crate::reliability::simplify(&mut offered_schemas, &self.enabled_tools);
            }
            let snapshot = crate::reliability::Contract::new(offered_schemas.clone())?;
            names = snapshot.names.clone();
            if !self.hermetic && self.config.reliability.enabled {
                let guidance = snapshot.instruction(single_call, self.config.reliability.examples);
                if let Some(first) = offered_schemas.first_mut() {
                    let description = first["function"]["description"].as_str().unwrap_or("");
                    first["function"]["description"] =
                        json!(format!("{description}\n\n{guidance}"));
                }
            }
            let request = Chat {
                messages,
                tools: offered_schemas,
                temperature: selection.temperature.unwrap_or(h.temperature),
                max_tokens: selection.max_tokens,
                seed: if self.hermetic {
                    Some(42 + u64::from(step))
                } else {
                    None
                },
            };
            let requested_tokens = request.max_tokens;
            if !self.hermetic {
                let mut profile = selection.profile.clone();
                if single_call
                    && let Some(profile) = profile.as_mut()
                    && profile.capabilities.parallel_tool_calls.is_some()
                {
                    profile.capabilities.parallel_tool_calls = Some(false);
                }
                backend.next_request_profile(profile);
                let signature = json!([
                    selection.enabled,
                    selection.phase,
                    selection.temperature,
                    selection.top_p,
                    selection.max_tokens,
                    selection.pre_action_secs,
                    selection.reasoning_effort
                ])
                .to_string();
                if signature != policy_signature {
                    policy_signature = signature;
                    if selection.enabled {
                        ui.diagnostic(&format!("Policy: {} → {} · temp {} · output {} · pre-action {}s · /policy shows why",selection.task,selection.phase.label(),selection.temperature.map(|t|t.to_string()).unwrap_or("provider-controlled".into()),selection.max_tokens,selection.pre_action_secs));
                    }
                }
                let mut control = self.session.control_state();
                control["reliability"]["interface"] = json!(if single_call {
                    "compact single call"
                } else {
                    "standard"
                });
                control["reliability"]["current_tools"] = json!(names);
                self.session.set_control_state(control)?;
                self.record(step, "request_policy", serde_json::to_value(&selection)?)?;
                self.save_request_policy(
                    Some(serde_json::to_value(&selection)?),
                    json!({"state":"awaiting response"}),
                    ui,
                )?;
            }
            let request_estimate = context_meter::estimate_request(&request);
            self.record(
                step,
                "request_overhead",
                serde_json::to_value(context_meter::overhead(&request))?,
            )?;
            let request_context = self.context_meter.predict(request_estimate);
            let started = Instant::now();
            let timeout = self.config.worker_timeout_secs;
            let mut reasoning_policy = self.config.reasoning_guard.clone();
            reasoning_policy.max_pre_action_secs = selection.pre_action_secs;
            let interrupt = self.interrupt.clone();
            // Steering cancels only this generation, never the whole user turn or
            // independent workers that share the ordinary interrupt flag.
            let generation_cancel = Arc::new(AtomicBool::new(false));
            let mut steered_during_generation = false;
            let mut steering_error = None;
            let result = if self.hermetic {
                crate::learn::compute::background_call(self.config, backend, |backend, cancel| {
                    reasoning::chat(
                        backend,
                        adapter.as_deref(),
                        &request,
                        timeout,
                        if crate::learn::compute::can_yield() {
                            cancel
                        } else {
                            interrupt.clone()
                        },
                        &reasoning_policy,
                        &mut |_| true,
                        &mut |_| {},
                        &mut |_| true,
                    )
                })
            } else {
                chat_with_ui(
                    backend,
                    adapter.as_deref(),
                    &request,
                    timeout,
                    generation_cancel.clone(),
                    true,
                    Some(request_context),
                    ui,
                    Some(&reasoning_policy),
                    &mut |ui| {
                        self.pump_subagents(ui);
                        if self.interrupt.load(Ordering::SeqCst) {
                            generation_cancel.store(true, Ordering::SeqCst);
                        }
                        match self.take_steering(ui) {
                            Ok(true) => {
                                steered_during_generation = true;
                                generation_cancel.store(true, Ordering::SeqCst);
                            }
                            Ok(false) => {}
                            Err(error) => {
                                steering_error = Some(error);
                                generation_cancel.store(true, Ordering::SeqCst);
                            }
                        }
                    },
                )
            };
            // Also clear a profile if the model worker failed before entering the provider.
            backend.next_request_profile(None);
            let request_tools = request.tools.clone();
            *schemas = request.tools;
            let context_before = request.messages;
            if let Some(error) = steering_error {
                return Err(error);
            }
            if steered_during_generation && !self.interrupt.load(Ordering::SeqCst) {
                steering_pending = true;
                self.record(
                    step,
                    "steering_superseded_generation",
                    json!({"elapsed_ms":started.elapsed().as_millis()}),
                )?;
                ui.notice("Stopped the old generation; replanning with your new instruction.");
                budget.give_back();
                continue;
            }
            let reply = match result {
                Ok(reply) => reply,
                Err(error)
                    if error.downcast_ref::<reasoning::GateHit>().is_some()
                        && !self.interrupt.load(Ordering::SeqCst) =>
                {
                    reasoning_budget = Some(requested_tokens);
                    self.request_phase
                        .recover("reasoning gate intercepted the previous request");
                    self.save_request_policy(None,json!({"state":"reasoning gate","elapsed_ms":started.elapsed().as_millis()}),ui)?;
                    self.metrics.reasoning_interceptions += 1;
                    self.record(step, "reasoning_gate", json!({"reason":error.to_string(),"elapsed_ms":started.elapsed().as_millis()}))?;
                    match self.reasoning_recovery.intercept(&error.to_string()) {
                        Ok(message) => {
                            ui.notice(&message);
                            self.push(json!({"role":"user","content":message}))?;
                        }
                        Err(message) => return self.stop_turn(ui, message, step + 1),
                    }
                    continue;
                }
                Err(error)
                    if error.downcast_ref::<llm::Interrupted>().is_some()
                        || self.interrupt.load(Ordering::SeqCst) =>
                {
                    self.save_request_policy(
                        None,
                        json!({"state":"interrupted","elapsed_ms":started.elapsed().as_millis()}),
                        ui,
                    )?;
                    return Ok(TurnReport {
                        text: String::new(),
                        outcome: TurnOutcome::Interrupted,
                        steps: step,
                        interrupted: true,
                        trajectory: None,
                    });
                }
                Err(error)
                    if !self.hermetic && looks_like_overflow(&error) && !overflow_retried =>
                {
                    self.request_phase
                        .recover("context overflow; one bounded compaction retry");
                    overflow_retried = true;
                    ui.notice("context window exceeded; compacting and retrying");
                    self.compact_using(backend, ui)?;
                    budget.give_back();
                    continue;
                }
                Err(error) => {
                    self.save_request_policy(None,json!({"state":"generation failed","elapsed_ms":started.elapsed().as_millis()}),ui)?;
                    self.record(step, "generation_error", json!({"error":format!("{error:#}"),"elapsed_ms":started.elapsed().as_millis()}))?;
                    return Err(error);
                }
            };
            self.context_meter.complete(
                request_estimate,
                request_context,
                reply.prompt_tokens,
                sent_messages,
            );
            reasoning_budget = None;
            self.step_checks.visual.delivered();
            self.save_request_policy(None,json!({"state":"response received","prompt_tokens":reply.prompt_tokens,"completion_tokens":reply.completion_tokens,"elapsed_ms":started.elapsed().as_millis(),"tool_calls":reply.tool_calls.len(),"finish_reason":reply.finish_reason}),ui)?;
            ui.usage(
                reply.prompt_tokens,
                reply.completion_tokens,
                started.elapsed().as_secs_f64(),
            );
            self.metrics.responses += 1;
            if reply.prompt_tokens.is_some() && reply.completion_tokens.is_some() {
                self.metrics.usage_reported_responses += 1;
            }
            self.metrics.prompt_tokens += reply.prompt_tokens.unwrap_or(0);
            self.metrics.output_tokens += reply.completion_tokens.unwrap_or(0);
            self.metrics.cache_write_tokens += reply.cache_creation_tokens.unwrap_or(0);
            self.metrics.cached_tokens += reply.cached_prompt_tokens.unwrap_or(0);
            self.metrics.reasoning_tokens += reply.reasoning_tokens.unwrap_or(0);
            ui.cached_usage(reply.cached_prompt_tokens, reply.cache_creation_tokens);
            if let Some(cost) = backend.reply_cost(&reply) {
                self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost);
            }
            if let Some(tokens) = reply.prompt_tokens {
                self.last_prompt_tokens = tokens;
                self.reported_messages = sent_messages;
            }
            self.probe_context();
            self.record(step, "generation", json!({"output":reply.text,"tool_calls":reply.tool_calls.len(),
                "prompt_tokens":reply.prompt_tokens,"completion_tokens":reply.completion_tokens,"elapsed_ms":started.elapsed().as_millis()}))?;
            if self.take_steering(ui)? {
                steering_pending = true;
                ui.notice("Discarded the old response before executing its tools; replanning with your new instruction.");
                budget.give_back();
                continue;
            }
            if reply.finish_reason == "length" {
                if self.config.reasoning_guard.enabled
                    && reply.text.trim().is_empty()
                    && reply.tool_calls.is_empty()
                {
                    reasoning_budget = Some(requested_tokens);
                    self.request_phase
                        .recover("output budget consumed without an action or answer");
                    self.metrics.reasoning_interceptions += 1;
                    let reason = "the output token limit was spent without any answer or tool call";
                    self.record(
                        step,
                        "reasoning_gate",
                        json!({"reason":reason,"elapsed_ms":started.elapsed().as_millis()}),
                    )?;
                    match self.reasoning_recovery.intercept(reason) {
                        Ok(message) => {
                            ui.notice(&message);
                            self.push(json!({"role":"user","content":message,"dial_reminder":"reasoning"}))?;
                        }
                        Err(message) => return self.stop_turn(ui, message, step + 1),
                    }
                    continue;
                }
                match guard::recover_from_length(
                    requested_tokens,
                    self.context_window(),
                    self.last_prompt_tokens,
                    &reply,
                    length_nudges,
                ) {
                    guard::LengthAction::Retry { max_tokens: raised } => {
                        self.request_phase.recover(
                            "output was truncated; retry retains an expanded token budget",
                        );
                        recovering_output = true;
                        ui.notice(&format!("the model hit its output limit ({requested_tokens} tokens); retrying with {raised}"));
                        max_tokens = raised;
                        budget.refund();
                        continue;
                    }
                    guard::LengthAction::Nudge { partial, message } => {
                        self.request_phase.recover(
                            "continue truncated output without executing a partial tool call",
                        );
                        recovering_output = true;
                        length_nudges += 1;
                        if let Some(text) = partial {
                            self.push(json!({"role":"assistant","content":text}))?;
                        }
                        self.push(json!({"role":"user","content":message}))?;
                        budget.refund();
                        continue;
                    }
                    guard::LengthAction::GiveUp(text) => return self.stop_turn(ui, text, step + 1),
                }
            }
            if recovering_output {
                self.metrics.truncated_recoveries += 1;
                recovering_output = false;
            }
            // Form failures bypass tool-result guards. Bound their recovery independently of
            // the optional step limit, including when providers alternate failure formats.
            let unusable = reply.dropped_tool_call()
                || (reply.tool_calls.is_empty() && looks_like_text_call(&reply.text, &names));
            if unusable {
                unusable_replies += 1;
                if unusable_replies >= 8 {
                    return self.stop_turn(
                        ui,
                        "Stopped: the model returned 8 consecutive unusable tool calls; no tools ran during these attempts. Choose a tool-capable model or adjust its tool-call configuration, then retry. Completed work and the chat are kept.".into(),
                        step + 1,
                    );
                }
            } else {
                unusable_replies = 0;
            }
            if reply.dropped_tool_call() {
                self.request_phase
                    .recover("the previous tool call was malformed");
                self.record(step, "tool_form_failure", json!({"kind":"dropped_call"}))?;
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.push("dropped_call".into());
                self.push(json!({"role":"user","content":"Your tool call was unreadable JSON. Issue it again with valid arguments, or answer directly."}))?;
                budget.refund();
                continue;
            }
            if reply.tool_calls.is_empty() && looks_like_text_call(&reply.text, &names) {
                self.request_phase
                    .recover("tool syntax appeared as text instead of a valid call");
                // The model wrote a tool call as plain text instead of using the tool-call channel.
                // That is a form failure (and a learnable one): do not run it, do not hide it.
                self.record(step, "tool_form_failure", json!({"kind":"text_call"}))?;
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.push("text_call".into());
                failed.reject(json!({"role":"assistant","content":reply.text.clone()}));
                self.push(json!({"role":"assistant","content":reply.text}))?;
                self.push(json!({"role":"user","content":"That looked like a tool call written as plain text, so nothing ran. Call the tool through the tool-calling interface, or answer in words."}))?;
                budget.refund();
                continue;
            }
            if reply.tool_calls.is_empty() {
                if !reply.text.trim().is_empty() {
                    self.step_checks.answer_delivered = true;
                }
                self.push(json!({"role":"assistant","content":reply.text}))?;
                if self.take_steering(ui)? {
                    steering_pending = true;
                    continue;
                }
                if self.interrupt.load(Ordering::SeqCst) {
                    return Ok(TurnReport {
                        text: String::new(),
                        outcome: TurnOutcome::Interrupted,
                        steps: step + 1,
                        interrupted: true,
                        trajectory: None,
                    });
                }
                if self.plan_mode && !completion::open_work(&self.todo).is_empty() {
                    return Ok(TurnReport {
                        text: reply.text,
                        outcome: TurnOutcome::NeedsInput,
                        steps: step + 1,
                        interrupted: false,
                        trajectory: None,
                    });
                }
                match completion.stopped(&self.todo) {
                    completion::StopAction::Accept => {}
                    completion::StopAction::Continue(message) => {
                        ui.notice("Continuing: todo items are still unfinished.");
                        self.push(json!({"role":"user","content":message,"dial_completion_reminder":true}))?;
                        continue;
                    }
                    completion::StopAction::Blocked(blockers) => {
                        ui.notice(&blockers);
                        return Ok(TurnReport {
                            text: reply.text,
                            outcome: TurnOutcome::Blocked,
                            steps: step + 1,
                            interrupted: false,
                            trajectory: None,
                        });
                    }
                    completion::StopAction::Incomplete(text) => {
                        return self.stop_turn(ui, text, step + 1);
                    }
                }
                if !self.hermetic
                    && let Some(prompt) = self.final_review_prompt(ui)?
                {
                    ui.notice("Review the changed files before reporting completion.");
                    self.push(llm::assistant_message(&reply))?;
                    self.push(json!({
                        "role":"user",
                        "content":prompt,
                        "dial_completion_reminder":true
                    }))?;
                    continue;
                }
                if self.logging {
                    self.training_capture.record(
                        &context_before,
                        &request_tools,
                        llm::assistant_message(&reply),
                    );
                }
                self.metrics.turns_completed += 1;
                let streamed_len = reply.text.len();
                let text = self.step_checks.visual.summary(
                    reply.text,
                    (
                        self.step_checks.mutations,
                        self.step_checks.uncheckpointed_shell_commands,
                    ),
                );
                if text.len() > streamed_len {
                    ui.text_delta(&text[streamed_len..]);
                }
                return Ok(TurnReport {
                    text,
                    outcome: TurnOutcome::Completed,
                    steps: step + 1,
                    interrupted: false,
                    trajectory: None,
                });
            }
            let narration_warning = narration.observe(&reply);
            self.push(llm::assistant_message(&reply))?;
            let (mut form_kinds, mut rejected_calls, mut execution_failed) =
                (Vec::new(), Vec::new(), false);
            let mut observed_check_failure = false;
            let mut parallel = std::collections::VecDeque::new();
            let mut stop: Option<String> = None;
            let mut round_succeeded = false;
            let mut last_tool_failure = None;
            let mut finished = None;
            let mut round_steering = Vec::new();
            for (index, call) in reply.tool_calls.iter().enumerate() {
                if self.interrupt.load(Ordering::SeqCst) {
                    self.session.repair_incomplete_tools()?;
                    return Ok(TurnReport {
                        text: String::new(),
                        outcome: TurnOutcome::Interrupted,
                        steps: step + 1,
                        interrupted: true,
                        trajectory: None,
                    });
                }
                round_steering.extend(
                    ui.take_steering()
                        .into_iter()
                        .filter(|text| !text.trim().is_empty()),
                );
                if !round_steering.is_empty() && parallel.is_empty() {
                    // Close the old call/result pair before appending the new user
                    // message. Already-prefetched reads keep their actual results.
                    self.push(llm::tool_message(&call.id, "This call was not executed: a newer user instruction superseded this pending action. Replan from the latest user message."))?;
                    continue;
                }
                let observed_sources = self.experience_sources(call, ui);
                // Read-only batch results may already have finished before a prior
                // result arms recovery. Consume those results honestly; never label
                // a prefetched operation as unexecuted or shift its result to another call.
                let form = (if parallel.is_empty() && let Some(reason) = recovery_checkpoint.rejection(&call.name) {
                    Some(("recovery_checkpoint", reason))
                } else if single_call && reply.tool_calls.len() > 1 {
                    Some(("batch_contract", "This request permits one tool call. The entire batch is rejected without executing it; choose one action and await its result.".into()))
                } else { call_form_error(call, schemas, &names) }).map(|(kind, why)| {
                    if planning_required && call.name != "todo" && kind == "unknown_tool" {
                        (kind, format!("{} is temporarily withheld by the investigation checkpoint. This request offers only todo. Create a plan with todo before requesting read, grep, bash or other tools; they return in the next round after a valid plan.", call.name))
                    } else if call.name == "todo" && kind == "schema" {
                        let op = if self.todo.tasks().is_empty() { "set" } else { "append" };
                        (kind, format!("{why}. Example valid todo arguments: {{\"op\":\"{op}\",\"tasks\":[{{\"content\":\"Resolve the remaining investigation question\",\"investigation\":true}}]}}. Task numbers are assigned automatically; index belongs at the top level for start/done/drop/block, never inside tasks."))
                    } else {
                        (kind, why)
                    }
                }).or_else(|| {
                    (call.name == "finish" && index + 1 != reply.tool_calls.len()).then(|| {
                        (
                            "invalid_arguments",
                            "finish must be the last call in a batch".to_owned(),
                        )
                    })
                });
                let contract_failed = form.is_some();
                let todo_before = self.todo.snapshot();
                let mut image = None;
                let mut preflight = None;
                if form.is_none()
                    && !self.hermetic
                    && matches!(call.name.as_str(), "write" | "edit" | "ast_edit")
                    && let (Some(cache), Some(path)) =
                        (self.codebase.clone(), call.arguments["path"].as_str())
                {
                    if prechecked_paths.insert(path.to_owned()) {
                        let path = path.to_owned();
                        let check = thread::scope(|scope| -> Result<_> {
                            let (tx, rx) = mpsc::sync_channel(1);
                            scope.spawn(move || {
                                let _ = tx.send(cache.pre_edit_context(&path));
                            });
                            receive_with_heartbeat(&rx, ui)?
                        });
                        match check {
                            Ok(Some(text)) => {
                                preflight = Some(ToolOutput {
                                    data: None,
                                    text,
                                    is_error: true,
                                });
                            }
                            Ok(None) => {
                                revalidated_paths
                                    .insert(call.arguments["path"].as_str().unwrap().to_owned());
                            }
                            Err(error) => {
                                preflight = Some(ToolOutput {
                                    data: None,
                                    text: format!(
                                        "Could not check saved assumptions: {error}. Read the current target before retrying."
                                    ),
                                    is_error: true,
                                });
                            }
                        }
                    } else if !revalidated_paths.contains(path) {
                        preflight = Some(ToolOutput {
                            data: None,
                            text: format!(
                                "Read current {path} before retrying this edit; its saved assumptions required review."
                            ),
                            is_error: true,
                        });
                    }
                }
                let mut output = if let Some(output) = preflight {
                    ui.tool_call(&call.name, &summarize(call));
                    ui.tool_result(&call.name, &output);
                    output
                } else if let Some((kind, why)) = form {
                    self.metrics.tool_contract_errors += 1;
                    self.record(
                        step,
                        "tool_form_failure",
                        json!({"kind":kind,"name":call.name}),
                    )?;
                    form_kinds.push(kind.to_owned());
                    if !call.malformed_arguments {
                        rejected_calls.push(rejected_tool_call_message(call));
                    }
                    let output = ToolOutput {
                        data: None,
                        text: format!("invalid tool call: {why}"),
                        is_error: true,
                    };
                    ui.tool_call(&call.name, &summarize(call));
                    ui.tool_timing(&call.name, 0.0);
                    ui.tool_result(&call.name, &output);
                    output
                } else {
                    ui.tool_call(&call.name, &summarize(call));
                    self.record(
                        step,
                        "tool_call",
                        json!({"name":call.name,"input":call.arguments}),
                    )?;
                    if self.enabled_tools.contains(&call.name) {
                        self.metrics.dynamic_tools_used += 1;
                    }
                    if !single_call
                        && parallel.is_empty()
                        && self.tool_hooks().is_empty()
                        && approval::parallel_read(&self.ext, &call.name, &call.arguments)
                        && !approval::denied_by_rule(&self.always, call, self.workspace.root())
                    {
                        let count = reply.tool_calls[index..]
                            .iter()
                            .take_while(|c| {
                                approval::parallel_read(&self.ext, &c.name, &c.arguments)
                                    && !approval::denied_by_rule(
                                        &self.always,
                                        c,
                                        self.workspace.root(),
                                    )
                                    && call_form_error(c, schemas, &names).is_none()
                            })
                            .count();
                        parallel = parallel_tools(
                            &self.workspace,
                            self.config,
                            &self.ext,
                            &reply.tool_calls[index..index + count],
                            &self.interrupt,
                            ui,
                        )
                        .into();
                    }
                    let (mut out, seconds) = match parallel.pop_front() {
                        Some(result) => result,
                        None if call.name == "finish" && call.arguments["outcome"] == "completed" && (execution_failed || !form_kinds.is_empty()) => {
                            (ToolOutput { data: None,
text: "finish completed cannot follow a failed call in the same batch; address its error first".into(), is_error: true }, 0.0)
                        }
                        None => { let started = Instant::now(); let out = self.dispatch(backend, call, ui); (out, started.elapsed().as_secs_f64()) }
                    };
                    if call.name == "browser" && !out.is_error {
                        match tools::artifact::capture_image(self.workspace.artifacts(), &mut out) {
                            Ok(attachment) => image = attachment,
                            Err(e) => {
                                out = ToolOutput {
                                    data: None,
                                    text: format!("retaining browser screenshot: {e:#}"),
                                    is_error: true,
                                }
                            }
                        }
                    }
                    ui.tool_timing(&call.name, seconds);
                    ui.tool_result(&call.name, &out);
                    out
                };
                reliability.observe(
                    contract_failed,
                    output.is_error,
                    !matches!(
                        call.name.as_str(),
                        "todo" | "finish" | "working_state" | "tool_search" | "tool_load"
                    ),
                    turn_started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                );
                self.observe_experience(call, &output, observed_sources, ui);
                if output.is_error {
                    last_tool_failure = Some(format!(
                        "Most recent tool error (`{}`):\n{}",
                        call.name,
                        guard::clip_middle(&output.text, 1200)
                    ));
                }
                let is_check = call.name == "bash"
                    && call.arguments["command"]
                        .as_str()
                        .is_some_and(is_check_command);
                if is_check {
                    if output.is_error || verification::exit_code(&output.text) != Some(0) {
                        reliability.failing_checks += 1;
                    } else {
                        reliability.passing_checks += 1;
                    }
                }
                let check_failed = is_check
                    && (verification::exit_code(&output.text).is_some_and(|code| code != 0)
                        || verification::zero_tests(&output.text));
                observed_check_failure |= check_failed;
                self.request_phase
                    .observe(&call.name, !output.is_error && !check_failed, is_check);
                if output.is_error && form_kinds.is_empty() {
                    execution_failed = true;
                }
                if !output.is_error
                    && call.name == "read"
                    && let Some(path) = call.arguments["path"].as_str()
                    && prechecked_paths.contains(path)
                {
                    revalidated_paths.insert(path.to_owned());
                }
                if !output.is_error {
                    round_succeeded = true;
                }
                completion.observe(call, !output.is_error, todo_before != self.todo.snapshot());
                if self.todo.has_open() {
                    unplanned_inspections = 0;
                } else if matches!(
                    call.name.as_str(),
                    "read" | "grep" | "glob" | "find" | "ast_search"
                ) {
                    if !output.is_error {
                        unplanned_inspections += 1;
                    }
                } else if !planning_required
                    && !matches!(
                        call.name.as_str(),
                        "todo" | "finish" | "working_state" | "tool_search" | "tool_load"
                    )
                {
                    unplanned_inspections = 0;
                }
                if !output.is_error
                    && matches!(call.name.as_str(), "write" | "edit" | "ast_edit" | "solve")
                {
                    self.step_checks.mutations += 1;
                    if !self.hermetic
                        && let Some(path) = call.arguments["path"].as_str()
                    {
                        self.step_checks.visual.changed(Path::new(path));
                    }
                }
                if call.name == "bash" {
                    if call.arguments["command"]
                        .as_str()
                        .is_some_and(|command| !approval::safe_bash(command))
                    {
                        self.step_checks.uncheckpointed_shell_commands += 1;
                    }
                } else if let Some(tool) = self.ext.get(&call.name)
                    && tool.may_modify_workspace(&call.arguments)
                {
                    self.step_checks.uncheckpointed_extension_calls += 1;
                    self.step_checks
                        .uncheckpointed_extension_tools
                        .insert(call.name.clone());
                }
                if !self.hermetic
                    && !output.is_error
                    && matches!(call.name.as_str(), "write" | "edit" | "ast_edit")
                    && let (Some(cache), Some(path)) =
                        (self.codebase.clone(), call.arguments["path"].as_str())
                    && reviewed_paths.len() < 4
                    && reviewed_paths.insert(path.to_owned())
                {
                    let paths = vec![path.to_owned()];
                    let context = thread::scope(|scope| -> Result<_> {
                        let (tx, rx) = mpsc::sync_channel(1);
                        scope.spawn(move || {
                            let _ = tx.send(cache.impact_context(&paths, 3000));
                        });
                        receive_with_heartbeat(&rx, ui)?
                    });
                    if let Ok(context) = context {
                        output.text.push_str("\n\n");
                        output.text.push_str(&context);
                        self.record(step, "change_review", json!({"path":path}))?;
                    }
                }
                if !output.is_error
                    && !matches!(
                        call.name.as_str(),
                        "todo" | "finish" | "working_state" | "tool_search" | "tool_load"
                    )
                {
                    self.step_checks.evidence = true;
                }
                if call.name == "finish" && !output.is_error && self.step_checks.finish_accepted {
                    finished = Some(
                        completion::Finish::parse(&call.arguments).map_err(anyhow::Error::msg)?,
                    );
                }
                recovery_checkpoint.progress(
                    call,
                    !output.is_error && !check_failed,
                    todo_before != self.todo.snapshot(),
                );
                let duplicate = investigation.observe(call, &output.text, output.is_error);
                let is_duplicate = duplicate.is_some();
                if duplicate.is_some() {
                    // An unchanged observation consumes the remaining unplanned allowance;
                    // concrete new evidence continues under the ordinary task budget.
                    if !self.todo.has_open() {
                        unplanned_inspections = self.task_policy.inspection_limit;
                    }
                    self.record(step, "investigation_gate", json!({"name":call.name}))?;
                    recovery_checkpoint.arm(&call.name, duplicate.as_deref().unwrap());
                    ui.notice(&format!("Investigation repeated an unchanged observation. Recovery checkpoint: `{}` is temporarily withheld until a different action succeeds.", call.name));
                }
                if stop.is_none() {
                    stop = investigation.stop_message();
                }
                if output.is_error {
                    let advice = crate::learn::task_policy::recovery_hint(&call.name, &output.text);
                    output.text.push_str("\n\n");
                    output.text.push_str(advice);
                }
                if let Some(advice) = repeats.observe(call, output.is_error) {
                    recovery_checkpoint.arm(&call.name, &advice);
                    ui.notice(&format!("Recovery checkpoint: `{}` repeated a failing call; choose a different action to restore it.", call.name));
                    output.text.push_str("\n\n");
                    output.text.push_str(&advice);
                }
                match stall.call(call, output.is_error) {
                    guard::Stall::Warn(advice) => {
                        if self.config.reliability.enabled {
                            recovery_checkpoint.arm(&call.name, &advice);
                            ui.notice(&format!("Recovery checkpoint: `{}` keeps failing; choose a different action to restore it.", call.name));
                        } else {
                            ui.notice(&advice);
                        }
                        output.text.push_str("\n\n");
                        output.text.push_str(&advice);
                    }
                    guard::Stall::Stop(message) => {
                        stop.get_or_insert(message);
                    }
                    guard::Stall::None => {}
                }
                if stop.is_none() {
                    stop = repeats.stop_message(call);
                }
                self.record(
                    step,
                    if output.is_error {
                        "tool_error"
                    } else {
                        "tool_result"
                    },
                    json!({"name":call.name,"output":truncate(&output.text, 4096)}),
                )?;
                let artifact = match self.workspace.artifacts() {
                    Some(store) => Some(
                        store
                            .retain(&output.text)
                            .context("retaining complete tool evidence")?,
                    ),
                    None => None,
                };
                let limit = tools::preview_limit(
                    self.context_window(),
                    &call.name,
                    output.is_error,
                    artifact.is_some(),
                );
                let mut body =
                    duplicate.unwrap_or_else(|| guard::clip_evidence(&output.text, limit));
                if call.name == "bash"
                    && !output.is_error
                    && call.arguments["command"]
                        .as_str()
                        .is_some_and(is_check_command)
                    && let Some(uri) = &artifact
                    && let Some(compact) = verification::compact_success(&output.text, uri)
                {
                    body = compact;
                }
                if (output.text.len() > limit || body.starts_with("[Investigation check"))
                    && let Some(uri) = &artifact
                {
                    body.push_str(&format!(
                        "\n[Complete tool evidence: {uri}; retrieve with artifact_read]"
                    ));
                }
                let mut message = llm::tool_message(&call.id, &body);
                message["dial_evidence"] = json!({"artifact":artifact,"is_error":output.is_error});
                let clipped = output.text.len() > limit
                    || output.text.contains("output omitted")
                    || output.text.contains("bytes clipped");
                if clipped {
                    let mut state = self.session.control_state();
                    let mut uris = state["truncated_artifacts"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    if let Some(uri) = &artifact
                        && !uris.contains(&json!(uri))
                    {
                        uris.push(json!(uri));
                    }
                    for uri in tools::artifact::uris(&output.text) {
                        if !uris.contains(&json!(uri)) {
                            uris.push(json!(uri));
                        }
                    }
                    state["truncated_artifacts"] = json!(uris);
                    self.session.set_control_state(state)?;
                }
                if call.name == "read" && !output.is_error && !is_duplicate {
                    let mut observation = call.arguments.clone();
                    if let Some(args) = observation.as_object_mut() {
                        args.remove("i");
                    }
                    message["dial_evidence"]["observation"] = json!(observation.to_string());
                }
                if let Some(image) = image {
                    message["dial_images"] = json!([image]);
                }
                if call.name == "read"
                    && !output.is_error
                    && !self.hermetic
                    && let Some(path) = call.arguments["path"].as_str()
                {
                    let path = Path::new(path);
                    if llm::images::is_image(path) {
                        let authorized =
                            llm::images::authorized_user_reference(&self.session.messages(), path);
                        let attachment = if let Some(image) = authorized {
                            Ok(image)
                        } else {
                            self.workspace
                                .resolve(path.to_string_lossy().as_ref())
                                .map_err(anyhow::Error::msg)
                                .and_then(|absolute| {
                                    let relative = absolute
                                        .strip_prefix(self.workspace.root())
                                        .context("image resolves outside the project")?;
                                    llm::images::attachment(self.workspace.root(), relative)
                                })
                        };
                        match attachment {
                            Ok(image) => message["dial_images"] = json!([image]),
                            Err(error) => {
                                ui.notice(&format!("Image attachment unavailable: {error:#}"))
                            }
                        }
                    }
                }
                if !self.hermetic
                    && message.get("dial_images").is_some()
                    && backend.accepts_image_input()
                {
                    self.step_checks.visual.captured(
                        (
                            self.step_checks.mutations,
                            self.step_checks.uncheckpointed_shell_commands,
                        ),
                        call.name == "browser",
                    );
                }
                for reference in message["dial_images"].as_array().into_iter().flatten() {
                    ui.image(reference);
                }
                self.push(message)?;
                let (tokens, estimated) = self.context_usage();
                ui.context_usage(tokens, estimated);
            }
            if self.accept_steering(round_steering, ui)? || self.take_steering(ui)? {
                steering_pending = true;
                ui.notice("Kept completed tool results and skipped unstarted old actions; replanning with your new instruction.");
                continue;
            }
            if let Some(advice) = narration_warning {
                ui.notice(&advice);
                self.push(json!({"role":"user","content":advice}))?;
            }
            let warning = match stall.round(!round_succeeded) {
                guard::Stall::Stop(message) => {
                    stop.get_or_insert(message);
                    None
                }
                guard::Stall::Warn(advice) => Some(advice),
                guard::Stall::None => None,
            };
            if let Some(advice) = warning {
                self.push(json!({"role":"user","content":advice}))?;
            }
            if let Some(plan) = self.pending_plan.take() {
                self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
            }
            reliability.round(!form_kinds.is_empty(), round_succeeded);
            if !self.hermetic {
                let mut control = self.session.control_state();
                let previous = control["reliability"].clone();
                control["reliability"] = json!({
                    "automatic_passing_checks":previous["automatic_passing_checks"],
                    "automatic_failing_checks":previous["automatic_failing_checks"],
                    "checked_commands":previous["checked_commands"],
                    "stats":reliability,
                    "measured":measured,
                    "interface":if single_call {"compact single call"}else{"standard"},
                    "verification":if self.step_checks.failed {"failed"}else if self.step_checks.clean() {"focused checks passed; review task behavior"}else if self.step_checks.mutations > 0 {"edits need verification"}else {"no edits this turn"},
                    "edit_loop":self.step_checks.status()
                });
                self.session.set_control_state(control.clone())?;
                ui.request_policy(&control);
                if self.config.reliability.enabled
                    && reliability.consecutive_contract_failures
                        >= self.config.reliability.max_contract_failures
                {
                    stop = Some(format!(
                        "Stopped after {} consecutive tool-contract failures without useful recovery. No rejected calls were executed. Inspect /reliability; run dial reliability check or select another model before retrying.",
                        reliability.consecutive_contract_failures
                    ));
                }
                let mut result = control["adaptive_policy"]["result"].clone();
                result["tool_round_failed"] = json!(
                    !form_kinds.is_empty()
                        || execution_failed
                        || observed_check_failure
                        || self.request_phase.phase == crate::adaptive::Phase::Recovery
                );
                result["next_observed_phase"] = json!(self.request_phase.phase);
                self.save_request_policy(None, result, ui)?;
            }
            if let Some(mut message) = stop {
                if self.hermetic {
                    bail!("{message}");
                }
                if let Some(failure) = last_tool_failure {
                    message.push_str("\n\n");
                    message.push_str(&failure);
                }
                message.push_str("\n\nThe chat and completed work are kept. Send a corrected instruction or say \"continue\" to resume.");
                return self.stop_turn(ui, message, step + 1);
            }
            if !form_kinds.is_empty() || execution_failed || observed_check_failure {
                self.request_phase
                    .recover("tool round failed; correct its observed diagnostics");
            }
            names = self.offered();
            *schemas = self.schemas(&names)?;
            if let Some(mut finish) = finished {
                if self.take_steering(ui)? {
                    steering_pending = true;
                    continue;
                }
                if self.interrupt.load(Ordering::SeqCst) {
                    return Ok(TurnReport {
                        text: String::new(),
                        outcome: TurnOutcome::Interrupted,
                        steps: step + 1,
                        interrupted: true,
                        trajectory: None,
                    });
                }
                if finish.outcome == TurnOutcome::Completed {
                    finish.text = self.step_checks.visual.summary(
                        finish.text,
                        (
                            self.step_checks.mutations,
                            self.step_checks.uncheckpointed_shell_commands,
                        ),
                    );
                    if self.logging {
                        self.training_capture.record(
                            &context_before,
                            &request_tools,
                            llm::assistant_message(&reply),
                        );
                    }
                    self.metrics.turns_completed += 1;
                }
                self.push(json!({"role":"assistant","content":finish.text}))?;
                ui.text_delta(&finish.text);
                return Ok(TurnReport {
                    text: finish.text,
                    outcome: finish.outcome,
                    steps: step + 1,
                    interrupted: false,
                    trajectory: None,
                });
            }
            if self.interrupt.load(Ordering::SeqCst) {
                return Ok(TurnReport {
                    text: String::new(),
                    outcome: TurnOutcome::Interrupted,
                    steps: step + 1,
                    interrupted: true,
                    trajectory: None,
                });
            }
            if self.take_steering(ui)? {
                steering_pending = true;
            }
            if form_kinds.is_empty() && !execution_failed && self.logging {
                self.training_capture.record(
                    &context_before,
                    &request_tools,
                    llm::assistant_message(&reply),
                );
            }
            if !form_kinds.is_empty() {
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.extend(form_kinds);
                for rejected in rejected_calls {
                    failed.reject(rejected);
                }
            } else if !execution_failed
                && let (Some(failed), Some(store), Some((id, _, _))) =
                    (pending.take(), self.store, &self.trajectory)
            {
                // Corrected turn: every call validated and ran without error.
                match store.tool_recovery(*id, self.version.id, &json!({
                        "context": failed.context, "tools": request_tools, "corrected": llm::assistant_message(&reply),
                        "failure_kinds": failed.kinds, "rejected": failed.rejected,
                        "project_root": self.workspace.root().to_string_lossy(),
                    })) {
                    Ok(true) => {
                        if self.record(step, "tool_recovery", json!({"failure_kinds":failed.kinds})).is_err() {
                            ui.notice("Tool recovery was captured, but its optional learning event could not be recorded; continuing.");
                        }
                    }
                    Ok(false) => {} // Duplicate or oversized learning sample, not a failed tool call.
                    Err(_) => ui.notice("Optional tool-recovery capture unavailable; continuing the task."),
                }
            }
        }
        if self.hermetic {
            bail!("agent exhausted the admitted step budget without a final answer")
        }
        self.stop_turn(
            ui,
            format!("Stopped: this turn used its {max_steps}-step limit. The chat and completed work are kept. Say \"continue\" to keep going, or raise harness.max_steps in /settings."),
            iteration,
        )
    }

    /// Context occupancy for display, including evidence added since the last request.
    pub fn context_usage(&self) -> (u64, bool) {
        self.context_meter.current(&self.session)
    }

    /// The prompt size the next request will have: the provider's own count for the last request plus a local
    /// estimate of what was added since (the reply and its tool results, easily several thousand tokens). Judging
    /// by the last report alone lets a big tool result carry the next request past the window.
    fn projected_prompt_tokens(&self) -> u64 {
        self.last_prompt_tokens
            + self
                .session
                .weigh_from(self.reported_messages, evidence::estimate) as u64
    }

    /// The provider reported `tokens` for the conversation as it stands now. (The loop anchors on the messages a
    /// request actually carried; this is for callers that learn a count from elsewhere.)
    pub fn record_prompt_usage(&mut self, tokens: u64) {
        self.last_prompt_tokens = tokens;
        self.reported_messages = self.session.message_count();
        self.context_meter
            .record_prompt(tokens, self.reported_messages);
    }

    /// End the turn without an answer: keep the reason in the conversation, show it, and report `Incomplete`.
    fn stop_turn(&mut self, ui: &mut dyn Ui, text: String, steps: u32) -> Result<TurnReport> {
        self.push(json!({"role":"assistant","content":text}))?;
        ui.notice(&text);
        Ok(TurnReport {
            text,
            outcome: TurnOutcome::Incomplete,
            steps,
            interrupted: false,
            trajectory: None,
        })
    }
    /// Verified repair patterns from this project that match the request, as advisory text; never in evaluation.
    fn repair_context(&self, query: &str, ui: &mut dyn Ui) -> Option<String> {
        let store = self.store.filter(|_| !self.hermetic)?;
        let root = self.workspace.root().to_string_lossy().into_owned();
        let hints = match verified_repair_hints(store, &root, query, &self.offered()) {
            Ok(hints) => hints,
            Err(_) => {
                ui.notice("verified repair lookup unavailable; continuing without it");
                return None;
            }
        };
        if hints.is_empty() {
            return None;
        }
        let mut text = String::from(
            "Relevant verified repair patterns from this project follow. Treat them only as advisory structure: never reuse old argument values; recheck current files and retain all approval requirements.\n",
        );
        for hint in hints {
            text.push_str("- ");
            text.push_str(&hint);
            text.push('\n');
        }
        Some(text)
    }
    fn take_steering(&mut self, ui: &mut dyn Ui) -> Result<bool> {
        let messages = ui.take_steering();
        self.accept_steering(messages, ui)
    }

    fn accept_steering(&mut self, messages: Vec<String>, ui: &mut dyn Ui) -> Result<bool> {
        let mut received = false;
        for text in messages {
            if text.trim().is_empty() {
                continue;
            }
            let message = if let Some(layout) = &self.layout {
                let (message, warnings) = context::expand_message(&layout.project_root, &text);
                for warning in warnings {
                    ui.notice(&warning);
                }
                message
            } else {
                json!({"role":"user","content":text})
            };
            for reference in message["dial_images"].as_array().into_iter().flatten() {
                ui.image(reference);
            }
            self.push(message)?;
            self.task_policy = crate::learn::task_policy::Policy::for_prompt(
                &text,
                self.config.reasoning_guard.max_pre_action_secs,
            );
            self.request_phase = crate::adaptive::State::begin(self.task_policy.kind, &text);
            let mut control = self.session.control_state();
            control["current_instruction"] = json!(prompt::truncate_chars(&text, 4096));
            control["steering_active"] = json!(true);
            self.session.set_control_state(control)?;
            received = true;
        }
        if received {
            self.task_packet.clear();
            self.reasoning_recovery = Default::default();
            self.pending_plan = None;
            ui.notice("Steering received; continuing with the new instruction.");
        }
        Ok(received)
    }

    fn authorize_call(
        &mut self,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> std::result::Result<(), approval::Denial> {
        let result = if approval::denied_by_rule(&self.always, call, self.workspace.root()) {
            Err(approval::Denial::Rule)
        } else if self.plan_mode && !approval::is_read_only(&self.ext, &call.name, &call.arguments)
        {
            Err(approval::Denial::ReadOnly)
        } else if approval::needs_approval(&self.ext, call) {
            approval::authorize(
                self.approval,
                &mut self.always,
                self.workspace.root(),
                call,
                &summarize(call),
                ui,
            )
        } else {
            Ok(())
        };
        if let Err(denial) = &result {
            ui.notice(denial.message());
        }
        result
    }

    fn run_focused_check(
        &mut self,
        check: &verification::FocusedCheck,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let args = json!({"command":check.command,"timeout":check.timeout_secs});
        // Long generated commands (an interpreter path plus inline code) say nothing the check name does not.
        let title = if check.command.len() <= 80 {
            format!("Verify {}: {}", check.name, check.command)
        } else {
            format!("Verify {}", check.name)
        };
        ui.tool_call("bash", &title);
        let started = Instant::now();
        let output = self
            .run_background_tool("bash", &args, ui)
            .unwrap_or_else(|| tools::execute(&self.workspace, "bash", &args, check.timeout_secs));
        ui.tool_timing("bash", started.elapsed().as_secs_f64());
        ui.tool_result("bash", &output);
        self.note_verification(
            &ToolCall {
                id: format!("focused-check:{}", check.name),
                name: "bash".into(),
                arguments: args,
                malformed_arguments: false,
            },
            &output,
            ui,
        );
        output
    }

    /// Focused checks for the files this turn changed: built-in syntax/type checks plus the configured
    /// commands whose extensions match.
    fn focused_checks(&self, turn: u64) -> Vec<verification::FocusedCheck> {
        let paths = self
            .checkpoints
            .as_ref()
            .map(|checkpoints| checkpoints.changed_files(turn))
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        // Endpoint users may never have run `dial setup`; a missing interpreter must not read as broken code.
        let python = if self.config.python.is_file() {
            self.config.python.clone()
        } else {
            PathBuf::from("python3")
        };
        let mut checks = verification::plan(self.workspace.root(), &python, &paths);
        if self.config.assistance.enabled && self.config.assistance.selective_verification {
            checks.extend(verification::related_tests(
                self.workspace.root(),
                &python,
                &paths,
            ));
        }
        for custom in &self.config.verification.commands {
            if !paths.is_empty()
                && (custom.file_extensions.is_empty()
                    || paths.iter().any(|path| {
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .is_some_and(|ext| {
                                custom.file_extensions.iter().any(|suffix| suffix == ext)
                            })
                    }))
            {
                checks.push(verification::FocusedCheck {
                    name: custom.name.clone(),
                    command: custom.command.clone(),
                    timeout_secs: custom.timeout_secs,
                });
            }
        }
        checks
    }

    fn publish_no_automatic_check(&mut self, ui: &mut dyn Ui) {
        if self.hermetic {
            return;
        }
        let mut control = self.session.control_state();
        if control["reliability"].is_object() {
            control["reliability"]["verification"] =
                json!("no automatic focused check available; behavior unverified");
            control["reliability"]["checked_commands"] = json!([]);
            control["reliability"]["edit_loop"] = self.step_checks.status();
            if let Err(error) = self.session.set_control_state(control.clone()) {
                ui.notice(&format!("could not save verification status: {error:#}"));
            }
            ui.request_policy(&control);
        }
    }
    fn publish_edit_loop_status(&mut self, ui: &mut dyn Ui) {
        if self.hermetic {
            return;
        }
        let mut control = self.session.control_state();
        control["reliability"]["edit_loop"] = self.step_checks.status();
        if let Err(error) = self.session.set_control_state(control.clone()) {
            ui.notice(&format!("could not save edit-loop status: {error:#}"));
        }
        ui.request_policy(&control);
    }
    fn final_review_prompt(&mut self, ui: &mut dyn Ui) -> Result<Option<String>> {
        let mutations_pending = self.step_checks.mutations > self.step_checks.diff_surfaced;
        let shell_pending = self.step_checks.uncheckpointed_shell_commands
            > self.step_checks.disclosed_shell_commands;
        let extension_pending = self.step_checks.uncheckpointed_extension_calls
            > self.step_checks.disclosed_extension_calls;
        let mut review = String::new();
        let mut paths = Vec::new();
        if let (Some(turn), Some(checkpoints)) = (self.step_checks.turn, self.checkpoints.as_ref())
        {
            paths = checkpoints.changed_files(turn);
            for path in &paths {
                self.step_checks.visual.changed(Path::new(path));
            }
            if mutations_pending && !paths.is_empty() {
                review = checkpoints.review_diff(turn)?;
            }
        }
        let visual = self.step_checks.visual.reminder((
            self.step_checks.mutations,
            self.step_checks.uncheckpointed_shell_commands,
        ));
        if !mutations_pending && !shell_pending && !extension_pending && visual.is_none() {
            return Ok(None);
        }
        if let Some(visual) = visual {
            ui.notice("Visual inspection checkpoint: inspect fresh pixels or report the concrete limitation.");
            review.push_str("\n\n");
            review.push_str(visual);
        }
        if mutations_pending && paths.is_empty() {
            review.push_str(
                "No checkpointed file diff is available; inspect the workspace diff/status manually.",
            );
        }
        if shell_pending || extension_pending {
            if shell_pending {
                review.push_str(&format!(
                    "\nUncheckpointed shell command attempts: {}.",
                    self.step_checks.uncheckpointed_shell_commands
                ));
            }
            if extension_pending {
                let tools = self
                    .step_checks
                    .uncheckpointed_extension_tools
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                review.push_str(&format!(
                    "\nUncheckpointed approval-requiring extension calls: {} (tools: {tools}).",
                    self.step_checks.uncheckpointed_extension_calls
                ));
            }
            review.push_str(
                "\nThese operations may have effects outside checkpoint tracking. Inspect workspace changes with:\n  git -c core.fsmonitor=false -c core.pager=cat --no-pager status --short --untracked-files=all\n  git -c core.fsmonitor=false -c core.pager=cat --no-pager diff --no-color --no-ext-diff --no-textconv\n  git -c core.fsmonitor=false -c core.pager=cat --no-pager diff --no-color --no-ext-diff --no-textconv --staged\nGit output may include pre-existing work; distinguish it from this turn and leave unrelated changes untouched. Inspect untracked public files individually. If this is not a Git worktree, report that review limit.",
            );
        }
        if mutations_pending && !self.config.verification.enabled {
            self.publish_no_automatic_check(ui);
        }
        self.step_checks.diff_surfaced = self.step_checks.mutations;
        self.step_checks.disclosed_shell_commands = self.step_checks.uncheckpointed_shell_commands;
        self.step_checks.disclosed_extension_calls =
            self.step_checks.uncheckpointed_extension_calls;
        self.publish_edit_loop_status(ui);
        Ok(Some(format!(
            "Final edit review required before completion. Inspect the changes below, then call finish again. Report verification limits.\n\n{review}"
        )))
    }

    /// Run `checks` until interrupted; one entry per failed check with its (clipped) output.
    fn run_checks(
        &mut self,
        checks: &[verification::FocusedCheck],
        ui: &mut dyn Ui,
    ) -> Vec<String> {
        if !checks.is_empty() {
            self.step_checks.failed_checks.clear();
        }
        let mut failures = Vec::new();
        for check in checks {
            if self.interrupt.load(Ordering::SeqCst) {
                failures.push(format!("{} not run: verification interrupted", check.name));
                break;
            }
            let output = self.run_focused_check(check, ui);
            if output.is_error
                || verification::exit_code(&output.text) != Some(0)
                || verification::zero_tests(&output.text)
            {
                self.step_checks.failed_checks.push(check.clone());
                failures.push(format!(
                    "{} failed:\n{}",
                    check.name,
                    guard::clip_evidence(&output.text, 4096)
                ));
            }
        }
        let failed_check_count = self.step_checks.failed_checks.len() as u64;
        if !failures.is_empty()
            && let Some(paths) = self
                .step_checks
                .turn
                .as_ref()
                .zip(self.checkpoints.as_ref())
                .map(|(turn, checkpoints)| checkpoints.changed_files(*turn))
                .filter(|paths| !paths.is_empty())
        {
            failures.insert(
                0,
                format!("Files changed in this turn: {}", paths.join(", ")),
            );
        }

        if !self.hermetic {
            let mut control = self.session.control_state();
            if control["reliability"].is_object() {
                let state = &mut control["reliability"];
                state["verification"] = json!(if checks.is_empty() {
                    "no automatic focused check available; behavior unverified"
                } else if failures.is_empty() {
                    "focused checks passed; assess coverage against requirements"
                } else {
                    "focused checks failed"
                });
                state["checked_commands"] =
                    json!(checks.iter().map(|c| &c.name).collect::<Vec<_>>());
                state["automatic_passing_checks"] = json!(
                    state["automatic_passing_checks"].as_u64().unwrap_or(0)
                        + (checks.len() as u64).saturating_sub(failed_check_count)
                );
                state["automatic_failing_checks"] = json!(
                    state["automatic_failing_checks"].as_u64().unwrap_or(0) + failed_check_count
                );
                state["edit_loop"] = self.step_checks.status();
                ui.request_policy(&control);
                if let Err(error) = self.session.set_control_state(control) {
                    ui.notice(&format!("could not save verification status: {error:#}"));
                }
            }
        }
        failures
    }

    /// Gate `todo done`. A step with no tool call since it started is a claim without evidence and stays
    /// open. A step that edited files runs the focused checks, so a broken step is caught while its
    /// context is fresh instead of after the whole plan. Failed checks keep the step open;
    /// repeating done without further edits cannot turn a failure into evidence of success.
    fn check_step(&mut self, ui: &mut dyn Ui, args: &Value) -> Result<Option<String>, String> {
        let text_only = self.step_checks.answer_delivered
            && self.step_checks.mutations == self.step_checks.checked
            && !self.step_checks.uncheckpointed_pending()
            && self.todo.tasks().iter().enumerate().any(|(index, task)| {
                task.status == todo::Status::InProgress
                    && !task.investigation
                    && args["index"]
                        .as_u64()
                        .is_none_or(|target| target == (index + 1) as u64)
            });
        if !self.step_checks.evidence && !text_only {
            return Err("Step not closed: no tool has run since it started and no text-only answer was delivered. Deliver the report for a non-investigation step, do the required work, or block it with a concrete reason. Do not repeat done unchanged.".into());
        }
        let pending = self.step_checks.mutations != self.step_checks.checked;
        let (Some(turn), true, true) = (
            self.step_checks.turn,
            pending,
            self.config.verification.enabled,
        ) else {
            return if self.step_checks.failed {
                Err("Step not closed: the last focused checks failed. Repair the failure before completing this step, or mark the step blocked with a concrete reason.".into())
            } else {
                Ok(None)
            };
        };
        let mut checks = self.focused_checks(turn);
        self.step_checks.include_failed_checks(&mut checks);
        if checks.is_empty() {
            self.publish_no_automatic_check(ui);
            return Ok(Some(
                "No automatic focused check is available for this step; behavior remains unverified."
                    .into(),
            ));
        }
        self.step_checks.checked = self.step_checks.mutations;
        let failures = self.run_checks(&checks, ui);
        self.step_checks.failed = !failures.is_empty();
        self.publish_edit_loop_status(ui);
        if failures.is_empty() {
            return Ok(Some(format!(
                "Focused checks passed: {}.",
                checks
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Err(format!(
            "Step not closed: focused checks failed after its edits. Fix the diagnostics, then call todo done again, or mark the step blocked with a concrete reason.\n{}",
            failures.join("\n")
        ))
    }

    /// Operator hooks apply to live agents only; evaluation runs must stay hermetic.
    fn tool_hooks(&self) -> &[crate::config::types::HookConfig] {
        if self.hermetic {
            &[]
        } else {
            &self.config.hooks
        }
    }

    fn dispatch(&mut self, backend: &mut Backend, call: &ToolCall, ui: &mut dyn Ui) -> ToolOutput {
        if call.name == "finish" {
            self.step_checks.finish_accepted = false;
        }
        if call.name != "exit_plan" {
            let root = self.workspace.root().to_path_buf();
            if let Err(reason) = hooks::pre_tool(
                self.tool_hooks(),
                &root,
                &call.name,
                &call.arguments,
                &mut |n| ui.notice(n),
            ) {
                let _ = self.record(
                    self.cur_step,
                    "tool_denied",
                    json!({"name":call.name,"hook":true}),
                );
                return ToolOutput {
                    data: None,
                    text: reason,
                    is_error: true,
                };
            }
        }
        let mut out = self.dispatch_inner(backend, call, ui);
        if call.name != "exit_plan" {
            let root = self.workspace.root().to_path_buf();
            hooks::post_tool(
                self.tool_hooks(),
                &root,
                &call.name,
                &call.arguments,
                &mut out,
                &mut |n| ui.notice(n),
            );
        }
        out
    }

    fn dispatch_inner(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let denied = |why: &str| ToolOutput {
            data: None,
            text: why.to_owned(),
            is_error: true,
        };
        if call.name == "exit_plan" && self.plan_mode {
            let Some(plan) = call.arguments["plan"]
                .as_str()
                .filter(|p| !p.trim().is_empty())
            else {
                return denied("exit_plan requires a nonempty plan");
            };
            match ui.approve("exit_plan", plan) {
                Decision::Allow | Decision::AllowAlways => {
                    self.set_plan_mode(false);
                    self.pending_plan = Some(plan.to_owned());
                    ui.notice(&format!("Plan approved:\n{plan}"));
                    return ToolOutput { data: None,
text: "Plan approved; implementation is now permitted subject to normal tool approvals.".into(), is_error: false };
                }
                Decision::Deny => {
                    return denied(&format!(
                        "Plan rejected; remain in plan mode. {}",
                        ui.approval_reason()
                            .unwrap_or_else(|| "Ask the user what to revise.".into())
                    ));
                }
            }
        }
        if let Err(denial) = self.authorize_call(call, ui) {
            let _ = self.record(self.cur_step, "tool_denied", json!({"name":call.name}));
            return denied(denial.message());
        }
        if self.interrupt.load(Ordering::SeqCst) {
            return interrupted_tool();
        }
        let args = &call.arguments;
        let name = call.name.as_str();
        if name == "read"
            && !self.hermetic
            && let Some(path) = args["path"].as_str()
            && let Some(reference) =
                llm::images::authorized_user_reference(&self.session.messages(), Path::new(path))
        {
            if args["offset"].as_u64().is_some() {
                return denied("images do not support line offsets");
            }
            return match llm::images::verified_bytes(&reference) {
                Ok((bytes, _)) => ToolOutput {
                    data: None,
                    text: format!(
                        "Image: {path} ({} original bytes; explicitly supplied by the user; attached to the next model request when vision is available)",
                        bytes.len()
                    ),
                    is_error: false,
                },
                Err(error) => denied(&format!("Cannot read supplied image: {error:#}")),
            };
        }
        if name == "experience" {
            return self.experience_tool(backend, call, ui);
        }
        if name == "edit_preview" {
            return self.preview_tool(args, ui);
        }
        if name == "find" {
            match tools::navigation::semantic_request(&self.workspace, args) {
                Ok(Some(request)) => return self.run_semantic_find(backend, args, request, ui),
                Ok(None) => {}
                Err(text) => {
                    return ToolOutput {
                        data: None,
                        text,
                        is_error: true,
                    };
                }
            }
        }
        if let Some(out) = self.run_background_tool(name, args, ui) {
            if name == "bash" {
                self.note_verification(call, &out, ui);
            }
            return out;
        }
        if tools::BUILTIN.contains(&name) {
            let out = tools::execute(&self.workspace, name, args, 120);
            if name == "bash" {
                self.note_verification(call, &out, ui);
            }
            return out;
        }
        if let Some(tool) = self.ext.get(name) {
            let event = |event: &str, is_error| LifecycleEvent {
                event: event.into(),
                session_id: Some(self.session.id().to_owned()),
                turn_id: None,
                tool_name: Some(call.name.clone()),
                is_error,
            };
            self.ext.fire_lifecycle(&event("tool_started", None));
            let output = tool.call(
                &ToolContext {
                    workspace: &self.workspace,
                    config: self.config,
                },
                args,
            );
            self.ext
                .fire_lifecycle(&event("tool_finished", Some(output.is_error)));
            return output;
        }
        match name {
            "todo" => {
                let mut next = self.todo.clone();
                let mut output = next.call(args);
                if output.is_error || next == self.todo {
                    return output;
                }
                if args["op"] == "done" {
                    match self.check_step(ui, args) {
                        Ok(Some(note)) => {
                            output.text.push('\n');
                            output.text.push_str(&note);
                        }
                        Ok(None) => {}
                        Err(failure) => return denied(&failure),
                    }
                }
                let mut state = self.session.control_state();
                state["todo"] = next.snapshot();
                if let Err(error) = self.session.set_control_state(state) {
                    return denied(&format!("could not persist todos: {error:#}"));
                }
                // A new step needs its own evidence; work done before the first plan counts toward its first step.
                let active = |list: &TodoList| {
                    list.tasks()
                        .iter()
                        .position(|t| t.status == todo::Status::InProgress)
                };
                if active(&self.todo).is_some() && active(&next) != active(&self.todo) {
                    self.step_checks.evidence = false;
                    self.step_checks.answer_delivered = false;
                }
                self.todo = next;
                ui.todo(&self.todo);
                output
            }
            "finish" => {
                let finish = match completion::Finish::parse(args) {
                    Ok(finish) => finish,
                    Err(error) => return denied(&error),
                };
                if finish.outcome == TurnOutcome::Completed {
                    let work = completion::open_work(&self.todo);
                    if !work.is_empty() {
                        return denied(&format!(
                            "cannot finish completed with open todos:\n{work}\nFinish them, drop them with a reason, or finish blocked if a blocker remains."
                        ));
                    }
                    self.pump_subagents(ui);
                    if self
                        .live_tasks
                        .status(None)
                        .ok()
                        .and_then(|jobs| jobs.as_array().cloned())
                        .is_some_and(|jobs| jobs.iter().any(|job| job["status"] == "running"))
                    {
                        return denied(
                            "cannot finish completed while live tasks are running; wait for and inspect their results",
                        );
                    }
                }
                if finish.outcome == TurnOutcome::Completed
                    && !self.hermetic
                    && self.step_checks.failed
                    && (!self.config.verification.enabled
                        || self.step_checks.mutations <= self.step_checks.checked)
                {
                    return denied(
                        "Cannot finish completed: focused checks failed and no new edits are awaiting a fresh check. Repair, rerun the failed check, or report a blocker.",
                    );
                }
                if finish.outcome == TurnOutcome::Completed
                    && !self.hermetic
                    && self.config.verification.enabled
                    && self.step_checks.mutations > self.step_checks.checked
                    && let Some(turn) = self.step_checks.turn
                {
                    let mut checks = self.focused_checks(turn);
                    self.step_checks.include_failed_checks(&mut checks);
                    if !checks.is_empty() {
                        self.step_checks.checked = self.step_checks.mutations;
                        let failures = self.run_checks(&checks, ui);
                        self.step_checks.failed = !failures.is_empty();
                        self.publish_edit_loop_status(ui);
                        if !failures.is_empty() {
                            return denied(&format!(
                                "Cannot finish completed: focused verification failed. Fix the observed diagnostics or report a blocker.\n{}",
                                failures.join("\n")
                            ));
                        }
                    } else {
                        self.publish_no_automatic_check(ui);
                    }
                }
                if finish.outcome == TurnOutcome::Completed && !self.hermetic {
                    match self.final_review_prompt(ui) {
                        Ok(Some(prompt)) => {
                            ui.notice("Completion review pending; continuing the agent.");
                            return ToolOutput {
                                data: None,
                                text: prompt,
                                is_error: false,
                            };
                        }
                        Ok(None) => {}
                        Err(error) => {
                            return denied(&format!(
                                "Cannot complete final diff review: {error:#}. Inspect the changed files before finishing."
                            ));
                        }
                    }
                }

                self.step_checks.finish_accepted = true;
                ToolOutput {
                    data: None,
                    text: "Turn outcome accepted.".into(),
                    is_error: false,
                }
            }
            "tool_search" => {
                let query = args["query"].as_str().unwrap_or("").to_lowercase();
                let matches: Vec<Value> = self.catalog().into_iter().filter_map(|name| {
                    let schema = self.schemas(std::slice::from_ref(&name)).ok()?.pop()?;
                    let description = schema["function"]["description"].as_str().unwrap_or("");
                    let text = format!("{name} {description}").to_lowercase();
                    query.split_whitespace().all(|word| text.contains(word)).then(|| json!({"name":name,"description":description,"enabled":self.offered().contains(&name)}))
                }).take(32).collect();
                ToolOutput {
                    data: None,
                    text: json!(matches).to_string(),
                    is_error: false,
                }
            }
            "tool_load" => {
                let requested: Vec<String> = args["names"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let unload: Vec<String> = args["unload"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let catalog = self.catalog();
                if (requested.is_empty() && unload.is_empty())
                    || requested
                        .iter()
                        .chain(&unload)
                        .any(|n| !catalog.contains(n))
                {
                    return denied("tool_load contains an unavailable tool; use tool_search first");
                }
                let mut enabled = self.enabled_tools.clone();
                enabled.extend(requested.iter().cloned());
                for name in &unload {
                    enabled.remove(name);
                }
                let mut state = self.session.control_state();
                let mut sorted: Vec<_> = enabled.iter().cloned().collect();
                sorted.sort();
                state["enabled_tools"] = json!(sorted);
                if let Err(e) = self.session.set_control_state(state) {
                    return denied(&format!("persisting enabled tools: {e:#}"));
                }
                self.enabled_tools = enabled;
                self.system = self.build_system();
                ToolOutput {
                    data: None,
                    text: format!(
                        "Next round: enabled [{}]; unloaded [{}]",
                        requested.join(", "),
                        unload.join(", ")
                    ),
                    is_error: false,
                }
            }
            "artifact_read" => match self.workspace.artifacts() {
                Some(store) => {
                    let output = store.call(args);
                    if !output.is_error {
                        self.metrics.artifacts_retrieved += 1;
                        if self.session.control_state()["truncated_artifacts"]
                            .as_array()
                            .is_some_and(|uris| uris.contains(&args["uri"]))
                        {
                            self.metrics.truncated_recoveries += 1;
                        }
                    }
                    output
                }
                None => denied("artifacts are unavailable in hermetic runs"),
            },
            "working_state" => {
                let mut state = self.session.control_state();
                if args["action"] == "update" {
                    let mut working = args.clone();
                    working.as_object_mut().unwrap().remove("action");
                    state["working_state"] = working;
                    if let Err(e) = self.session.set_control_state(state.clone()) {
                        return denied(&format!("{e:#}"));
                    }
                }
                ToolOutput {
                    data: None,
                    text: state["working_state"].to_string(),
                    is_error: false,
                }
            }
            "skill" | "skill_manage" | "session_search" => match &self.layout {
                None => denied("unavailable here"),
                Some(layout) => match name {
                    "skill" => skills::skill_tool::call(layout, args),
                    "skill_manage" => skills::manage_tool::call(layout, args),
                    _ => session::session_search_tool::call(layout, args),
                },
            },
            "task" => self.run_task(backend, args, ui),
            "solve" => self.run_solve(backend, args, ui),
            "escalate" => {
                backend.release();
                let env = subagents::Env {
                    config: self.config,
                    workspace: &self.workspace,
                    ext: &self.ext,
                    layout: self.layout.as_ref(),
                    harness: &self.version.harness,
                    interrupt: &self.interrupt,
                    usage: Some(&self.usage_scope),
                };
                solve::escalate(env, args, &mut self.session_cost, ui)
            }
            other => match self.config.tools.get(other) {
                Some(tool) => {
                    let input = args.get("input").and_then(Value::as_str).unwrap_or("");
                    match worker::run_command(
                        &tool.program,
                        &tool.args,
                        &json!({"input":input}).to_string(),
                        tool.timeout_secs,
                    ) {
                        Ok(text) => ToolOutput {
                            data: None,
                            text,
                            is_error: false,
                        },
                        Err(error) => ToolOutput {
                            data: None,
                            text: format!("{error:#}"),
                            is_error: true,
                        },
                    }
                }
                None => denied(&format!("unknown tool: {other}")),
            },
        }
    }
    /// Blocking I/O tools run off the controller thread so the inline UI can repaint and process input.
    fn run_background_tool(
        &mut self,
        name: &str,
        args: &Value,
        ui: &mut dyn Ui,
    ) -> Option<ToolOutput> {
        if self.hermetic {
            return None;
        }
        let builtin = tools::BUILTIN.contains(&name);
        let extension = self.ext.get(name).cloned();
        let cancellation = extension.clone();
        let external = self.config.tools.get(name).cloned();
        if !builtin && extension.is_none() && external.is_none() {
            return None;
        }
        let workspace = self.workspace.clone();
        let config = self.config.clone();
        let arguments = args.clone();
        let name = name.to_owned();
        let cancel = self.interrupt.clone();
        let active = Arc::new(AtomicBool::new(false));
        let started = active.clone();
        let serial = !approval::parallel_read(&self.ext, &name, &arguments);
        enum Event {
            Output(tools::ext::ToolEvent),
            Done(ToolOutput),
        }
        let (tx, rx) = mpsc::sync_channel(64);
        let worker = match thread::Builder::new()
            .name("dial-tool".to_owned())
            .spawn(move || {
                let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _serial = if serial {
                        Some(workspace.serial_operation())
                    } else {
                        None
                    };
                    if cancel.load(Ordering::SeqCst) {
                        return interrupted_tool();
                    }
                    started.store(true, Ordering::SeqCst);
                    let emit = |event| {
                        let _ = tx.send(Event::Output(event));
                    };
                    if name == "bash" {
                        let stream = |bytes: &[u8]| {
                            emit(tools::ext::ToolEvent {
                                kind: "output".into(),
                                name: name.clone(),
                                text: String::from_utf8_lossy(bytes).into_owned(),
                                artifact: None,
                            })
                        };
                        return tools::shell::bash_tool_streaming(
                            &workspace, &arguments, 120, &stream, &cancel,
                        )
                        .unwrap_or_else(|text| ToolOutput {
                            data: None,
                            text,
                            is_error: true,
                        });
                    }
                    if builtin {
                        return tools::execute(&workspace, &name, &arguments, 120);
                    }
                    if let Some(tool) = extension {
                        return tool.call_streaming(
                            &ToolContext {
                                workspace: &workspace,
                                config: &config,
                            },
                            &arguments,
                            &emit,
                            &cancel,
                        );
                    }
                    let tool = external.expect("external tool was checked");
                    match worker::run_command(
                        &tool.program,
                        &tool.args,
                        &json!({"input":arguments["input"].as_str().unwrap_or("")}).to_string(),
                        tool.timeout_secs,
                    ) {
                        Ok(text) => ToolOutput {
                            data: None,
                            text,
                            is_error: false,
                        },
                        Err(e) => ToolOutput {
                            data: None,
                            text: format!("{e:#}"),
                            is_error: true,
                        },
                    }
                }))
                .unwrap_or_else(|_| ToolOutput {
                    data: None,
                    text: "tool worker panicked".into(),
                    is_error: true,
                });
                started.store(false, Ordering::SeqCst);
                let _ = tx.send(Event::Done(output));
            }) {
            Ok(worker) => worker,
            Err(e) => {
                return Some(ToolOutput {
                    data: None,
                    text: format!("starting tool: {e}"),
                    is_error: true,
                });
            }
        };
        let output = loop {
            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(Event::Output(event)) => {
                    if event.kind == "output" {
                        ui.tool_output(&event.name, &event.text);
                    } else {
                        ui.background_notice(&format!(
                            "{}: {}{}",
                            event.name,
                            event.text,
                            event
                                .artifact
                                .map(|uri| format!("\n[Complete output: {uri}]"))
                                .unwrap_or_default()
                        ));
                    }
                }
                Ok(Event::Done(output)) => break output,
                Err(RecvTimeoutError::Timeout) => {
                    ui.heartbeat();
                    if self.interrupt.load(Ordering::SeqCst)
                        && active.load(Ordering::SeqCst)
                        && let Some(tool) = &cancellation
                    {
                        tool.cancel_operation();
                    }
                    self.pump_subagents(ui);
                }
                Err(_) => {
                    break ToolOutput {
                        data: None,
                        text: "tool worker ended without output".into(),
                        is_error: true,
                    };
                }
            }
        };
        let _ = worker.join();
        Some(output)
    }

    pub fn pump_subagents(&mut self, ui: &mut dyn Ui) {
        self.live_tasks.bind(self.session.runtime());
        ui.runtime(&self.session.runtime());
        if self.interrupt.load(Ordering::SeqCst) {
            self.live_tasks.cancel_all();
        }
        self.live_tasks.pump(
            self.approval,
            self.plan_mode,
            &mut self.always,
            self.workspace.root(),
            &mut self.session_cost,
            ui,
        );
        ui.heartbeat();
        if let Err(error) = self.session.flush_settled_runtime() {
            ui.diagnostic(&format!(
                "could not journal settled execution state: {error:#}"
            ));
        }
        for name in self.ext.names() {
            if let Some(tool) = self.ext.get(name) {
                for event in tool.drain_notifications() {
                    if event.kind == "output" {
                        ui.tool_output(&event.name, &event.text);
                    } else {
                        ui.background_notice(&format!(
                            "{}: {}{}",
                            event.name,
                            event.text,
                            event
                                .artifact
                                .map(|uri| format!("\n[Complete output: {uri}]"))
                                .unwrap_or_default()
                        ));
                    }
                }
            }
        }
    }
    pub fn subagent_status(&mut self, id: Option<&str>, ui: &mut dyn Ui) -> Result<Value> {
        self.pump_subagents(ui);
        self.live_tasks.status(id).map_err(anyhow::Error::msg)
    }
    pub fn subagent_message(&self, id: &str, text: &str) -> Result<Value> {
        self.live_tasks
            .message(id, text)
            .map_err(anyhow::Error::msg)
    }
    pub fn cancel_subagent(&self, id: &str) -> Result<Value> {
        self.live_tasks.cancel(id).map_err(anyhow::Error::msg)
    }
    pub fn live_task_control(&self) -> LiveTaskControl {
        self.live_tasks.control()
    }
    pub fn start_subagents(&mut self, args: &Value, ui: &mut dyn Ui) -> Result<Value> {
        self.live_tasks.bind(self.session.runtime());
        ui.runtime(&self.session.runtime());
        let call = ToolCall {
            id: String::new(),
            name: "task".into(),
            arguments: args.clone(),
            malformed_arguments: false,
        };
        self.authorize_call(&call, ui)
            .map_err(|e| anyhow::anyhow!(e.message()))?;
        let mut harness = self.version.harness.clone();
        harness.tools = self.offered();
        let result = self
            .live_tasks
            .start(
                self.config,
                &self.workspace,
                &self.ext,
                self.layout.as_ref(),
                &harness,
                args,
                Some(&self.usage_scope),
            )
            .map_err(anyhow::Error::msg);
        self.session.flush_runtime()?;
        result
    }

    /// Delegate to the subagent runner, lending it only the pieces a child may use: the workspace,
    /// extension tools and interrupt flag, plus the approval state its mutating calls go through.
    fn run_task(&mut self, main: &mut Backend, args: &Value, ui: &mut dyn Ui) -> ToolOutput {
        self.pump_subagents(ui);
        let mut harness = self.version.harness.clone();
        harness.tools = self.offered();
        match args["action"].as_str().unwrap_or("run") {
            "start" => {
                return live_tasks::output(self.live_tasks.start(
                    self.config,
                    &self.workspace,
                    &self.ext,
                    self.layout.as_ref(),
                    &harness,
                    args,
                    Some(&self.usage_scope),
                ));
            }
            "status" => return live_tasks::output(self.live_tasks.status(args["id"].as_str())),
            "wait" => {
                return live_tasks::output(self.live_tasks.wait(
                    args["id"].as_str(),
                    args["timeout"].as_u64().unwrap_or(30),
                    self.approval,
                    self.plan_mode,
                    &mut self.always,
                    self.workspace.root(),
                    &mut self.session_cost,
                    ui,
                ));
            }
            "message" => {
                return live_tasks::output(self.live_tasks.message(
                    args["id"].as_str().unwrap_or(""),
                    args["message"].as_str().unwrap_or(""),
                ));
            }
            "cancel" => {
                return live_tasks::output(
                    self.live_tasks.cancel(args["id"].as_str().unwrap_or("")),
                );
            }
            "run" => {}
            _ => {
                return ToolOutput {
                    data: None,
                    text: "unknown task action".into(),
                    is_error: true,
                };
            }
        }
        let env = subagents::Env {
            config: self.config,
            workspace: &self.workspace,
            ext: &self.ext,
            layout: self.layout.as_ref(),
            harness: &harness,
            interrupt: &self.interrupt,
            usage: Some(&self.usage_scope),
        };
        let parent = subagents::Parent {
            env,
            approval: self.approval,
            always: &mut self.always,
            main,
            side: &mut self.subagent_backend,
            cost: &mut self.session_cost,
            adapter: self.version.adapter.as_deref(),
        };
        subagents::run(parent, args, ui)
    }

    fn run_solve(&mut self, main: &mut Backend, args: &Value, ui: &mut dyn Ui) -> ToolOutput {
        let env = subagents::Env {
            config: self.config,
            workspace: &self.workspace,
            ext: &self.ext,
            layout: self.layout.as_ref(),
            harness: &self.version.harness,
            interrupt: &self.interrupt,
            usage: Some(&self.usage_scope),
        };
        solve::run(
            subagents::Parent {
                env,
                approval: self.approval,
                always: &mut self.always,
                main,
                side: &mut self.subagent_backend,
                cost: &mut self.session_cost,
                adapter: self.version.adapter.as_deref(),
            },
            args,
            ui,
        )
    }

    /// Idle compaction temporarily owns the main backend; in-loop compaction borrows the active one.
    pub fn compact(&mut self, ui: &mut dyn Ui) -> Result<()> {
        let mut backend = self.backend.take().context("backend is already in use")?;
        let result = self.compact_using(&mut backend, ui);
        self.backend = Some(backend);
        result
    }

    /// Estimated tokens of the conversation the next request would carry (summary plus retained messages).
    fn session_tokens(&self) -> usize {
        self.session.messages().iter().map(evidence::estimate).sum()
    }

    /// Compact, then check it worked. The provider reported `last_prompt_tokens` for the whole request, so what it
    /// counted beyond the conversation (system prompt, tool schemas, recalled notes) is the fixed `overhead`.
    /// If summarising leaves the next prompt above 70% of the window it compacts once more, harder; if even that
    /// leaves it above 90% it says so, because a further summary cannot help when the fixed part is the problem.
    fn compact_using(&mut self, backend: &mut Backend, ui: &mut dyn Ui) -> Result<()> {
        let window = self.context_window() as usize;
        // What the provider counted for its request beyond the conversation it carried (system prompt, tool
        // schemas, recalled notes). Messages added since that request are not part of its count.
        let added_since = self
            .session
            .weigh_from(self.reported_messages, evidence::estimate);
        let overhead = (self.last_prompt_tokens as usize)
            .saturating_sub(self.session_tokens().saturating_sub(added_since));
        self.compact_once(backend, ui, None)?;
        let predicted = |agent: &Self| overhead + agent.session_tokens();
        if predicted(self) * 10 <= window * 7 {
            return Ok(());
        }
        ui.notice(&format!("compaction freed too little (next prompt ~{} of {window} tokens); compacting older messages harder", predicted(self)));
        if let Err(error) = self.compact_once(backend, ui, Some(window / 8)) {
            ui.notice(&format!("no further compaction possible: {error:#}"));
        }
        let remaining = predicted(self);
        ensure!(
            remaining * 10 <= window * 9,
            "the next prompt would still be about {remaining} of {window} tokens even after compacting: the fixed part (system prompt, tool schemas, recalled notes) is about {overhead}. Raise the model's context window, or trim `integrations` and the tool list"
        );
        Ok(())
    }

    /// One summarisation pass. `tail_budget` cuts by token budget (keep about that much verbatim); `None` keeps
    /// whole recent turns.
    fn compact_once(
        &mut self,
        backend: &mut Backend,
        ui: &mut dyn Ui,
        tail_budget: Option<usize>,
    ) -> Result<()> {
        let messages = self.session.messages();
        let window = self.context_window() as usize;
        let cut = match tail_budget {
            Some(budget) => evidence::token_cut(&messages, budget),
            None => Self::compaction_cut(&messages, window),
        }
        .context("nothing to compact yet")?;
        let transcript = render_transcript(&messages[..cut]);
        let request = Chat {
            messages: vec![
                json!({"role":"system","content":"You compress conversations for a coding agent. Write a dense summary that preserves: the user's goal, decisions made, files read or changed (with paths), commands run and their results, open problems and next steps. No preamble."}),
                json!({"role":"user","content":transcript}),
            ],
            tools: vec![],
            temperature: 0.2,
            max_tokens: 1200,
            seed: None,
        };
        let summary = self.role_chat(backend, "compaction", &request, ui)?;
        ensure!(
            !summary.trim().is_empty(),
            "model produced an empty summary"
        );
        let goal = self.session.control_state()["original_goal"].clone();
        let evidence = messages[..cut]
            .iter()
            .filter_map(|m| m["dial_evidence"]["artifact"].as_str())
            .collect::<Vec<_>>();
        let tasks = if self.todo.is_empty() {
            String::new()
        } else {
            format!(
                "\nTask list (todo tool, authoritative):\n{}",
                self.todo.render()
            )
        };
        let summary = format!(
            "{}\n\nOriginal user goal: {}\nDurable decisions/open work: {}{tasks}\nRetained evidence (read and mutation results): {}",
            summary.trim(),
            goal,
            self.session.control_state()["working_state"],
            evidence.join(", ")
        );
        self.session.compact(summary, messages.len() - cut)?;
        self.metrics.compactions += 1;
        self.last_prompt_tokens = 0;
        self.context_meter = context_meter::ContextMeter::default();
        self.reported_messages = 0;
        let (tokens, estimated) = self.context_usage();
        ui.context_usage(tokens, estimated);
        ui.notice(&format!(
            "Context compacted: summarised {cut} messages; retained {} recent messages.",
            messages.len() - cut
        ));
        Ok(())
    }

    /// Where to cut history for compaction. Normally just before the second-most-recent user message, so whole
    /// recent turns stay verbatim. When that tail alone is over half the window (one user request followed by a
    /// long tool chain, the usual agent shape) it cuts by token budget instead, so compaction can actually free
    /// room mid-turn rather than failing with "nothing to compact".
    fn compaction_cut(messages: &[Value], window: usize) -> Option<usize> {
        let users: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "user")
            .map(|(i, _)| i)
            .collect();
        if let Some(&cut) = users.len().checked_sub(2).and_then(|i| users.get(i)) {
            let tail: usize = messages[cut..].iter().map(evidence::estimate).sum();
            if tail <= window / 2 {
                return Some(cut);
            }
        }
        evidence::token_cut(messages, window / 4)
    }

    fn run_semantic_find(
        &mut self,
        main: &mut Backend,
        args: &Value,
        request: tools::navigation::SemanticRequest,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let result = (|| -> Result<ToolOutput> {
            let supplementary = tools::navigation::lsp_args(args).map_err(anyhow::Error::msg)?;
            let response = if request.has_candidates() {
                let chat = semantic_chat(&request, self.version.harness.max_tokens);
                self.role_chat(main, "subagent", &chat, ui)
                    .context("semantic find ranking failed")?
            } else {
                "{\"indices\":[]}".to_owned()
            };
            let mut output = ToolOutput {
                data: None,
                text: request
                    .render_response(&response)
                    .map_err(anyhow::Error::msg)?,
                is_error: false,
            };
            if !request.has_candidates() {
                output.text.insert_str(
                    0,
                    "[No source candidates; semantic model was not invoked.]\n",
                );
            }
            if let Some(arguments) = supplementary {
                let metadata = self.run_background_tool("lsp", &arguments, ui).context(
                    "semantic find supplemental metadata requires the configured lsp integration",
                )?;
                append_find_supplement(&self.workspace, &mut output, metadata)?;
            }
            Ok(output)
        })();
        result.unwrap_or_else(|e| ToolOutput {
            data: None,
            text: format!("{e:#}"),
            is_error: true,
        })
    }

    fn role_chat(
        &mut self,
        main: &mut Backend,
        role: &str,
        request: &Chat,
        ui: &mut dyn Ui,
    ) -> Result<String> {
        let selection = self.config.model_roles.get(role).filter(|selection| {
            !(selection.base_url.is_none()
                && self.config.endpoint.is_none()
                && selection.model == self.config.model
                && selection.provider == "mlx"
                && self.config.provider.as_deref().is_none_or(|p| p == "mlx"))
        });
        let Some(selection) = selection else {
            let reply = if self.hermetic {
                crate::learn::compute::background_call(self.config, main, |main, cancel| {
                    main.chat_with_control(
                        self.version.adapter.as_deref(),
                        request,
                        self.config.worker_timeout_secs,
                        None,
                        &mut |_| {},
                        cancel,
                    )
                })?
            } else {
                let adapter = self.version.adapter.clone();
                let timeout = self.config.worker_timeout_secs;
                let interrupt = self.interrupt.clone();
                chat_with_ui(
                    main,
                    adapter.as_deref(),
                    request,
                    timeout,
                    interrupt,
                    false,
                    None,
                    ui,
                    None,
                    &mut |ui| self.pump_subagents(ui),
                )?
            };
            if let Some(cost) = main.reply_cost(&reply) {
                self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost);
            }
            return Ok(reply.text);
        };
        let role_config = self.config.with_selection(selection);
        let mut backend = match self.role_backends.remove(role) {
            Some(backend) => backend,
            None => make_backend(&role_config)?,
        };
        main.release();
        ui.role(role, &selection.display_name());
        let result = if self.hermetic {
            crate::learn::compute::background_call(self.config, &mut backend, |backend, cancel| {
                backend.chat_with_control(
                    None,
                    request,
                    self.config.worker_timeout_secs,
                    None,
                    &mut |_| {},
                    cancel,
                )
            })
        } else {
            let timeout = self.config.worker_timeout_secs;
            let interrupt = self.interrupt.clone();
            chat_with_ui(
                &mut backend,
                None,
                request,
                timeout,
                interrupt,
                false,
                None,
                ui,
                None,
                &mut |ui| self.pump_subagents(ui),
            )
        };
        if let Ok(reply) = &result
            && let Some(cost) = backend.reply_cost(reply)
        {
            self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost);
        }
        backend.release();
        self.role_backends.insert(role.to_owned(), backend);
        ui.role("main", &self.config.selection().display_name());
        result.map(|reply| reply.text)
    }
}

fn semantic_chat(request: &tools::navigation::SemanticRequest, max_tokens: u32) -> Chat {
    Chat {
        messages: vec![
            json!({"role":"system","content":request.system.as_str()}),
            json!({"role":"user","content":request.prompt.as_str()}),
        ],
        tools: vec![],
        temperature: 0.0,
        max_tokens,
        seed: None,
    }
}

fn append_find_supplement(
    workspace: &Workspace,
    output: &mut ToolOutput,
    metadata: ToolOutput,
) -> Result<()> {
    output
        .text
        .push_str("\n[Supplementary real language-server metadata; outside model ranking]\n");
    output
        .text
        .push_str(&guard::clip_middle(&metadata.text, 4096));
    if metadata.text.len() > 4096 {
        let store = workspace
            .artifacts()
            .context("retaining supplementary metadata requires artifact storage")?;
        output.text.push_str(&format!(
            "\n[Complete language-server metadata: {}]\n",
            store.retain(&metadata.text)?
        ));
    }
    output.is_error |= metadata.is_error;
    Ok(())
}

fn result_clone(report: &TurnReport) -> TurnReport {
    TurnReport {
        text: report.text.clone(),
        outcome: report.outcome,
        steps: report.steps,
        interrupted: report.interrupted,
        trajectory: report.trajectory,
    }
}

fn configure(
    ws: Workspace,
    config: &Config,
    mode: crate::config::types::EditModeConfig,
    layout: Option<&Layout>,
    checkpoints: Option<Arc<Checkpoints>>,
) -> Result<Workspace> {
    let mut ws = ws
        .with_edit_mode(tools::EditMode::from(mode))
        .with_syntax_guard(config.assistance.enabled && config.assistance.syntax_guard);
    if let Some(checkpoints) = checkpoints {
        ws = ws.with_observer(Arc::new(CheckpointObserver(checkpoints)));
    }
    configure_sandbox(ws, config, layout)
}

fn configure_strict(
    ws: Workspace,
    config: &Config,
    mode: crate::config::types::EditModeConfig,
) -> Result<Workspace> {
    configure_sandbox(ws.with_edit_mode(tools::EditMode::from(mode)), config, None)
}

fn configure_sandbox(ws: Workspace, config: &Config, layout: Option<&Layout>) -> Result<Workspace> {
    tools::apply_sandbox_config(
        ws,
        &config.sandbox,
        layout.map(|l| (l.state_dir.as_path(), l.user_dir.as_path())),
    )
}

/// A superseded read is replaced only if no more than this many estimated tokens follow it (or the context is
/// under pressure): replacing one buried deeper would force the provider to prefill everything after it again.
const CACHE_WARM_SUFFIX_TOKENS: usize = 6_000;
/// Old observation results are dropped under context pressure, except in the newest part of the conversation:
/// a fifth of the window, never less than this, so the model keeps the evidence it is working from right now.
const PRUNE_PROTECT_MIN_TOKENS: usize = 4_000;
/// Longest search query sent to memory recall, in characters; the store itself refuses queries over 8 KiB.
const RECALL_QUERY_CHARS: usize = 2_000;
const MAX_REPAIR_REJECTIONS: usize = 3;
const MAX_REPAIR_REJECTION_BYTES: usize = 8 * 1024;

struct Pending {
    context: Vec<Value>,
    kinds: Vec<String>,
    rejected: Vec<Value>,
}

impl Pending {
    fn new(context: Vec<Value>) -> Self {
        Self {
            context,
            kinds: Vec::new(),
            rejected: Vec::new(),
        }
    }

    fn reject(&mut self, message: Value) {
        if self.rejected.len() < MAX_REPAIR_REJECTIONS
            && serde_json::to_vec(&message)
                .is_ok_and(|bytes| bytes.len() <= MAX_REPAIR_REJECTION_BYTES)
        {
            self.rejected.push(message);
        }
    }
}

fn rejected_tool_call_message(call: &ToolCall) -> Value {
    json!({
        "role":"assistant",
        "content":null,
        "tool_calls":[{
            "id":call.id,
            "type":"function",
            "function":{"name":call.name,"arguments":call.arguments.to_string()}
        }]
    })
}
fn repair_terms(text: &str) -> HashSet<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "from", "this", "that", "with", "into", "when", "after", "before",
        "how", "can", "please", "help", "make", "tool", "tools", "call", "run", "fix", "use",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| word.chars().count() >= 3 && !STOP.contains(&word.as_str()))
        .take(128)
        .collect()
}

/// Only tiny structural hints from verified repairs are retrieved; old argument values and outputs
/// never enter the new prompt.
fn verified_repair_hints(
    store: &Store,
    project_root: &str,
    query: &str,
    offered: &[String],
) -> Result<Vec<String>> {
    let query_terms = repair_terms(query);
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }
    let offered: HashSet<&str> = offered.iter().map(String::as_str).collect();
    let threshold = if query_terms.len() <= 3 { 1 } else { 2 };
    let mut candidates: Vec<(usize, i64, String, String)> = Vec::new();
    for (trajectory, payload) in store.recent_tool_recoveries(project_root, 8)? {
        if payload["project_root"].as_str() != Some(project_root) {
            continue;
        }
        let Some(context) = payload["context"].as_array() else {
            continue;
        };
        let Some(prompt) = context
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
        else {
            continue;
        };
        let prior_terms = repair_terms(prompt);
        let overlap = query_terms.intersection(&prior_terms).count();
        if overlap < threshold {
            continue;
        }
        let kinds = payload["failure_kinds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|kind| {
                kind.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .take(32)
                    .collect::<String>()
            })
            .filter(|kind| !kind.is_empty())
            .take(3)
            .collect::<Vec<_>>();
        let calls = payload
            .pointer("/corrected/tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for call in calls {
            let Some(name) = call.pointer("/function/name").and_then(Value::as_str) else {
                continue;
            };
            if !offered.contains(name) {
                continue;
            }
            let argument = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            let mut keys = argument
                .as_ref()
                .and_then(Value::as_object)
                .map(|object| {
                    object
                        .keys()
                        .filter(|key| {
                            key.len() <= 32
                                && key.chars().all(|c| {
                                    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
                                })
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            keys.sort();
            keys.truncate(5);
            let signature = format!("{name}:{}:{}", keys.join(","), kinds.join(","));
            let reason = if kinds.is_empty() {
                "a malformed call".to_owned()
            } else {
                kinds.join(", ")
            };
            let fields = if keys.is_empty() {
                "its schema".to_owned()
            } else {
                keys.join(", ")
            };
            let hint = format!(
                "Similar verified repair used `{name}` with fields {fields} after {reason}. Re-read and validate current values."
            );
            candidates.push((overlap, trajectory, signature, truncate(&hint, 240)));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut seen = HashSet::new();
    Ok(candidates
        .into_iter()
        .filter_map(|(_, _, signature, hint)| seen.insert(signature).then_some(hint))
        .take(3)
        .collect())
}

/// Deterministic call-form check against the schema we offered. `None` means the call is well formed
/// (it may still fail at runtime, which is environment feedback rather than a model skill).
fn call_form_error(
    call: &ToolCall,
    schemas: &[Value],
    names: &[String],
) -> Option<(&'static str, String)> {
    if call.malformed_arguments || call.name.is_empty() {
        return Some((
            "malformed_arguments",
            "tool arguments were not a valid JSON object".into(),
        ));
    }
    if !names.contains(&call.name) {
        return Some((
            "unknown_tool",
            format!("unknown or unavailable tool: {}", call.name),
        ));
    }
    let parameters = schemas
        .iter()
        .find(|s| s.pointer("/function/name").and_then(Value::as_str) == Some(call.name.as_str()))
        .and_then(|s| s.pointer("/function/parameters"))?;
    calls::validate(parameters, &call.arguments)
        .err()
        .map(|why| ("schema", why))
}

fn summarize(call: &ToolCall) -> String {
    summarize_call(&call.name, &call.arguments)
}

/// One line a person can read for a tool call: tool lines, approval prompts and replayed sessions. Tools that can
/// need approval keep their full arguments unless a field below already says everything the approver must see.
pub fn summarize_call(name: &str, a: &Value) -> String {
    let s = |key: &str| a[key].as_str().unwrap_or("");
    let within = |pattern: &str| match (s("path"), s("glob")) {
        ("", "") => pattern.to_owned(),
        (path, "") => format!("{pattern} in {path}"),
        ("", glob) => format!("{pattern} ({glob})"),
        (path, glob) => format!("{pattern} in {path} ({glob})"),
    };
    let text = match name {
        "bash" => s("command").to_owned(),
        "write" | "read" => s("path").to_owned(),
        "edit" => a["input"]
            .as_str()
            .or_else(|| a["path"].as_str())
            .unwrap_or_default()
            .to_owned(),
        "task" => subagents::summarize(a),
        "grep" | "glob" => within(s("pattern")),
        "todo" => summarize_todo(a),
        "finish" if s("outcome") == "completed" => "request completion".into(),
        "finish" => s("outcome").replace('_', " "),
        "tool_search" => s("query").to_owned(),
        "tool_load" => a["names"]
            .as_array()
            .map(|n| {
                n.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "artifact_read" => s("uri").to_owned(),
        "working_state" => s("action").to_owned(),
        _ if a.is_null() => String::new(),
        _ => a.to_string(),
    };
    truncate(&text, 1500)
}

fn summarize_todo(a: &Value) -> String {
    let op = a["op"].as_str().unwrap_or("?");
    let count = a["tasks"].as_array().map_or(0, Vec::len);
    let plural = if count == 1 { "" } else { "s" };
    let index = a["index"]
        .as_u64()
        .map(|i| format!(" {i}"))
        .unwrap_or_default();
    match op {
        "set" if count == 0 => "clear".to_owned(),
        "set" => format!("plan {count} task{plural}"),
        "append" => format!("add {count} task{plural}"),
        _ => match a["reason"].as_str() {
            Some(reason) => format!("{op}{index}: {reason}"),
            None => format!("{op}{index}"),
        },
    }
}

fn render_transcript(messages: &[Value]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("?");
        match role {
            "tool" => out.push_str(&format!(
                "[tool result] {}\n",
                guard::clip_middle(m["content"].as_str().unwrap_or(""), 1500)
            )),
            _ => {
                if let Some(calls) = m["tool_calls"].as_array() {
                    for c in calls {
                        out.push_str(&format!(
                            "[assistant called {}] {}\n",
                            c["function"]["name"].as_str().unwrap_or("?"),
                            truncate(c["function"]["arguments"].as_str().unwrap_or(""), 800)
                        ));
                    }
                }
                if let Some(text) = m["content"].as_str().filter(|t| !t.is_empty()) {
                    out.push_str(&format!("[{role}] {}\n", guard::clip_middle(text, 4000)));
                }
            }
        }
    }
    guard::clip_middle(&out, 60_000)
}

/// Marker error text for a hermetic run that used its whole step budget without answering.
const BUDGET_MSG: &str = "agent exhausted the admitted step budget";

fn seed_workspace(dir: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    for (rel, content) in files {
        let p = Path::new(rel);
        ensure!(
            !rel.is_empty()
                && p.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "seeded file path must be relative"
        );
        let target = dir.join(p);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, content)?;
    }
    Ok(())
}

/// Run the agent hermetically on one task and return whether `check` passes on the final workspace
/// and answer. A stuck agent (step budget exhausted) fails the case; infrastructure errors propagate
/// so a broken model server aborts an evaluation instead of silently scoring zero.
pub fn check_case(
    runtime: &mut Runtime,
    config: &Config,
    version: &Version,
    prompt: &str,
    files: &BTreeMap<String, String>,
    check: &Check,
) -> Result<bool> {
    Ok(check_case_measured(runtime, config, version, prompt, files, check)?.passed)
}

/// The sealed verifier verdict plus observed workflow cost, without writing evaluation traces.
pub fn check_case_measured(
    runtime: &mut Runtime,
    config: &Config,
    version: &Version,
    prompt: &str,
    files: &BTreeMap<String, String>,
    check: &Check,
) -> Result<crate::learn::workflow::CaseMeasurement> {
    ensure!(
        !prompt.trim().is_empty() && prompt.len() <= 65536,
        "prompt must be 1..65536 bytes"
    );
    let workspace = tempfile::tempdir()?;
    seed_workspace(workspace.path(), files)?;
    let mut agent = Agent::hermetic(
        config,
        runtime.take_backend(config)?,
        version,
        workspace.path(),
        None,
        None,
    )?;
    let result = agent.turn(prompt, &mut NullUi);
    runtime.backend = agent.backend.take();
    let metrics = agent.workflow_metrics.into_inner();
    let passed = match result {
        Ok(report) => {
            report.outcome == TurnOutcome::Completed && check.verify(workspace.path(), &report.text)
        }
        Err(error) if format!("{error:#}").contains(BUDGET_MSG) => false,
        Err(error) => return Err(error),
    };
    Ok(crate::learn::workflow::CaseMeasurement { passed, metrics })
}

/// Self-play: like `check_case`, but the trajectory is recorded (domain `selfplay:<family>`) with the
/// verifier's verdict as its outcome, so passing runs become training data and failures become signal.
pub fn selfplay_run(
    runtime: &mut Runtime,
    config: &Config,
    store: &Store,
    version: &Version,
    family: &str,
    prompt: &str,
    files: &BTreeMap<String, String>,
    check: &Check,
) -> Result<bool> {
    let workspace = tempfile::tempdir()?;
    seed_workspace(workspace.path(), files)?;
    let mut agent = Agent::hermetic(
        config,
        runtime.take_backend(config)?,
        version,
        workspace.path(),
        Some(store),
        Some(format!("selfplay:{family}")),
    )?;
    let result = (|| -> Result<bool> {
        let mut input = prompt.to_owned();
        for attempt in 0..2 {
            let result = agent.turn(&input, &mut NullUi);
            let (trajectory, text, completed) = match result {
                Ok(report) => (
                    report.trajectory,
                    report.text,
                    report.outcome == TurnOutcome::Completed,
                ),
                Err(error) if format!("{error:#}").contains(BUDGET_MSG) => {
                    (agent.last_trajectory, String::new(), false)
                }
                Err(error) => return Err(error),
            };
            let passed = completed && check.verify(workspace.path(), &text);
            if let Some(id) = trajectory {
                store.outcome(&crate::config::types::Outcome {
                    trajectory: id,
                    score: if passed { 100.0 } else { 0.0 },
                    source: format!("verifier:{family}"),
                    reference: Some(format!("training attempt {}", attempt + 1)),
                })?;
            }
            if passed {
                return Ok(true);
            }
            if !completed || attempt == 1 {
                return Ok(false);
            }
            crate::learn::auto::check_stop(config)?;
            // This feedback is confined to training cases. Gate cases are never repaired or logged.
            input = "The independent training-task verifier rejected the result. Revisit the original requirements, inspect the smallest relevant implementation, and correct the behavior. Preserve tests and requirements. Change your approach instead of repeating the failed attempt, and run relevant checks before finishing.".into();
        }
        Ok(false)
    })();
    runtime.backend = agent.backend.take();
    result
}

/// Commands whose exit status is a meaningful external verdict on the work (tests, builds, linters).
fn is_check_command(command: &str) -> bool {
    const MARKERS: &[&str] = &[
        "pytest",
        "cargo test",
        "cargo check",
        "cargo build",
        "cargo clippy",
        "npm test",
        "npm run test",
        "npm run build",
        "npm run lint",
        "yarn test",
        "pnpm test",
        "go test",
        "go build",
        "go vet",
        "make test",
        "make check",
        "make build",
        "tsc",
        "mvn test",
        "gradle test",
        "ruff",
        "flake8",
        "mypy",
        "eslint",
        "unittest",
        "swift test",
        "dotnet test",
        "bun test",
        "deno test",
        "rspec",
        "phpunit",
        "ctest",
    ];
    let c = command.to_lowercase();
    MARKERS.iter().any(|m| c.contains(m))
}

pub fn json_text(text: &str) -> &str {
    let text = text.trim();
    if let Some(body) = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
    {
        body.trim().strip_suffix("```").unwrap_or(body).trim()
    } else {
        text
    }
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &text[..end])
}

fn looks_like_overflow(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    text.contains("context")
        && ["length", "window", "too long", "exceed", "too large"]
            .iter()
            .any(|k| text.contains(k))
}

/// A reply that is just a JSON-looking object naming an offered tool. Small models often emit
/// slightly broken ones (`{{"name": ...}` with unbalanced braces), so this matches the shape
/// rather than requiring valid JSON.
fn looks_like_text_call(text: &str, names: &[String]) -> bool {
    let t = text.trim();
    if !t.starts_with('{')
        || t.len() > 8000
        || !t.contains("\"arguments\"") && !t.contains("\"parameters\"")
    {
        return false;
    }
    names.iter().any(|n| {
        t.contains(&format!("\"name\": \"{n}\"")) || t.contains(&format!("\"name\":\"{n}\""))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_slow_reads_overlap_and_results_stay_ordered() {
        struct SlowRead {
            entered: std::sync::atomic::AtomicUsize,
        }
        impl tools::ext::Tool for SlowRead {
            fn name(&self) -> &str {
                "web_fetch"
            }
            fn schema(&self) -> Value {
                json!({})
            }
            fn needs_approval(&self, _: &Value) -> bool {
                false
            }
            fn call(&self, _: &ToolContext, args: &Value) -> ToolOutput {
                self.entered.fetch_add(1, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(2);
                while self.entered.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(1));
                }
                let overlap = self.entered.load(Ordering::SeqCst) >= 2;
                if args["index"] == 0 {
                    thread::sleep(Duration::from_millis(30));
                }
                ToolOutput {
                    data: None,
                    text: args["index"].to_string(),
                    is_error: !overlap,
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let mut ext = Registry::new();
        ext.register(Arc::new(SlowRead {
            entered: std::sync::atomic::AtomicUsize::new(0),
        }));
        let calls: Vec<_> = (0..2)
            .map(|index| ToolCall {
                id: index.to_string(),
                name: "web_fetch".into(),
                arguments: json!({"index":index}),
                malformed_arguments: false,
            })
            .collect();
        let outputs = parallel_tools(
            &workspace,
            &Config::default(),
            &ext,
            &calls,
            &AtomicBool::new(false),
            &mut NullUi,
        );
        assert!(
            outputs.iter().all(|(out, _)| !out.is_error),
            "reads must overlap instead of timing out waiting for each other"
        );
        assert_eq!(
            outputs
                .iter()
                .map(|(out, _)| out.text.as_str())
                .collect::<Vec<_>>(),
            ["0", "1"]
        );
    }

    #[test]
    fn repair_hints_are_project_scoped_relevant_and_omit_old_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            "test-model",
            "test-usecase",
            &Default::default(),
        )
        .unwrap();
        let version = store.active().unwrap().id;
        let trajectory = store
            .begin_trajectory(version, "coding", "repair", "input")
            .unwrap();
        store.tool_recovery(trajectory, version, &json!({
            "project_root":"/workspace/a",
            "context":[{"role":"user","content":"Fix the parser for emoji filenames"}],
            "tools":[],
            "corrected":{"role":"assistant","tool_calls":[{
                "id":"call-1","type":"function",
                "function":{"name":"edit","arguments":"{\"path\":\"private/secret.txt\",\"input\":\"SECRET_VALUE\"}"}
            }]},
            "failure_kinds":["schema"],
            "rejected":[{"role":"assistant","content":"rejected"}]
        })).unwrap();
        let offered = vec!["edit".to_owned()];
        let hints = verified_repair_hints(
            &store,
            "/workspace/a",
            "Please fix the parser for emoji filenames",
            &offered,
        )
        .unwrap();
        assert_eq!(hints.len(), 1);
        assert!(
            hints[0].contains("edit") && hints[0].contains("path") && hints[0].contains("schema")
        );
        assert!(
            !hints[0].contains("secret.txt") && !hints[0].contains("SECRET_VALUE"),
            "hints must not replay historical argument values"
        );
        assert!(
            verified_repair_hints(
                &store,
                "/workspace/b",
                "fix the parser for emoji filenames",
                &offered
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            verified_repair_hints(&store, "/workspace/a", "weather forecast paris", &offered)
                .unwrap()
                .is_empty()
        );
        assert!(
            verified_repair_hints(
                &store,
                "/workspace/a",
                "fix the parser for emoji filenames",
                &["read".into()]
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn slow_streams_keep_the_ui_alive_before_the_first_token() {
        struct RecordingUi {
            heartbeats: usize,
            text: String,
        }
        impl Ui for RecordingUi {
            fn heartbeat(&mut self) {
                self.heartbeats += 1;
            }
            fn text_delta(&mut self, text: &str) {
                self.text.push_str(text);
            }
        }
        let delta = json!({"choices":[{"delta":{"content":"hello"}}]}).to_string();
        let finish = json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string();
        let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::SseSlow {
            pieces: vec![
                ": waiting for first token\n\n".to_owned(),
                format!("data: {delta}\n\n"),
                format!("data: {finish}\n\ndata: [DONE]\n\n"),
            ],
            pause: Duration::from_millis(180),
        }]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: format!("{}/v1", server.url),
                model: "test-model".to_owned(),
                api_key: None,
            },
            Path::new("/nonexistent/python").to_owned(),
            5,
        );
        let request = Chat {
            messages: vec![json!({"role":"user","content":"hello"})],
            tools: vec![],
            temperature: 0.2,
            max_tokens: 16,
            seed: None,
        };
        let mut ui = RecordingUi {
            heartbeats: 0,
            text: String::new(),
        };
        let reply = chat_with_ui(
            &mut backend,
            None,
            &request,
            5,
            Arc::new(AtomicBool::new(false)),
            true,
            None,
            &mut ui,
            None,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reply.text, "hello");
        assert_eq!(ui.text, "hello");
        assert!(
            ui.heartbeats >= 1,
            "the UI should repaint during the fake model's initial delay"
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn local_startup_is_visible_and_does_not_trigger_the_reasoning_gate() {
        #[derive(Default)]
        struct StartupUi {
            phases: Vec<llm::InferencePhase>,
            heartbeats: usize,
        }
        impl Ui for StartupUi {
            fn model_progress(&mut self, progress: &llm::ModelProgress) {
                self.phases.push(progress.phase);
            }
            fn heartbeat(&mut self) {
                self.heartbeats += 1;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let python = llm::server::tests::fake_python(dir.path());
        let script = std::fs::read_to_string(&python).unwrap();
        let (shebang, rest) = script.split_once('\n').unwrap();
        std::fs::write(
            &python,
            format!("{shebang}\nimport time; time.sleep(1.3)\n{rest}"),
        )
        .unwrap();
        crate::platform::fs::set_mode(&python, 0o755).unwrap();
        let mut backend = Backend::new(
            ProviderKind::Local {
                model_dir: dir
                    .path()
                    .join("model-info.json")
                    .to_string_lossy()
                    .into_owned(),
            },
            python,
            10,
        );
        let request = Chat {
            messages: vec![json!({"role":"user","content":"hi"})],
            tools: vec![],
            temperature: 0.2,
            max_tokens: 16,
            seed: None,
        };
        let mut ui = StartupUi::default();
        let policy = crate::config::types::ReasoningGuardConfig {
            max_pre_action_secs: 1,
            ..Default::default()
        };
        let reply = chat_with_ui(
            &mut backend,
            None,
            &request,
            5,
            Arc::new(AtomicBool::new(false)),
            true,
            None,
            &mut ui,
            Some(&policy),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reply.text, "pong");
        assert!(ui.phases.contains(&llm::InferencePhase::Loading));
        assert!(ui.phases.contains(&llm::InferencePhase::Processing));
        assert!(ui.phases.contains(&llm::InferencePhase::Generating));
        assert!(
            ui.heartbeats >= 5,
            "input must remain responsive during startup"
        );
    }

    #[test]
    fn context_updates_before_stream_callbacks_and_reconciles_reported_usage() {
        #[derive(Default)]
        struct RecordingUi {
            current: (u64, bool),
            observed: Vec<(u64, bool)>,
        }
        impl Ui for RecordingUi {
            fn context_usage(&mut self, tokens: u64, estimated: bool) {
                self.current = (tokens, estimated);
            }
            fn text_delta(&mut self, _: &str) {
                self.observed.push(self.current);
            }
            fn reasoning_delta(&mut self, _: &str) {
                self.observed.push(self.current);
            }
            fn tool_arguments_delta(&mut self, _: u64, _: &str, _: &str, _: &str) {
                self.observed.push(self.current);
            }
        }
        let content = "x".repeat(80);
        let arguments = json!({"path":content}).to_string();
        let chunks = [
            json!({"choices":[{"delta":{"reasoning_content":content}}]}),
            json!({"choices":[{"delta":{"content":content}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"r","type":"function","function":{"name":"read","arguments":arguments}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":512,"completion_tokens":7,"total_tokens":519}}),
        ];
        let mut pieces: Vec<_> = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect();
        pieces.push("data: [DONE]\n\n".into());
        let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::SseSlow {
            pieces,
            pause: Duration::from_millis(90),
        }]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: format!("{}/v1", server.url),
                model: "test-model".into(),
                api_key: None,
            },
            Path::new("/nonexistent/python").to_owned(),
            5,
        );
        let request = Chat {
            messages: vec![json!({"role":"user","content":"inspect"})],
            tools: vec![],
            temperature: 0.2,
            max_tokens: 128,
            seed: None,
        };
        let mut ui = RecordingUi::default();
        chat_with_ui(
            &mut backend,
            None,
            &request,
            5,
            Arc::new(AtomicBool::new(false)),
            true,
            Some(1000),
            &mut ui,
            None,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(ui.observed.len(), 3);
        assert_eq!(ui.observed[0], (1020, true));
        assert_eq!(ui.observed[1], (1040, true));
        assert!(ui.observed[2].0 > 1040 && ui.observed[2].1);
        assert_eq!(ui.current, (519, false));
    }

    #[test]
    fn background_waits_keep_the_ui_pumping() {
        struct CountingUi(usize);
        impl Ui for CountingUi {
            fn heartbeat(&mut self) {
                self.0 += 1;
            }
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let _ = tx.send(());
        });
        let mut ui = CountingUi(0);
        receive_with_heartbeat(&rx, &mut ui).unwrap();
        assert!(
            ui.0 >= 1,
            "the UI must repaint while the operation is pending"
        );
    }

    #[test]
    fn local_progress_reaches_ui_before_text_and_resets_between_requests() {
        #[derive(Default)]
        struct RecordingUi {
            progress: Vec<llm::ModelProgress>,
            text: String,
            before_text: bool,
            heartbeats: usize,
        }
        impl Ui for RecordingUi {
            fn model_progress(&mut self, p: &llm::ModelProgress) {
                if p.output_tokens == Some(12) && self.text.is_empty() {
                    self.before_text = true;
                }
                self.progress.push(p.clone());
            }
            fn text_delta(&mut self, text: &str) {
                self.text.push_str(text);
            }
            fn heartbeat(&mut self) {
                self.heartbeats += 1;
            }
        }
        let events = [
            json!({"dial_progress":{"phase":"processing","processed_tokens":32,"prompt_tokens":64}}),
            json!({"dial_progress":{"phase":"generating","output_tokens":12,"tokens_per_second":30}}),
            json!({"choices":[{"delta":{"content":"ok"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":64,"completion_tokens":14}}),
        ];
        let pieces = events
            .iter()
            .map(|e| format!("data: {e}\n\n"))
            .chain(["data: [DONE]\n\n".into()])
            .collect::<Vec<_>>();
        let server = crate::llm::fake::serve(vec![
            crate::llm::fake::Script::SseSlow { pieces, pause: Duration::from_millis(110) },
            crate::llm::fake::Script::Sse("data: {\"choices\":[{\"delta\":{\"content\":\"next\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()),
        ]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: format!("{}/v1", server.url),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        );
        let request = Chat {
            messages: vec![json!({"role":"user","content":"hello"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 32,
            seed: None,
        };
        let mut ui = RecordingUi::default();
        let reply = chat_with_ui(
            &mut backend,
            None,
            &request,
            5,
            Arc::new(AtomicBool::new(false)),
            true,
            None,
            &mut ui,
            None,
            &mut |_| {},
        )
        .unwrap();
        assert!(ui.before_text);
        assert!(ui.heartbeats > 0);
        assert_eq!(ui.text, "ok");
        assert_eq!(reply.completion_tokens, Some(14));
        assert!(ui.progress.iter().any(|p| p.processed_tokens == Some(32)));
        assert_eq!(ui.progress.last().unwrap().output_tokens, Some(14));
        let mut ui = RecordingUi::default();
        chat_with_ui(
            &mut backend,
            None,
            &request,
            5,
            Arc::new(AtomicBool::new(false)),
            false,
            None,
            &mut ui,
            None,
            &mut |_| {},
        )
        .unwrap();
        assert!(ui.text.is_empty());
        assert_eq!(ui.progress[0].output_tokens, None);
        assert_eq!(ui.progress.last().unwrap().output_tokens, Some(1));
        assert!(ui.progress.last().unwrap().estimated);
    }

    #[test]
    fn internal_model_output_stays_out_of_the_transcript() {
        #[derive(Default)]
        struct RecordingUi {
            text: usize,
            reasoning: usize,
            arguments: usize,
        }
        impl Ui for RecordingUi {
            fn text_delta(&mut self, _: &str) {
                self.text += 1;
            }
            fn reasoning_delta(&mut self, _: &str) {
                self.reasoning += 1;
            }
            fn tool_arguments_delta(&mut self, _: u64, _: &str, _: &str, _: &str) {
                self.arguments += 1;
            }
        }
        let events = [
            json!({"choices":[{"delta":{"reasoning_content":"private maintenance reasoning"}}]}),
            json!({"choices":[{"delta":{"content":"saved result"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"unexpected","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        ];
        let body = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Sse(body)]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: format!("{}/v1", server.url),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        );
        let request = Chat {
            messages: vec![json!({"role":"user","content":"maintain memory"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 32,
            seed: None,
        };
        let mut ui = RecordingUi::default();
        let reply = chat_with_ui(
            &mut backend,
            None,
            &request,
            5,
            Arc::new(AtomicBool::new(false)),
            false,
            None,
            &mut ui,
            None,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reply.text, "saved result");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!((ui.text, ui.reasoning, ui.arguments), (0, 0, 0));
    }

    #[test]
    fn continuous_model_events_do_not_starve_ui_heartbeats() {
        struct RecordingUi {
            heartbeats: usize,
            events: usize,
        }
        impl Ui for RecordingUi {
            fn heartbeat(&mut self) {
                self.heartbeats += 1;
            }
            fn reasoning_delta(&mut self, _: &str) {
                self.events += 1;
                // Simulate rendering while the worker queues the rest of a fast response.
                thread::sleep(Duration::from_millis(3));
            }
        }
        let event = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"reasoning_content":"thinking"}}]})
        );
        let body = event.repeat(100)
            + &format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
            );
        let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Sse(body)]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: format!("{}/v1", server.url),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        );
        let request = Chat {
            messages: vec![json!({"role":"user","content":"hello"})],
            tools: vec![],
            temperature: 0.2,
            max_tokens: 16,
            seed: None,
        };
        let mut ui = RecordingUi {
            heartbeats: 0,
            events: 0,
        };
        chat_with_ui(
            &mut backend,
            None,
            &request,
            5,
            Arc::new(AtomicBool::new(false)),
            true,
            None,
            &mut ui,
            None,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(ui.events, 100);
        assert!(
            ui.heartbeats >= 2,
            "a continuously occupied event queue must still pump input"
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn only_real_check_commands_count_as_verification() {
        for yes in [
            "cargo test --lib",
            "python3 -m pytest -q",
            "npm test",
            "go test ./...",
            "python -m unittest",
        ] {
            assert!(is_check_command(yes), "{yes}");
        }
        for no in [
            "ls -la",
            "cat notes.txt",
            "echo hello",
            "git status",
            "python3 script.py",
        ] {
            assert!(!is_check_command(no), "{no}");
        }
    }
}
