//! The agent runtime: one conversation, native tool calling, sessions, approvals, checkpoints,
//! compaction and (optionally) trajectory recording for the self-improvement loop.
//!
//! The same loop serves three callers: the interactive REPL and print mode (full context: memory,
//! skills, project files, persistent session) and the sealed gate (`run`: hermetic, ephemeral,
//! auto-approved, strict sandbox, nothing read from the user's environment).
pub mod approval;
pub mod calls;
mod child;
mod completion;
pub use completion::TurnOutcome;
pub mod guard;
pub mod context;
pub mod output_schema;
pub mod prompt;
pub mod subagents;
pub mod todo;
mod verification;
mod evidence;
mod training;
mod live_tasks;
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
        provenance::ProvenanceMemory,
        paths::Layout,
        session::{self, Session},
        skills,
        store::Store,
    },
    tools::{self, ToolOutput, Workspace, ext::{LifecycleEvent, Registry, ToolContext}},
};
use todo::TodoList;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::{mpsc::{self, Receiver, RecvTimeoutError}, Arc, atomic::{AtomicBool, Ordering}},
    thread,
    time::{Duration, Instant},
};

pub enum Decision { Allow, AllowAlways, Deny }

pub trait Ui {
    fn text_delta(&mut self, _text: &str) {}
    fn tool_call(&mut self, _name: &str, _summary: &str) {}
    fn tool_result(&mut self, _name: &str, _output: &ToolOutput) {}
    fn notice(&mut self, _message: &str) {}
    fn usage(&mut self, _prompt: Option<u64>, _output: Option<u64>, _seconds: f64) {}
    fn tool_output(&mut self, _name: &str, _text: &str) {}
    fn background_notice(&mut self, _message: &str) {}
    fn reasoning_delta(&mut self, _text: &str) {}
    /// Observational progress only: incomplete arguments are never dispatched as tool calls.
    fn tool_arguments_delta(&mut self, _index: u64, _id: &str, _name: &str, _delta: &str) {}
    fn cached_usage(&mut self, _read: Option<u64>, _write: Option<u64>) {}
    fn extension_panel(&mut self, title: &str, markdown: &str) { self.notice(&format!("{title}\n{markdown}")); }
    fn role(&mut self, _role: &str, _model: &str) {}
    fn approve(&mut self, _tool: &str, _summary: &str) -> Decision { Decision::Deny }
    fn approve_with_cancel(&mut self, tool: &str, summary: &str, cancel: &AtomicBool) -> Decision {
        if cancel.load(Ordering::SeqCst) { Decision::Deny } else { self.approve(tool, summary) }
    }
    fn approval_reason(&mut self) -> Option<String> { None }
    /// Called while an inference/tool worker is pending so interactive surfaces can repaint and pump input.
    fn heartbeat(&mut self) {}
    /// Elapsed time without visible model output while its request is still pending.
    fn model_wait(&mut self, _silence: Duration) {}
    /// Messages the user typed while the turn ran. Polled between steps; the agent appends them as user
    /// messages before the next model request so the model can change course without waiting for the turn to end.
    fn take_steering(&mut self) -> Vec<String> { Vec::new() }
    /// A tool call finished running; `seconds` is its wall time. Structured surfaces (JSON output) use it.
    fn tool_timing(&mut self, _name: &str, _seconds: f64) {}
}

pub struct NullUi;
impl Ui for NullUi {}

enum ChatEvent { Text(String), Reasoning(String), ToolArguments { index: u64, id: String, name: String, delta: String }, Notice(String), Finished(Result<llm::Reply>) }

fn receive_with_heartbeat<T>(rx: &Receiver<T>, ui: &mut dyn Ui) -> Result<T> {
    loop {
        match rx.recv_timeout(Duration::from_millis(80)) {
            Ok(event) => return Ok(event),
            Err(RecvTimeoutError::Timeout) => ui.heartbeat(),
            Err(RecvTimeoutError::Disconnected) => bail!("background operation ended without a result"),
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
    ui: &mut dyn Ui,
    on_idle: &mut dyn FnMut(&mut dyn Ui),
) -> Result<llm::Reply> {
    let (tx, rx) = mpsc::channel();
    ui.model_wait(Duration::ZERO);
    thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name("dial-model".to_owned())
            .spawn_scoped(scope, move || {
                let mut on_text = |text: &str| {
                    tx.send(ChatEvent::Text(text.to_owned())).is_ok()
                        && !interrupt.load(Ordering::SeqCst)
                };
                let mut on_notice = |notice: &str| { let _ = tx.send(ChatEvent::Notice(notice.to_owned())); };
                let mut on_event = |event: &llm::StreamEvent| {
                    match event {
                        llm::StreamEvent::Reasoning(text) => { let _ = tx.send(ChatEvent::Reasoning(text.clone())); },
                        llm::StreamEvent::ToolArguments { index, id, name, delta } => { let _ = tx.send(ChatEvent::ToolArguments { index: *index, id: id.clone(), name: name.clone(), delta: delta.clone() }); },
                    }
                    !interrupt.load(Ordering::SeqCst)
                };
                let result = backend.chat_with_control_events(adapter, request, timeout, Some(&mut on_text), &mut on_notice,
                    Arc::clone(&interrupt), &mut on_event);
                let _ = tx.send(ChatEvent::Finished(result));
            })
            .map_err(|error| anyhow::anyhow!("could not start model worker: {error}"))?;
        let mut last_progress = Instant::now();
        let mut last_tick = Instant::now();
        let result = loop {
            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(ChatEvent::Text(text)) => { last_progress = Instant::now(); if render_text { ui.text_delta(&text); } }
                Ok(ChatEvent::Notice(notice)) => { last_progress = Instant::now(); ui.notice(&notice); }
                Ok(ChatEvent::Reasoning(text)) => { last_progress = Instant::now(); ui.reasoning_delta(&text); }
                Ok(ChatEvent::ToolArguments { index, id, name, delta }) => { last_progress = Instant::now(); ui.tool_arguments_delta(index, &id, &name, &delta); }
                Ok(ChatEvent::Finished(result)) => break result,
                Err(RecvTimeoutError::Timeout) => {},
                Err(RecvTimeoutError::Disconnected) => break Err(anyhow::anyhow!("model worker ended without a result")),
            }
            // Pump input even when a role's hidden text continuously fills the event queue.
            if last_tick.elapsed() >= Duration::from_millis(80) {
                ui.model_wait(last_progress.elapsed());
                ui.heartbeat(); on_idle(ui);
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
    ToolOutput { text: "interrupted by the user before this ran".into(), is_error: true }
}

/// Run only a prevalidated consecutive group. No agent/session/UI state enters a worker.
fn parallel_tools(workspace: &Workspace, config: &Config, ext: &Registry, calls: &[ToolCall], interrupt: &AtomicBool, ui: &mut dyn Ui) -> Vec<(ToolOutput, f64)> {
    thread::scope(|scope| {
        let mut receivers = Vec::with_capacity(calls.len());
        for call in calls {
            let (tx, rx) = mpsc::sync_channel(1);
            receivers.push(rx);
            scope.spawn(move || {
                let started = Instant::now();
                let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if interrupt.load(Ordering::SeqCst) { return interrupted_tool(); }
                    if tools::BUILTIN.contains(&call.name.as_str()) {
                        tools::execute(workspace, &call.name, &call.arguments, 120)
                    } else if let Some(tool) = ext.get(&call.name) {
                        tool.call(&ToolContext { workspace, config }, &call.arguments)
                    } else {
                        ToolOutput { text: "parallel tool unavailable".into(), is_error: true }
                    }
                })).unwrap_or_else(|_| ToolOutput { text: "tool worker panicked".into(), is_error: true });
                let _ = tx.send((output, started.elapsed().as_secs_f64()));
            });
        }
        receivers.into_iter().map(|rx| receive_with_heartbeat(&rx, ui).unwrap_or_else(|error| {
            (ToolOutput { text: format!("{error:#}"), is_error: true }, 0.0)
        })).collect()
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
    pub fn new() -> Self { Self { backend: None } }

    pub fn backend(&mut self, config: &Config) -> Result<&mut Backend> {
        if self.backend.is_none() { self.backend = Some(make_backend(config)?); }
        self.backend.as_mut().context("backend missing")
    }

    /// Move the resident backend out; a later `backend()` rebuilds it on demand.
    pub fn take_backend(&mut self, config: &Config) -> Result<Backend> {
        if let Some(backend) = self.backend.take() { return Ok(backend); }
        make_backend(config)
    }

    pub fn release(&mut self) {
        if let Some(backend) = self.backend.as_mut() { backend.release(); }
        self.backend = None;
    }
}

impl Default for Runtime { fn default() -> Self { Self::new() } }

pub fn make_backend(config: &Config) -> Result<Backend> {
    config.require_model()?;
    let kind = match (&config.provider, &config.endpoint) {
        (Some(provider), _) if provider == crate::llm::chatgpt::PROVIDER => {
            if let Some(endpoint) = &config.endpoint {
                ensure!(endpoint.base_url == crate::llm::chatgpt::BASE_URL && endpoint.api_key_env.is_none(), "ChatGPT subscription uses its official Responses endpoint and OAuth credentials only");
            }
            ProviderKind::ChatGpt { model:config.model.clone(), user_dir:config.user_dir.clone() }
        },
        (Some(provider), Some(endpoint)) if provider == "anthropic" => ProviderKind::AnthropicEndpoint {
            base_url: endpoint.base_url.clone(),
            model: config.model.clone(),
            api_key: crate::llm::catalog::resolve_credential(
                &config.user_dir,
                &endpoint.base_url,
                endpoint.api_key_env.as_deref(),
            )?,
        },
        (_, None) => ProviderKind::Local { model_dir: config.model.clone() },
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
        project_root: config.project_root.clone(), project_dir: config.project_root.join(".dial"),
        user_dir: config.user_dir.clone(), state_dir: config.state_dir.clone(),
    };
    let mut backend = Backend::new(kind, config.python.clone(), config.worker_timeout_secs.min(600))
        .with_remote_idle_timeout(config.remote_model_idle_timeout_secs)
        .with_allow_insecure(config.endpoint.as_ref().is_some_and(|e| e.allow_insecure))
        .with_metadata(llm::catalog::model_metadata(&layout, &config.selection()));
    if let Some(profile) = config.active_model_profile() { backend = backend.with_profile(profile.clone()); }
    else if config.provider.as_deref()==Some(llm::chatgpt::PROVIDER) {
        if let Some(profile)=llm::chatgpt::model_profile(&config.user_dir,&config.model)? { backend=backend.with_profile(profile); }
    }
    Ok(backend)
}

struct CheckpointObserver(Arc<Checkpoints>);

impl tools::WriteObserver for CheckpointObserver {
    fn before_write(&self, abs: &Path) -> Result<()> { self.0.before_write(abs) }
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
    pub reasoning_tokens: u64,
    pub compactions: u64,
    pub tool_contract_errors: u64,
    pub artifacts_retrieved: u64,
    pub dynamic_tools_used: u64,
    pub truncated_recoveries: u64,
    pub turns_completed: u64,
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
    cur_step: u32,
    pub last_prompt_tokens: u64,
    /// How many conversation messages the request behind `last_prompt_tokens` carried.
    reported_messages: usize,
    step_probe: Option<crate::learn::probe::StepProbe>,
    enabled_tools: HashSet<String>,
    metrics: AgentMetrics,
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
            None => Version { id: 0, parent: None, kind: "config".into(), harness: config.harness.clone(), adapter: None, status: "active".into() },
        };
        let legacy_memory_migrated = crate::config::types::migrate_memory_tools(&mut version.harness);
        config.validate_harness(&version.harness)?;
        version.harness = config.effective_harness(&version.harness)?;
        let memory = if config.memory.enabled { Some(Arc::new(ProvenanceMemory::open(&layout)?)) } else { None };
        let checkpoints = Arc::new(Checkpoints::new(layout.checkpoints_dir().join(session.id()), layout.project_root.clone())?);
        let mode = version.harness.edit_mode.unwrap_or(config.edit_mode);
        layout.ensure_state_outside_project()?;
        let workspace = configure(Workspace::new(&layout.project_root)?, config, mode, Some(&layout), Some(checkpoints.clone()))?
            .with_artifacts(tools::artifact::ArtifactStore::new(&layout.state_dir.join("artifacts"))?);
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
            config, backend: Some(backend), layout: Some(layout), session, workspace, ext: Registry::standard(config), todo: TodoList::default(), memory,
            checkpoints: Some(checkpoints), store, approval: config.approval, always, interrupt,
            plan_mode: false, session_cost: None, pending_plan: None,
            hermetic: false, domain_override: None, trajectory: None, last_trajectory: None, training_capture: training::Capture::default(), logging: true,
            verifications: Vec::new(), cur_step: 0, last_prompt_tokens: 0, reported_messages: 0, version, system: String::new(), parts,
            subagent_backend,
            role_backends: BTreeMap::new(),
            context_cap: None,
            enabled_tools: HashSet::new(), metrics: AgentMetrics::default(), live_tasks: live_tasks::LiveTasks::default(), superseded: evidence::Supersession::default(),
            legacy_memory_migrated,
        };
        if let Some(memory) = &agent.memory {
            for tool in crate::storage::provenance::tools(memory.clone(), agent.session.id().to_owned()) { agent.ext.register(tool); }
        }
        agent.restore_control();
        agent.system = agent.build_system();
        Ok(agent)
    }

    /// Hermetic agent for sealed evaluation and self-play: no user memory, skills, context files or
    /// persistence of its own. With a `store` it records trajectories (self-play only).
    fn hermetic(config: &'a Config, backend: Backend, version: &Version, workspace: &Path,
                store: Option<&'a Store>, domain: Option<String>) -> Result<Self> {
        let mut version = version.clone();
        // Bound evaluation cost: a stuck candidate must fail, not run for an hour.
        version.harness.max_steps = version.harness.max_steps.min(24);
        let legacy_memory_migrated = crate::config::types::migrate_memory_tools(&mut version.harness);
        config.validate_harness(&version.harness)?;
        let mut strict = config.clone();
        strict.sandbox.mode = SandboxMode::Strict;
        let mode = version.harness.edit_mode.unwrap_or(config.edit_mode);
        let workspace = configure_strict(Workspace::new(workspace)?, &strict, mode)?;
        let mut agent = Self {
            step_probe: None,
            config, backend: Some(backend), layout: None, session: Session::ephemeral(), workspace, ext: Registry::new(), todo: TodoList::default(), memory: None,
            checkpoints: None, store, approval: Approval::Auto, always: Vec::new(),
            plan_mode: false, session_cost: None, pending_plan: None,
            interrupt: Arc::new(AtomicBool::new(false)), hermetic: true, domain_override: domain, trajectory: None, last_trajectory: None,
            training_capture: training::Capture::default(), logging: true, verifications: Vec::new(), cur_step: 0, last_prompt_tokens: 0, reported_messages: 0, version,
            system: String::new(),
            parts: PromptParts { soul: None, overrides: (None, None), context: Vec::new(), skills: String::new(), memory: String::new() },
            subagent_backend: None,
            role_backends: BTreeMap::new(),
            context_cap: None,
            enabled_tools: HashSet::new(), metrics: AgentMetrics::default(), live_tasks: live_tasks::LiveTasks::default(), superseded: evidence::Supersession::default(),
            legacy_memory_migrated,
        };
        agent.system = agent.build_system();
        Ok(agent)
    }

    fn build_system(&self) -> String {
        let mut system = prompt::system_prompt(&prompt::Inputs {
            cwd: self.workspace.root(), harness: &self.version.harness, edit_mode: self.edit_mode(), soul: self.parts.soul.clone(),
            overrides: self.parts.overrides.clone(), context: &self.parts.context, skills: self.parts.skills.clone(),
            memory: self.parts.memory.clone(), tools: &self.offered(),
        });
        if !self.hermetic { system.push_str("\nDiscover specialist capabilities with tool_search, then tool_load their exact names. Loaded schemas are available only in the next model round, never the same call batch. Use working_state to preserve goals, decisions, constraints and open work with evidence links. artifact_read recovers retained output across compaction. task action=start creates independent live runs; status/wait/message/cancel control them. Children cannot spawn descendants."); }
        if self.plan_mode {
            system.push_str("\n\nPlan mode: explore using read-only tools and produce a concrete plan. Do not mutate files or execute code. Use exit_plan with your complete plan to request user approval before implementation.");
        }
        system
    }

    fn edit_mode(&self) -> crate::config::types::EditModeConfig { self.version.harness.edit_mode.unwrap_or(self.config.edit_mode) }

    /// Pick up a version the improvement loop admitted since this session started (between turns only,
    /// so a turn never changes harness mid-flight).
    fn refresh_version(&mut self, ui: &mut dyn Ui) -> Result<()> {
        let Some(store) = self.store else { return Ok(()) };
        if self.hermetic || self.version.id == 0 { return Ok(()); }
        let mut active = store.active()?;
        if active.id == self.version.id { return Ok(()); }
        let migrated = crate::config::types::migrate_memory_tools(&mut active.harness);
        if migrated { ui.notice("Migrated legacy memory tool selection to provenance memory tools for this runtime; admitted history is unchanged."); }
        self.config.validate_harness(&active.harness)?;
        active.harness = self.config.effective_harness(&active.harness)?;
        crate::learn::improve::verify_adapter(store, &active)?;
        self.version = active;
        self.workspace = self.workspace.clone().with_edit_mode(tools::EditMode::from(self.edit_mode()));
        self.system = self.build_system();
        ui.notice(&format!("now running improved version v{} (admitted by the evaluation gate)", self.version.id));
        Ok(())
    }

    pub fn session(&self) -> &Session { &self.session }
    pub fn session_mut(&mut self) -> &mut Session { &mut self.session }
    pub fn version(&self) -> &Version { &self.version }
    pub fn todo(&self) -> &TodoList { &self.todo }
    pub fn restore_todo(&mut self, snapshot: &Value) -> Result<()> {
        self.todo.restore(snapshot).map_err(anyhow::Error::msg)
    }
    pub fn checkpoints(&self) -> Option<&Arc<Checkpoints>> { self.checkpoints.as_ref() }
    pub fn set_approval(&mut self, approval: Approval) { self.approval = approval; }
    pub fn approval(&self) -> Approval { self.approval }
    pub fn swap_session(&mut self, session: Session) {
        self.live_tasks.cancel_all();
        self.session = session;
        self.last_prompt_tokens = 0;
        self.reported_messages = 0;
        self.restore_control();
        if let Some(memory) = &self.memory {
            for tool in crate::storage::provenance::tools(memory.clone(), self.session.id().to_owned()) { self.ext.register(tool); }
        }
        self.system = self.build_system();
    }
    pub fn backend_description(&self) -> String { self.backend.as_ref().map_or("none".into(), Backend::describe) }
    /// The window to plan against: what is configured, limited by what the server really loaded.
    pub fn context_window(&self) -> u32 { self.config.context_window.min(self.context_cap.unwrap_or(u32::MAX)) }
    pub fn tool_names(&self) -> Vec<String> { self.offered() }
    pub fn available_tool_names(&self) -> Vec<String> { self.catalog() }
    pub fn session_cost(&self) -> Option<f64> { self.session_cost }
    pub fn set_session_cost(&mut self, cost: Option<f64>) { self.session_cost = cost; }
    pub fn metrics(&self) -> &AgentMetrics { &self.metrics }
    pub fn register_tool(&mut self, tool: Arc<dyn tools::ext::Tool>) -> Result<()> {
        self.validate_tool_registration(tool.as_ref())?;
        self.ext.register(tool); self.system = self.build_system(); Ok(())
    }
    pub fn register_tools(&mut self, tools: Vec<Arc<dyn tools::ext::Tool>>) -> Result<()> {
        let mut names = HashSet::new();
        for tool in &tools {
            self.validate_tool_registration(tool.as_ref())?;
            ensure!(names.insert(tool.name()), "duplicate tool name in extension batch");
        }
        drop(names);
        for tool in tools { self.ext.register(tool); }
        self.system = self.build_system(); Ok(())
    }
    fn validate_tool_registration(&self, tool: &dyn tools::ext::Tool) -> Result<()> {
        ensure!(tool.extension_api_version() == tools::ext::EXTENSION_API_VERSION, "unsupported extension API version");
        let name = tool.name();
        ensure!(!tools::BUILTIN.contains(&name) && !tools::EXTRA_TOOLS.contains(&name) && !matches!(name, "exit_plan" | "task_message"), "extension cannot replace a core/control tool");
        ensure!(!self.ext.contains(name) && !self.config.tools.contains_key(name) && !self.version.harness.tools.iter().any(|n| n == name), "tool name already registered");
        ensure!(!name.is_empty() && name.len() <= 128 && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')), "invalid extension tool name");
        ensure!(tool.schema()["function"]["name"] == name, "extension schema name does not match tool name");
        Ok(())
    }
    fn restore_control(&mut self) {
        let state = self.session.control_state();
        self.enabled_tools = state["enabled_tools"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        self.todo = TodoList::default();
        if let Some(snapshot) = state.get("todo") { let _ = self.todo.restore(snapshot); }
    }
    fn catalog(&self) -> Vec<String> {
        let mut names: Vec<String> = self.version.harness.tools.iter().cloned().chain(tools::EXTRA_TOOLS.iter().map(|s| s.to_string())).chain(self.ext.names().map(str::to_owned)).chain(self.config.tools.keys().cloned()).collect();
        names.retain(|n| tools::BUILTIN.contains(&n.as_str()) || matches!(n.as_str(), "todo" | "finish" | "tool_search" | "tool_load" | "artifact_read" | "working_state") || (self.layout.is_some() && matches!(n.as_str(), "task" | "skill" | "skill_manage" | "session_search")) || self.ext.contains(n) || self.config.tools.contains_key(n));
        if let Some(allowed) = self.config.active_model_profile().and_then(|p| p.tools.as_ref()) { names.retain(|name| allowed.contains(name)); }
        if self.config.active_model_profile().is_some_and(|p| p.capabilities.tools == Some(false)) { names.clear(); }
        if self.hermetic { names.retain(|n| tools::BUILTIN.contains(&n.as_str()) || n == "todo"); }
        names.sort(); names.dedup(); names
    }
    pub fn plan_mode(&self) -> bool { self.plan_mode }
    pub fn set_plan_mode(&mut self, enabled: bool) {
        self.plan_mode = enabled && !self.hermetic;
        self.system = self.build_system();
    }
    pub fn permission_rules(&self) -> &[approval::PermissionRule] { &self.always }
    pub fn set_permission_rules(&mut self, rules: Vec<approval::PermissionRule>) { self.always = rules; }
    pub fn add_permission_rule(&mut self, rule: approval::PermissionRule, persist: bool) -> Result<()> {
        rule.validate()?;
        let mut rules = self.always.clone();
        rules.push(rule);
        if persist {
            let layout = self.layout.as_ref().context("persistent permissions unavailable here")?;
            layout.ensure_state_outside_project()?;
            approval::save(&layout.state_dir.join("permissions.toml"), &rules)?;
        }
        self.always = rules;
        Ok(())
    }
    pub fn remove_permission_rule(&mut self, index: usize, persist: bool) -> Result<()> {
        ensure!(index < self.always.len(), "permission rule index out of range");
        let mut rules = self.always.clone();
        rules.remove(index);
        if persist {
            let layout = self.layout.as_ref().context("persistent permissions unavailable here")?;
            layout.ensure_state_outside_project()?;
            approval::save(&layout.state_dir.join("permissions.toml"), &rules)?;
        }
        self.always = rules;
        Ok(())
    }
    pub fn approve_user_shell(&mut self, command: &str, ui: &mut dyn Ui) -> bool {
        let call = ToolCall { id: String::new(), name: "bash".into(), arguments: json!({"command":command}), malformed_arguments: false };
        self.authorize_call(&call, ui).is_ok()
    }

    /// LM Studio loads models on demand with a context size of its own choosing, often much smaller
    /// than Dial's default. Its native API reports the loaded size; other servers answer 404 and are
    /// left alone. An unloaded model keeps the last known cap rather than forgetting it.
    fn probe_context(&mut self) {
        if self.hermetic { return; }
        let Some(endpoint) = &self.config.endpoint else { return };
        let token = crate::llm::catalog::resolve_credential(&self.config.user_dir, &endpoint.base_url, endpoint.api_key_env.as_deref()).ok().flatten();
        if let Some(loaded) = crate::llm::catalog::loaded_context(&endpoint.base_url, &self.config.model, token.as_deref()) {
            self.context_cap = Some(loaded);
        }
    }

    /// Tools actually offered: the harness selection minus anything unavailable in this context.
    fn offered(&self) -> Vec<String> {
        let catalog = self.catalog();
        let mut names: Vec<String> = self.version.harness.tools.iter().chain(self.enabled_tools.iter()).filter(|n| catalog.contains(n)).cloned().collect();
        // Completion is a control tool, including for older admitted harnesses that predate it.
        if !self.hermetic && catalog.iter().any(|name| name == "finish") { names.push("finish".into()); }
        names.retain(|n| !self.plan_mode || matches!(n.as_str(), "task" | "bg" | "lsp" | "ast_edit" | "working_state" | "tool_search" | "tool_load") || approval::is_read_only(&self.ext, n, &json!({})));
        if self.plan_mode { names.push("exit_plan".into()); }
        names.sort(); names.dedup(); names
    }

    fn schemas(&self, names: &[String]) -> Result<Vec<Value>> {
        let mode = tools::EditMode::from(self.edit_mode());
        let mut schemas = Vec::new();
        for name in names {
            schemas.push(match name.as_str() {
                n if tools::BUILTIN.contains(&n) => tools::schemas(&[n], mode)?.remove(0),
                n if self.ext.contains(n) => self.ext.get(n).map(|tool| tool.schema()).context("extension tool vanished")?,
                "todo" => todo::schema(),
                "finish" => completion::schema(),
                "task" => subagents::schema(self.config),
                "skill" => skills::skill_tool::schema(),
                "skill_manage" => skills::manage_tool::schema(),
                "session_search" => session::session_search_tool::schema(),
                "artifact_read" => tools::artifact::schema(),
                "working_state" => evidence::working_schema(),
                "tool_search" => json!({"type":"function","function":{"name":"tool_search","description":"Discover specialist tools by capability or name. Discovery does not enable calls: load exact names and await the next model round.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}}),
                "tool_load" => json!({"type":"function","function":{"name":"tool_load","description":"Enable available specialist tools for subsequent model rounds. Selection persists in this session. Tools loaded in this call cannot be called in the same batch.","parameters":{"type":"object","properties":{"names":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"string"}}},"required":["names"],"additionalProperties":false}}}),
                "exit_plan" => json!({"type":"function","function":{"name":"exit_plan","description":"Ask the user to approve this plan and leave plan mode.","parameters":{"type":"object","properties":{"plan":{"type":"string","minLength":1}},"required":["plan"],"additionalProperties":false}}}),
                other => json!({"type":"function","function":{"name":other,
                    "description":"Operator-approved tool. Pass the request as `input`.",
                    "parameters":{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}}}),
            });
            if self.plan_mode {
                let schema = schemas.last_mut().expect("schema was just inserted");
                let properties = &mut schema["function"]["parameters"]["properties"];
                match name.as_str() {
                    "bg" => properties["action"]["enum"] = json!(["list", "output", "wait"]),
                    "ast_edit" => properties["dry_run"] = json!({"type":"boolean","enum":[true],"default":true}),
                    "task" => {
                        let agents: Vec<&str> = subagents::AGENTS.iter().filter(|a| !a.mutates).map(|a| a.name)
                            .chain(self.config.subagents.keys().map(String::as_str)).collect();
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
        if let Some(probe) = &self.step_probe {
            let risk = probe.risk(&crate::learn::probe::features(kind, step, &data));
            if let Some(object) = data.as_object_mut() {
                object.insert("predicted_failure_risk".into(), json!(risk));
            }
        }
        if let (Some(store), Some((id, domain, task))) = (self.store, &self.trajectory) {
            store.event(&Event { trajectory: *id, version: self.version.id, domain: domain.clone(), task: task.clone(),
                step, kind: kind.into(), data, score: None })?;
        }
        Ok(())
    }

    /// Append to the durable session; training capture happens at the inference boundary.
    fn push(&mut self, message: Value) -> Result<()> {
        self.session.append(message)?;
        Ok(())
    }

    /// One user message through to a final answer (or interruption / step limit).
    pub fn turn(&mut self, user_text: &str, ui: &mut dyn Ui) -> Result<TurnReport> {
        self.turn_inner(user_text, ui, true)
    }
    /// RPC admission already cleared its interrupt before enqueueing; preserve a subsequent abort.
    pub fn turn_preserving_interrupt(&mut self, user_text: &str, ui: &mut dyn Ui) -> Result<TurnReport> {
        self.turn_inner(user_text, ui, false)
    }
    fn turn_inner(&mut self, user_text: &str, ui: &mut dyn Ui, reset_interrupt: bool) -> Result<TurnReport> {
        ensure!(!user_text.trim().is_empty() && user_text.len() <= 262_144, "message must be 1..262144 bytes");
        if reset_interrupt { self.interrupt.store(false, Ordering::SeqCst); }
        if std::mem::take(&mut self.legacy_memory_migrated) { ui.notice("Migrated legacy memory tool selection to provenance memory tools for this runtime; admitted history is unchanged."); }
        self.refresh_version(ui)?;
        for notice in self.ext.take_notices() { ui.notice(&notice); }
        self.probe_context();
        self.step_probe = self.store.map(crate::learn::probe::load).transpose()?.flatten().filter(crate::learn::probe::informative);
        self.session.repair_incomplete_tools()?;
        let message = if let Some(layout) = &self.layout {
            let (message, warnings) = context::expand_message(&layout.project_root, user_text);
            for warning in warnings { ui.notice(&warning); }
            message
        } else { json!({"role":"user","content":user_text}) };
        let checkpoint_turn = self.checkpoints.as_ref().map(|checkpoints| {
            checkpoints.begin_turn(&prompt::truncate_chars(user_text, 60))
        }).transpose()?;
        self.ext.fire_lifecycle(&LifecycleEvent {
            event: "turn_started".into(), session_id: Some(self.session.id().to_owned()),
            turn_id: checkpoint_turn.map(|turn| turn.to_string()), tool_name: None, is_error: None,
        });
        self.trajectory = match self.store {
            Some(store) if self.version.id != 0 => {
                let task = self.domain_override.as_ref().map_or_else(|| format!("session:{}", self.session.id()), |_| "selfplay".to_owned());
                let domain = self.domain_override.clone().unwrap_or_else(|| self.config.use_case.clone());
                let id = store.begin_trajectory(self.version.id, &domain, &task, user_text)?;
                Some((id, domain, task))
            }
            _ => None,
        };
        self.training_capture.clear();
        self.verifications.clear();
        self.logging = self.store.is_some();
        let mut message = message;
        // Recalled notes and verified repair patterns are decided once, for this message, and stored with it.
        // Recomputing them on every request (or putting them in the system prompt, as hints once were) would
        // change the text in front of the whole conversation and discard the provider's prompt cache.
        let mut context = String::new();
        if let Some(memory) = &self.memory {
            // The lookup is a search query, not the prompt: a pasted file or log must not fail the turn.
            let query = prompt::truncate_chars(user_text, RECALL_QUERY_CHARS);
            context = memory.context_lookup(&query, self.config.memory.context_limit, self.config.memory.context_max_bytes)?;
        }
        if let Some(hints) = self.repair_context(user_text, ui) {
            if !context.is_empty() { context.push_str("\n\n"); }
            context.push_str(&hints);
        }
        if !context.is_empty() { message["dial_recall"] = json!(context); }
        self.push(message)?;
        let mut state = self.session.control_state();
        if state["original_goal"].is_null() {
            state["original_goal"] = json!(prompt::truncate_chars(user_text, 4096));
        }
        state["turn_outcome"] = Value::Null;
        self.session.set_control_state(state)?;
        let names = self.offered();
        let mut schemas = self.schemas(&names)?;
        self.last_trajectory = self.trajectory.as_ref().map(|(id, _, _)| *id);
        let max_steps = self.version.harness.max_steps;
        let mut backend = self.backend.take().context("backend is already in use")?;
        let mut result = self.run_loop(&mut backend, &names, &mut schemas, max_steps, ui);
        if self.config.verification.enabled {
            if let (Ok(report), Some(turn)) = (&result, checkpoint_turn) {
                if report.outcome == TurnOutcome::Completed {
                    let paths = self.checkpoints.as_ref().map(|checkpoints| checkpoints.changed_files(turn))
                        .unwrap_or_default().into_iter().map(PathBuf::from).collect::<Vec<_>>();
                    let mut checks = verification::plan(self.workspace.root(), &self.config.python, &paths);
                    for custom in &self.config.verification.commands {
                        if !paths.is_empty() && (custom.file_extensions.is_empty() || paths.iter().any(|path| path.extension()
                            .and_then(|ext| ext.to_str()).is_some_and(|ext| custom.file_extensions.iter().any(|suffix| suffix == ext)))) {
                            checks.push(verification::FocusedCheck {
                                name: custom.name.clone(), command: custom.command.clone(), timeout_secs: custom.timeout_secs,
                            });
                        }
                    }
                    let mut failures = Vec::new();
                    for check in &checks {
                        if self.interrupt.load(Ordering::SeqCst) { break; }
                        let output = self.run_focused_check(check, ui);
                        let exit = verification::exit_code(&output.text);
                        if output.is_error || exit != Some(0) { failures.push(format!("{} failed:\n{}", check.name, truncate(&output.text, 4096))); }
                    }
                    for attempt in 0..self.config.verification.repair_attempts {
                        if failures.is_empty() || self.interrupt.load(Ordering::SeqCst) { break; }
                        self.push(json!({"role":"user","content":format!(
                            "Automatic focused verification failed (repair attempt {}). Fix the diagnostics, then rerun the checks:\n{}",
                            attempt + 1, failures.join("\n")
                        )}))?;
                        let repaired = self.run_loop(&mut backend, &names, &mut schemas, max_steps.min(8), ui)?;
                        if repaired.interrupted { result = Ok(repaired); break; }
                        result = Ok(repaired);
                        failures.clear();
                        for check in &checks {
                            if self.interrupt.load(Ordering::SeqCst) { break; }
                            let output = self.run_focused_check(check, ui);
                            let exit = verification::exit_code(&output.text);
                            if output.is_error || exit != Some(0) { failures.push(format!("{} failed:\n{}", check.name, truncate(&output.text, 4096))); }
                        }
                    }
                    if self.interrupt.load(Ordering::SeqCst) {
                        let steps = result.as_ref().map_or(0, |report| report.steps);
                        result = Ok(TurnReport { text: String::new(), outcome: TurnOutcome::Interrupted, steps, interrupted: true, trajectory: None });
                    } else if !failures.is_empty() {
                        let diagnostic = format!("Focused verification still fails after bounded repair: {}", failures.join("; "));
                        ui.notice(&diagnostic);
                        result = Err(anyhow::anyhow!(diagnostic));
                    }
                }
            }
        }
        self.ext.fire_lifecycle(&LifecycleEvent {
            event: "turn_finished".into(), session_id: Some(self.session.id().to_owned()),
            turn_id: checkpoint_turn.map(|turn| turn.to_string()), tool_name: None, is_error: Some(result.is_err()),
        });
        self.backend = Some(backend);
        self.session.repair_incomplete_tools()?;
        if let Ok(report) = &result {
            let mut state = self.session.control_state();
            state["turn_outcome"] = json!(report.outcome);
            self.session.set_control_state(state)?;
        }
        if let Some(plan) = self.pending_plan.take() {
            self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
        }
        if let Some((id, _, _)) = self.trajectory.take() {
            if let Some(store) = self.store {
                match &result {
                    Ok(report) => {
                        store.finish_trajectory(id, Some(&report.text), None)?;
                        if report.outcome == TurnOutcome::Completed {
                            if let Err(error) = self.persist_turn(store, id) {
                                ui.notice(&format!("training observation was not saved: {error:#}"));
                            }
                        }
                    }
                    Err(error) => store.finish_trajectory(id, None, Some(&format!("{error:#}")))?,
                }
            }
            if let Ok(report) = &result { return Ok(TurnReport { trajectory: Some(id), ..result_clone(report) }); }
        }
        result
    }

    /// Retain observations, not self-assigned rewards. A model-selected shell command can spoof a
    /// successful test invocation; only a separate verifier or user verdict supplies training labels.
    fn persist_turn(&self, store: &Store, trajectory: i64) -> Result<()> {
        store.save_messages(trajectory, &self.training_capture.payload())
    }

    fn note_verification(&mut self, call: &ToolCall, output: &ToolOutput) {
        let Some(command) = call.arguments["command"].as_str().filter(|c| is_check_command(c)) else { return };
        let exit = verification::exit_code(&output.text);
        let passed = exit == Some(0);
        self.verifications.push(passed);
        let _ = self.record(self.cur_step, "verification", json!({"command":truncate(command, 200),"exit":exit,"passed":passed}));
    }

    fn run_loop(&mut self, backend: &mut Backend, names: &[String], schemas: &mut Vec<Value>, max_steps: u32, ui: &mut dyn Ui) -> Result<TurnReport> {
        let mut names = names.to_vec();
        let h = self.config.effective_harness(&self.version.harness)?;
        let adapter = self.version.adapter.clone();
        let mut pending: Option<Pending> = None;
        let mut overflow_retried = false;
        let mut compaction_failed = false;
        let mut max_tokens = h.max_tokens;
        let mut length_nudges = 0u32;
        let mut recovering_output = false;
        let mut repeats = guard::RepeatGuard::default();
        let mut stall = guard::StallGuard::default();
        let mut completion = completion::CompletionGuard::default();
        let mut budget = guard::StepBudget::new(max_steps, if self.hermetic { 0 } else { guard::StepBudget::FREE_RETRIES });
        let mut iteration = 0u32;
        while budget.begin() {
            let step = iteration;
            iteration += 1;
            if self.take_steering(ui)? { completion.reset_for_steering(); }
            self.pump_subagents(ui);
            self.cur_step = step;
            if self.interrupt.load(Ordering::SeqCst) {
                return Ok(TurnReport { text: String::new(), outcome: TurnOutcome::Interrupted, steps: step, interrupted: true, trajectory: None });
            }
            if !compaction_failed && self.projected_prompt_tokens() > (f64::from(self.context_window()) * 0.8) as u64 {
                // A failed summary must not kill the turn: the model may still fit. Try once per turn.
                if let Err(error) = self.compact_using(backend, ui) {
                    compaction_failed = true;
                    ui.notice(&format!("context compaction could not make enough room ({error:#}); continuing"));
                }
            }
            let mut messages = vec![json!({"role":"system","content":self.system.clone()})];
            self.session.extend_messages(&mut messages);
            // Everything before the newest messages must stay byte-identical from step to step so a
            // provider or local server can reuse its prompt cache; see `evidence::project`.
            let pressure = self.projected_prompt_tokens() > (f64::from(self.context_window()) * 0.5) as u64;
            let policy = evidence::Policy {
                warm_tokens: CACHE_WARM_SUFFIX_TOKENS,
                pressure,
                protect_tokens: (self.context_window() as usize / 5).max(PRUNE_PROTECT_MIN_TOKENS),
            };
            evidence::project(&mut messages, &mut self.superseded, &policy);
            let sent_messages = messages.len() - 1;
            let request = Chat { messages, tools: std::mem::take(schemas), temperature: h.temperature, max_tokens,
                seed: if self.hermetic { Some(42 + u64::from(step)) } else { None } };
            let started = Instant::now();
            let timeout = self.config.worker_timeout_secs;
            let interrupt = self.interrupt.clone();
            let result = if self.hermetic {
                backend.chat(adapter.as_deref(), &request, timeout, None)
            } else {
                chat_with_ui(backend, adapter.as_deref(), &request, timeout, interrupt, true, ui, &mut |ui| self.pump_subagents(ui))
            };
            let request_tools = request.tools.clone();
            *schemas = request.tools;
            let context_before = request.messages;
            let reply = match result {
                Ok(reply) => reply,
                Err(error) if error.downcast_ref::<llm::Interrupted>().is_some() || self.interrupt.load(Ordering::SeqCst) => {
                    return Ok(TurnReport { text: String::new(), outcome: TurnOutcome::Interrupted, steps: step, interrupted: true, trajectory: None });
                }
                Err(error) if !self.hermetic && looks_like_overflow(&error) && !overflow_retried => {
                    overflow_retried = true;
                    ui.notice("context window exceeded; compacting and retrying");
                    self.compact_using(backend, ui)?;
                    budget.give_back();
                    continue;
                }
                Err(error) => {
                    self.record(step, "generation_error", json!({"error":format!("{error:#}"),"elapsed_ms":started.elapsed().as_millis()}))?;
                    return Err(error);
                }
            };
            ui.usage(reply.prompt_tokens, reply.completion_tokens, started.elapsed().as_secs_f64());
            self.metrics.cached_tokens += reply.cached_prompt_tokens.unwrap_or(0);
            self.metrics.reasoning_tokens += reply.reasoning_tokens.unwrap_or(0);
            ui.cached_usage(reply.cached_prompt_tokens, reply.cache_creation_tokens);
            if let Some(cost) = backend.reply_cost(&reply) { self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost); }
            if let Some(tokens) = reply.prompt_tokens { self.last_prompt_tokens = tokens; self.reported_messages = sent_messages; }
            self.probe_context();
            self.record(step, "generation", json!({"output":reply.text,"tool_calls":reply.tool_calls.len(),
                "prompt_tokens":reply.prompt_tokens,"completion_tokens":reply.completion_tokens,"elapsed_ms":started.elapsed().as_millis()}))?;
            if reply.finish_reason == "length" {
                match guard::recover_from_length(max_tokens, self.context_window(), self.last_prompt_tokens, &reply, length_nudges) {
                    guard::LengthAction::Retry { max_tokens: raised } => {
                        recovering_output = true;
                        ui.notice(&format!("the model hit its output limit ({max_tokens} tokens); retrying with {raised}"));
                        max_tokens = raised;
                        budget.refund();
                        continue;
                    }
                    guard::LengthAction::Nudge { partial, message } => {
                        recovering_output = true;
                        length_nudges += 1;
                        if let Some(text) = partial { self.push(json!({"role":"assistant","content":text}))?; }
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
            if reply.dropped_tool_call() {
                self.record(step, "tool_form_failure", json!({"kind":"dropped_call"}))?;
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.push("dropped_call".into());
                self.push(json!({"role":"user","content":"Your tool call was unreadable JSON. Issue it again with valid arguments, or answer directly."}))?;
                budget.refund();
                continue;
            }
            if reply.tool_calls.is_empty() && looks_like_text_call(&reply.text, &names) {
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
                self.push(json!({"role":"assistant","content":reply.text}))?;
                if self.take_steering(ui)? { completion.reset_for_steering(); continue; }
                if self.interrupt.load(Ordering::SeqCst) {
                    return Ok(TurnReport { text: String::new(), outcome: TurnOutcome::Interrupted, steps: step + 1, interrupted: true, trajectory: None });
                }
                if self.plan_mode && !completion::open_work(&self.todo).is_empty() {
                    return Ok(TurnReport { text: reply.text, outcome: TurnOutcome::NeedsInput, steps: step + 1, interrupted: false, trajectory: None });
                }
                match completion.stopped(&self.todo) {
                    completion::StopAction::Accept => {},
                    completion::StopAction::Continue(message) => {
                        ui.notice("Continuing: todo items are still unfinished.");
                        self.push(json!({"role":"user","content":message,"dial_completion_reminder":true}))?;
                        continue;
                    }
                    completion::StopAction::Incomplete(text) => return self.stop_turn(ui, text, step + 1),
                }
                if self.logging { self.training_capture.record(&context_before, &request_tools, llm::assistant_message(&reply)); }
                self.metrics.turns_completed += 1;
                return Ok(TurnReport { text: reply.text, outcome: TurnOutcome::Completed, steps: step + 1, interrupted: false, trajectory: None });
            }
            self.push(llm::assistant_message(&reply))?;
            let (mut form_kinds, mut rejected_calls, mut execution_failed) = (Vec::new(), Vec::new(), false);
            let mut parallel = std::collections::VecDeque::new();
            let mut stop: Option<String> = None;
            let mut round_succeeded = false;
            let mut finished = None;
            for (index, call) in reply.tool_calls.iter().enumerate() {
                if self.interrupt.load(Ordering::SeqCst) {
                    self.session.repair_incomplete_tools()?;
                    return Ok(TurnReport { text: String::new(), outcome: TurnOutcome::Interrupted, steps: step + 1, interrupted: true, trajectory: None });
                }
                let form = call_form_error(call, schemas, &names).or_else(|| {
                    (call.name == "finish" && index + 1 != reply.tool_calls.len())
                        .then(|| ("invalid_arguments", "finish must be the last call in a batch".to_owned()))
                });
                let todo_before = self.todo.snapshot();
                let mut image = None;
                let mut output = if let Some((kind, why)) = form {
                    self.metrics.tool_contract_errors += 1;
                    self.record(step, "tool_form_failure", json!({"kind":kind,"name":call.name}))?;
                    form_kinds.push(kind.to_owned());
                    if !call.malformed_arguments { rejected_calls.push(rejected_tool_call_message(call)); }
                    let output = ToolOutput { text: format!("invalid tool call: {why}"), is_error: true };
                    ui.tool_call(&call.name, &summarize(call));
                    ui.tool_timing(&call.name, 0.0);
                    ui.tool_result(&call.name, &output);
                    output
                } else {
                    ui.tool_call(&call.name, &summarize(call));
                    self.record(step, "tool_call", json!({"name":call.name,"input":call.arguments}))?;
                    if self.enabled_tools.contains(&call.name) { self.metrics.dynamic_tools_used += 1; }
                    if parallel.is_empty() && approval::parallel_read(&self.ext, &call.name, &call.arguments)
                        && !approval::denied_by_rule(&self.always, call) {
                        let count = reply.tool_calls[index..].iter().take_while(|c| {
                            approval::parallel_read(&self.ext, &c.name, &c.arguments)
                                && !approval::denied_by_rule(&self.always, c)
                                && call_form_error(c, schemas, &names).is_none()
                        }).count();
                        parallel = parallel_tools(&self.workspace, self.config, &self.ext, &reply.tool_calls[index..index + count], &self.interrupt, ui).into();
                    }
                    let (mut out, seconds) = match parallel.pop_front() {
                        Some(result) => result,
                        None if call.name == "finish" && call.arguments["outcome"] == "completed" && (execution_failed || !form_kinds.is_empty()) => {
                            (ToolOutput { text: "finish completed cannot follow a failed call in the same batch; address its error first".into(), is_error: true }, 0.0)
                        }
                        None => { let started = Instant::now(); let out = self.dispatch(backend, call, ui); (out, started.elapsed().as_secs_f64()) }
                    };
                    if call.name == "browser" && !out.is_error {
                        match tools::artifact::capture_image(self.workspace.artifacts(), &mut out) {
                            Ok(attachment) => image = attachment,
                            Err(e) => out = ToolOutput { text: format!("retaining browser screenshot: {e:#}"), is_error: true },
                        }
                    }
                    ui.tool_timing(&call.name, seconds);
                    ui.tool_result(&call.name, &out);
                    out
                };
                if output.is_error && form_kinds.is_empty() { execution_failed = true; }
                if !output.is_error { round_succeeded = true; }
                completion.observe(call, !output.is_error, todo_before != self.todo.snapshot());
                if call.name == "finish" && !output.is_error { finished = Some(completion::Finish::parse(&call.arguments).map_err(anyhow::Error::msg)?); }
                if let Some(advice) = repeats.observe(call, output.is_error) {
                    output.text.push_str("\n\n");
                    output.text.push_str(&advice);
                }
                match stall.call(call, output.is_error) {
                    guard::Stall::Warn(advice) => { output.text.push_str("\n\n"); output.text.push_str(&advice); }
                    guard::Stall::Stop(message) => { stop.get_or_insert(message); }
                    guard::Stall::None => {}
                }
                if stop.is_none() { stop = repeats.stop_message(call); }
                self.record(step, if output.is_error { "tool_error" } else { "tool_result" },
                    json!({"name":call.name,"output":truncate(&output.text, 4096)}))?;
                let artifact = match self.workspace.artifacts() {
                    Some(store) => Some(store.retain(&output.text).context("retaining complete tool evidence")?),
                    None => None,
                };
                let mut body = guard::clip_middle(&output.text, tools::MODEL_OUTPUT_LIMIT);
                if output.text.len() > tools::MODEL_OUTPUT_LIMIT { if let Some(uri) = &artifact { body.push_str(&format!("\n[Complete tool evidence: {uri}; retrieve with artifact_read]")); } }
                let mut message = llm::tool_message(&call.id, &body);
                message["dial_evidence"] = json!({"artifact":artifact});
                let clipped = output.text.len() > tools::MODEL_OUTPUT_LIMIT || output.text.contains("output omitted") || output.text.contains("bytes clipped");
                if clipped {
                    let mut state = self.session.control_state();
                    let mut uris = state["truncated_artifacts"].as_array().cloned().unwrap_or_default();
                    if let Some(uri) = &artifact { if !uris.contains(&json!(uri)) { uris.push(json!(uri)); } }
                    for uri in tools::artifact::uris(&output.text) { if !uris.contains(&json!(uri)) { uris.push(json!(uri)); } }
                    state["truncated_artifacts"] = json!(uris);
                    self.session.set_control_state(state)?;
                }
                if call.name == "read" && !output.is_error {
                    let mut observation = call.arguments.clone();
                    if let Some(args) = observation.as_object_mut() { args.remove("i"); }
                    message["dial_evidence"]["observation"] = json!(observation.to_string());
                }
                if let Some(image) = image { message["dial_images"] = json!([image]); }
                if call.name == "read" && !output.is_error && !self.hermetic {
                    if let Some(path) = call.arguments["path"].as_str() {
                        let path = Path::new(path);
                        if llm::images::is_image(path) {
                            let attachment = self.workspace.resolve(path.to_string_lossy().as_ref()).map_err(anyhow::Error::msg)
                                .and_then(|absolute| {
                                    let relative = absolute.strip_prefix(self.workspace.root()).context("image resolves outside the project")?;
                                    llm::images::attachment(self.workspace.root(), relative)
                                });
                            match attachment {
                                Ok(image) => message["dial_images"] = json!([image]),
                                Err(error) => ui.notice(&format!("Image attachment unavailable: {error:#}")),
                            }
                        }
                    }
                }
                self.push(message)?;
            }
            let warning = match stall.round(!round_succeeded) {
                guard::Stall::Stop(message) => { stop.get_or_insert(message); None }
                guard::Stall::Warn(advice) => Some(advice),
                guard::Stall::None => None,
            };
            if let Some(message) = stop {
                if self.hermetic { bail!("{message}"); }
                return self.stop_turn(ui, message, step + 1);
            }
            if let Some(advice) = warning { self.push(json!({"role":"user","content":advice}))?; }
            if let Some(plan) = self.pending_plan.take() {
                self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
            }
            names = self.offered();
            *schemas = self.schemas(&names)?;
            if let Some(finish) = finished {
                if self.take_steering(ui)? { completion.reset_for_steering(); continue; }
                if self.interrupt.load(Ordering::SeqCst) {
                    return Ok(TurnReport { text: String::new(), outcome: TurnOutcome::Interrupted, steps: step + 1, interrupted: true, trajectory: None });
                }
                if finish.outcome == TurnOutcome::Completed {
                    if self.logging { self.training_capture.record(&context_before, &request_tools, llm::assistant_message(&reply)); }
                    self.metrics.turns_completed += 1;
                }
                self.push(json!({"role":"assistant","content":finish.text}))?;
                ui.text_delta(&finish.text);
                return Ok(TurnReport { text: finish.text, outcome: finish.outcome, steps: step + 1, interrupted: false, trajectory: None });
            }
            if self.interrupt.load(Ordering::SeqCst) {
                return Ok(TurnReport { text: String::new(), outcome: TurnOutcome::Interrupted, steps: step + 1, interrupted: true, trajectory: None });
            }
            if self.take_steering(ui)? { completion.reset_for_steering(); }
            if form_kinds.is_empty() && !execution_failed && self.logging {
                self.training_capture.record(&context_before, &request_tools, llm::assistant_message(&reply));
            }
            if !form_kinds.is_empty() {
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.extend(form_kinds);
                for rejected in rejected_calls { failed.reject(rejected); }
            } else if !execution_failed {
                if let (Some(failed), Some(store), Some((id, _, _))) = (pending.take(), self.store, &self.trajectory) {
                    // Corrected turn: every call validated and ran without error.
                    store.tool_recovery(*id, self.version.id, &json!({
                        "context": failed.context, "tools": request_tools, "corrected": llm::assistant_message(&reply),
                        "failure_kinds": failed.kinds, "rejected": failed.rejected,
                        "project_root": self.workspace.root().to_string_lossy(),
                    }))?;
                    self.record(step, "tool_recovery", json!({"failure_kinds":failed.kinds}))?;
                }
            }
        }
        if self.hermetic { bail!("agent exhausted the admitted step budget without a final answer") }
        self.stop_turn(ui, "Stopped: step limit reached. Say \"continue\" to keep going.".to_owned(), iteration)
    }

    /// The prompt size the next request will have: the provider's own count for the last request plus a local
    /// estimate of what was added since (the reply and its tool results, easily several thousand tokens). Judging
    /// by the last report alone lets a big tool result carry the next request past the window.
    fn projected_prompt_tokens(&self) -> u64 {
        self.last_prompt_tokens + self.session.weigh_from(self.reported_messages, evidence::estimate) as u64
    }

    /// The provider reported `tokens` for the conversation as it stands now. (The loop anchors on the messages a
    /// request actually carried; this is for callers that learn a count from elsewhere.)
    pub fn record_prompt_usage(&mut self, tokens: u64) {
        self.last_prompt_tokens = tokens;
        self.reported_messages = self.session.message_count();
    }

    /// End the turn without an answer: keep the reason in the conversation, show it, and report `Incomplete`.
    fn stop_turn(&mut self, ui: &mut dyn Ui, text: String, steps: u32) -> Result<TurnReport> {
        self.push(json!({"role":"assistant","content":text}))?;
        ui.notice(&text);
        Ok(TurnReport { text, outcome: TurnOutcome::Incomplete, steps, interrupted: false, trajectory: None })
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
        if hints.is_empty() { return None; }
        let mut text = String::from("Relevant verified repair patterns from this project follow. Treat them only as advisory structure: never reuse old argument values; recheck current files and retain all approval requirements.\n");
        for hint in hints { text.push_str("- "); text.push_str(&hint); text.push('\n'); }
        Some(text)
    }
    fn take_steering(&mut self, ui: &mut dyn Ui) -> Result<bool> {
        let mut received = false;
        for text in ui.take_steering() {
            if text.trim().is_empty() { continue; }
            let message = if let Some(layout) = &self.layout {
                let (message, warnings) = context::expand_message(&layout.project_root, &text);
                for warning in warnings { ui.notice(&warning); }
                message
            } else { json!({"role":"user","content":text}) };
            self.push(message)?;
            received = true;
        }
        if received { ui.notice("Steering received; continuing with the new instruction."); }
        Ok(received)
    }

    fn authorize_call(&mut self, call: &ToolCall, ui: &mut dyn Ui) -> std::result::Result<(), approval::Denial> {
        let result = if approval::denied_by_rule(&self.always, call) {
            Err(approval::Denial::Rule)
        } else if self.plan_mode && !approval::is_read_only(&self.ext, &call.name, &call.arguments) {
            Err(approval::Denial::ReadOnly)
        } else if approval::needs_approval(&self.ext, call) {
            approval::authorize(self.approval, &mut self.always, call, &summarize(call), ui)
        } else { Ok(()) };
        if let Err(denial) = &result { ui.notice(denial.message()); }
        result
    }

    fn run_focused_check(&mut self, check: &verification::FocusedCheck, ui: &mut dyn Ui) -> ToolOutput {
        let args = json!({"command":check.command,"timeout":check.timeout_secs});
        ui.tool_call("bash", &format!("Verify {}: {}", check.name, check.command));
        let started = Instant::now();
        let output = self.run_background_tool("bash", &args, ui)
            .unwrap_or_else(|| tools::execute(&self.workspace, "bash", &args, check.timeout_secs));
        ui.tool_timing("bash", started.elapsed().as_secs_f64());
        ui.tool_result("bash", &output);
        output
    }

    fn dispatch(&mut self, backend: &mut Backend, call: &ToolCall, ui: &mut dyn Ui) -> ToolOutput {
        let denied = |why: &str| ToolOutput { text: why.to_owned(), is_error: true };
        if call.name == "exit_plan" && self.plan_mode {
            let Some(plan) = call.arguments["plan"].as_str().filter(|p| !p.trim().is_empty()) else { return denied("exit_plan requires a nonempty plan"); };
            match ui.approve("exit_plan", plan) {
                Decision::Allow | Decision::AllowAlways => {
                    self.set_plan_mode(false);
                    self.pending_plan = Some(plan.to_owned());
                    ui.notice(&format!("Plan approved:\n{plan}"));
                    return ToolOutput { text: "Plan approved; implementation is now permitted subject to normal tool approvals.".into(), is_error: false };
                }
                Decision::Deny => return denied(&format!("Plan rejected; remain in plan mode. {}", ui.approval_reason().unwrap_or_else(|| "Ask the user what to revise.".into()))),
            }
        }
        if let Err(denial) = self.authorize_call(call, ui) {
            let _ = self.record(self.cur_step, "tool_denied", json!({"name":call.name}));
            return denied(denial.message());
        }
        if self.interrupt.load(Ordering::SeqCst) { return interrupted_tool(); }
        let args = &call.arguments;
        let name = call.name.as_str();
        if name == "find" {
            match tools::navigation::semantic_request(&self.workspace, args) {
                Ok(Some(request)) => return self.run_semantic_find(backend, args, request, ui),
                Ok(None) => {},
                Err(text) => return ToolOutput { text, is_error: true },
            }
        }
        if let Some(out) = self.run_background_tool(name, args, ui) {
            if name == "bash" { self.note_verification(call, &out); }
            return out;
        }
        if tools::BUILTIN.contains(&name) {
            let out = tools::execute(&self.workspace, name, args, 120);
            if name == "bash" { self.note_verification(call, &out); }
            return out;
        }
        if let Some(tool) = self.ext.get(name) {
            let event = |event: &str, is_error| LifecycleEvent {
                event: event.into(), session_id: Some(self.session.id().to_owned()),
                turn_id: None, tool_name: Some(call.name.clone()), is_error,
            };
            self.ext.fire_lifecycle(&event("tool_started", None));
            let output = tool.call(&ToolContext { workspace: &self.workspace, config: self.config }, args);
            self.ext.fire_lifecycle(&event("tool_finished", Some(output.is_error)));
            return output;
        }
        match name {
            "todo" => {
                let before = self.todo.snapshot();
                let output = self.todo.call(args);
                if !output.is_error && self.todo.snapshot() != before {
                    let mut state = self.session.control_state();
                    state["todo"] = self.todo.snapshot();
                    if let Err(error) = self.session.set_control_state(state) {
                        let _ = self.todo.restore(&before);
                        return denied(&format!("could not persist todos: {error:#}"));
                    }
                }
                output
            },
            "finish" => {
                let finish = match completion::Finish::parse(args) { Ok(finish) => finish, Err(error) => return denied(&error) };
                if finish.outcome == TurnOutcome::Completed {
                    let work = completion::open_work(&self.todo);
                    if !work.is_empty() { return denied(&format!("cannot finish completed with unfinished todos:\n{work}")); }
                    self.pump_subagents(ui);
                    if self.live_tasks.status(None).ok().and_then(|jobs| jobs.as_array().cloned()).is_some_and(|jobs| jobs.iter().any(|job| job["status"] == "running")) {
                        return denied("cannot finish completed while live tasks are running; wait for and inspect their results");
                    }
                }
                ToolOutput { text: "Turn outcome accepted.".into(), is_error: false }
            },
            "tool_search" => {
                let query = args["query"].as_str().unwrap_or("").to_lowercase();
                let matches: Vec<Value> = self.catalog().into_iter().filter_map(|name| {
                    let schema = self.schemas(std::slice::from_ref(&name)).ok()?.pop()?;
                    let description = schema["function"]["description"].as_str().unwrap_or("");
                    let text = format!("{name} {description}").to_lowercase();
                    query.split_whitespace().all(|word| text.contains(word)).then(|| json!({"name":name,"description":description,"enabled":self.offered().contains(&name)}))
                }).take(32).collect();
                ToolOutput { text: json!(matches).to_string(), is_error: false }
            },
            "tool_load" => {
                let requested: Vec<String> = args["names"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
                let catalog = self.catalog();
                if requested.is_empty() || requested.iter().any(|n| !catalog.contains(n)) { return denied("tool_load contains an unavailable tool; use tool_search first"); }
                let mut enabled = self.enabled_tools.clone(); enabled.extend(requested.iter().cloned());
                let mut state = self.session.control_state(); let mut sorted: Vec<_> = enabled.iter().cloned().collect(); sorted.sort();
                state["enabled_tools"] = json!(sorted);
                if let Err(e) = self.session.set_control_state(state) { return denied(&format!("persisting enabled tools: {e:#}")); }
                self.enabled_tools = enabled; self.system = self.build_system();
                ToolOutput { text: format!("Enabled for the next model round: {}", requested.join(", ")), is_error: false }
            },
            "artifact_read" => match self.workspace.artifacts() {
                Some(store) => { let output = store.call(args); if !output.is_error { self.metrics.artifacts_retrieved += 1; if self.session.control_state()["truncated_artifacts"].as_array().is_some_and(|uris| uris.contains(&args["uri"])) { self.metrics.truncated_recoveries += 1; } } output },
                None => denied("artifacts are unavailable in hermetic runs"),
            },
            "working_state" => {
                let mut state = self.session.control_state();
                if args["action"] == "update" {
                    let mut working = args.clone(); working.as_object_mut().unwrap().remove("action"); state["working_state"] = working;
                    if let Err(e) = self.session.set_control_state(state.clone()) { return denied(&format!("{e:#}")); }
                }
                ToolOutput { text: state["working_state"].to_string(), is_error: false }
            },
            "skill" | "skill_manage" | "session_search" => match &self.layout {
                None => denied("unavailable here"),
                Some(layout) => match name {
                    "skill" => skills::skill_tool::call(layout, args),
                    "skill_manage" => skills::manage_tool::call(layout, args),
                    _ => session::session_search_tool::call(layout, args),
                },
            },
            "task" => self.run_task(backend, args, ui),
            other => match self.config.tools.get(other) {
                Some(tool) => {
                    let input = args.get("input").and_then(Value::as_str).unwrap_or("");
                    match worker::run_command(&tool.program, &tool.args, &json!({"input":input}).to_string(), tool.timeout_secs) {
                        Ok(text) => ToolOutput { text, is_error: false },
                        Err(error) => ToolOutput { text: format!("{error:#}"), is_error: true },
                    }
                }
                None => denied(&format!("unknown tool: {other}")),
            },
        }
    }
    /// Blocking I/O tools run off the controller thread so the inline UI can repaint and process input.
    fn run_background_tool(&mut self, name: &str, args: &Value, ui: &mut dyn Ui) -> Option<ToolOutput> {
        if self.hermetic { return None; }
        let builtin = tools::BUILTIN.contains(&name);
        let extension = self.ext.get(name).cloned();
        let cancellation = extension.clone();
        let external = self.config.tools.get(name).cloned();
        if !builtin && extension.is_none() && external.is_none() { return None; }
        let workspace = self.workspace.clone();
        let config = self.config.clone();
        let arguments = args.clone();
        let name = name.to_owned();
        let cancel = self.interrupt.clone();
        let active = Arc::new(AtomicBool::new(false));
        let started = active.clone();
        let serial = !approval::parallel_read(&self.ext, &name, &arguments);
        enum Event { Output(tools::ext::ToolEvent), Done(ToolOutput) }
        let (tx, rx) = mpsc::sync_channel(64);
        let worker = match thread::Builder::new().name("dial-tool".to_owned()).spawn(move || {
            let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _serial = if serial { Some(workspace.serial_operation()) } else { None };
                if cancel.load(Ordering::SeqCst) { return interrupted_tool(); }
                started.store(true, Ordering::SeqCst);
                let emit = |event| { let _ = tx.send(Event::Output(event)); };
                if name == "bash" {
                    let stream = |bytes: &[u8]| emit(tools::ext::ToolEvent { kind: "output".into(), name: name.clone(), text: String::from_utf8_lossy(bytes).into_owned(), artifact: None });
                    return tools::shell::bash_tool_streaming(&workspace, &arguments, 120, &stream, &cancel).unwrap_or_else(|text| ToolOutput { text, is_error: true });
                }
                if builtin { return tools::execute(&workspace, &name, &arguments, 120); }
                if let Some(tool) = extension { return tool.call_streaming(&ToolContext { workspace: &workspace, config: &config }, &arguments, &emit, &cancel); }
                let tool = external.expect("external tool was checked");
                match worker::run_command(&tool.program, &tool.args, &json!({"input":arguments["input"].as_str().unwrap_or("")}).to_string(), tool.timeout_secs) {
                    Ok(text) => ToolOutput { text, is_error: false }, Err(e) => ToolOutput { text: format!("{e:#}"), is_error: true },
                }
            })).unwrap_or_else(|_| ToolOutput { text: "tool worker panicked".into(), is_error: true });
            started.store(false, Ordering::SeqCst);
            let _ = tx.send(Event::Done(output));
        }) { Ok(worker) => worker, Err(e) => return Some(ToolOutput { text: format!("starting tool: {e}"), is_error: true }) };
        let output = loop {
            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(Event::Output(event)) => if event.kind == "output" { ui.tool_output(&event.name, &event.text); } else { ui.background_notice(&format!("{}: {}{}", event.name, event.text, event.artifact.map(|uri| format!("\n[Complete output: {uri}]")).unwrap_or_default())); },
                Ok(Event::Done(output)) => break output,
                Err(RecvTimeoutError::Timeout) => { ui.heartbeat(); if self.interrupt.load(Ordering::SeqCst) && active.load(Ordering::SeqCst) { if let Some(tool) = &cancellation { tool.cancel_operation(); } } self.pump_subagents(ui); },
                Err(_) => break ToolOutput { text: "tool worker ended without output".into(), is_error: true },
            }
        };
        let _ = worker.join();
        Some(output)
    }

    pub fn pump_subagents(&mut self, ui: &mut dyn Ui) {
        if self.interrupt.load(Ordering::SeqCst) { self.live_tasks.cancel_all(); }
        self.live_tasks.pump(self.approval, self.plan_mode, &mut self.always, &mut self.session_cost, ui);
        for name in self.ext.names() {
            if let Some(tool) = self.ext.get(name) {
                for event in tool.drain_notifications() { if event.kind == "output" { ui.tool_output(&event.name, &event.text); } else { ui.background_notice(&format!("{}: {}{}", event.name, event.text, event.artifact.map(|uri| format!("\n[Complete output: {uri}]")).unwrap_or_default())); } }
            }
        }
    }
    pub fn subagent_status(&mut self, id: Option<&str>, ui: &mut dyn Ui) -> Result<Value> { self.pump_subagents(ui); self.live_tasks.status(id).map_err(anyhow::Error::msg) }
    pub fn subagent_message(&self, id: &str, text: &str) -> Result<Value> { self.live_tasks.message(id, text).map_err(anyhow::Error::msg) }
    pub fn cancel_subagent(&self, id: &str) -> Result<Value> { self.live_tasks.cancel(id).map_err(anyhow::Error::msg) }
    pub fn live_task_control(&self) -> LiveTaskControl { self.live_tasks.control() }
    pub fn start_subagents(&mut self, args: &Value, ui: &mut dyn Ui) -> Result<Value> {
        let call = ToolCall { id: String::new(), name: "task".into(), arguments: args.clone(), malformed_arguments: false };
        self.authorize_call(&call, ui).map_err(|e| anyhow::anyhow!(e.message()))?;
        let mut harness = self.version.harness.clone(); harness.tools = self.offered();
        self.live_tasks.start(self.config, &self.workspace, &self.ext, self.layout.as_ref(), &harness, args).map_err(anyhow::Error::msg)
    }

    /// Delegate to the subagent runner, lending it only the pieces a child may use: the workspace,
    /// extension tools and interrupt flag, plus the approval state its mutating calls go through.
    fn run_task(&mut self, main: &mut Backend, args: &Value, ui: &mut dyn Ui) -> ToolOutput {
        self.pump_subagents(ui);
        let mut harness = self.version.harness.clone();
        harness.tools = self.offered();
        match args["action"].as_str().unwrap_or("run") {
            "start" => return live_tasks::output(self.live_tasks.start(self.config, &self.workspace, &self.ext, self.layout.as_ref(), &harness, args)),
            "status" => return live_tasks::output(self.live_tasks.status(args["id"].as_str())),
            "wait" => return live_tasks::output(self.live_tasks.wait(args["id"].as_str(), args["timeout"].as_u64().unwrap_or(30), self.approval, self.plan_mode, &mut self.always, &mut self.session_cost, ui)),
            "message" => return live_tasks::output(self.live_tasks.message(args["id"].as_str().unwrap_or(""), args["message"].as_str().unwrap_or(""))),
            "cancel" => return live_tasks::output(self.live_tasks.cancel(args["id"].as_str().unwrap_or(""))),
            "run" => {},
            _ => return ToolOutput { text: "unknown task action".into(), is_error: true },
        }
        let env = subagents::Env {
            config: self.config, workspace: &self.workspace, ext: &self.ext, layout: self.layout.as_ref(),
            harness: &harness, interrupt: &self.interrupt,
        };
        let parent = subagents::Parent {
            env, approval: self.approval, always: &mut self.always, main, side: &mut self.subagent_backend,
            cost: &mut self.session_cost,
            adapter: self.version.adapter.as_deref(),
        };
        subagents::run(parent, args, ui)
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
        let added_since = self.session.weigh_from(self.reported_messages, evidence::estimate);
        let overhead = (self.last_prompt_tokens as usize).saturating_sub(self.session_tokens().saturating_sub(added_since));
        self.compact_once(backend, ui, None)?;
        let predicted = |agent: &Self| overhead + agent.session_tokens();
        if predicted(self) * 10 <= window * 7 { return Ok(()); }
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
    fn compact_once(&mut self, backend: &mut Backend, ui: &mut dyn Ui, tail_budget: Option<usize>) -> Result<()> {
        let messages = self.session.messages();
        let window = self.context_window() as usize;
        let cut = match tail_budget {
            Some(budget) => evidence::token_cut(&messages, budget),
            None => Self::compaction_cut(&messages, window),
        }.context("nothing to compact yet")?;
        let transcript = render_transcript(&messages[..cut]);
        let request = Chat {
            messages: vec![
                json!({"role":"system","content":"You compress conversations for a coding agent. Write a dense summary that preserves: the user's goal, decisions made, files read or changed (with paths), commands run and their results, open problems and next steps. No preamble."}),
                json!({"role":"user","content":transcript}),
            ],
            tools: vec![], temperature: 0.2, max_tokens: 1200, seed: None,
        };
        let summary = self.role_chat(backend, "compaction", &request, ui)?;
        ensure!(!summary.trim().is_empty(), "model produced an empty summary");
        let goal = self.session.control_state()["original_goal"].clone();
        let evidence = messages[..cut].iter().filter_map(|m| m["dial_evidence"]["artifact"].as_str()).collect::<Vec<_>>();
        let summary = format!("{}\n\nOriginal user goal: {}\nDurable decisions/open work: {}\nRetained evidence (read and mutation results): {}", summary.trim(), goal, self.session.control_state()["working_state"], evidence.join(", "));
        self.session.compact(summary, messages.len() - cut)?;
        self.metrics.compactions += 1;
        self.last_prompt_tokens = 0;
        self.reported_messages = 0;
        ui.notice(&format!("Context compacted: summarised {cut} messages; retained {} recent messages.", messages.len() - cut));
        Ok(())
    }

    /// Where to cut history for compaction. Normally just before the second-most-recent user message, so whole
    /// recent turns stay verbatim. When that tail alone is over half the window (one user request followed by a
    /// long tool chain, the usual agent shape) it cuts by token budget instead, so compaction can actually free
    /// room mid-turn rather than failing with "nothing to compact".
    fn compaction_cut(messages: &[Value], window: usize) -> Option<usize> {
        let users: Vec<usize> = messages.iter().enumerate().filter(|(_, m)| m["role"] == "user").map(|(i, _)| i).collect();
        if let Some(&cut) = users.len().checked_sub(2).and_then(|i| users.get(i)) {
            let tail: usize = messages[cut..].iter().map(evidence::estimate).sum();
            if tail <= window / 2 { return Some(cut); }
        }
        evidence::token_cut(messages, window / 4)
    }

    fn run_semantic_find(&mut self, main: &mut Backend, args: &Value, request: tools::navigation::SemanticRequest, ui: &mut dyn Ui) -> ToolOutput {
        let result = (|| -> Result<ToolOutput> {
            let supplementary = tools::navigation::lsp_args(args).map_err(anyhow::Error::msg)?;
            let response = if request.has_candidates() {
                let chat = semantic_chat(&request, self.version.harness.max_tokens);
                self.role_chat(main, "subagent", &chat, ui).context("semantic find ranking failed")?
            } else { "{\"indices\":[]}".to_owned() };
            let mut output = ToolOutput { text: request.render_response(&response).map_err(anyhow::Error::msg)?, is_error: false };
            if !request.has_candidates() { output.text.insert_str(0, "[No source candidates; semantic model was not invoked.]\n"); }
            if let Some(arguments) = supplementary {
                let metadata = self.run_background_tool("lsp", &arguments, ui).context("semantic find supplemental metadata requires the configured lsp integration")?;
                append_find_supplement(&self.workspace, &mut output, metadata)?;
            }
            Ok(output)
        })();
        result.unwrap_or_else(|e| ToolOutput { text: format!("{e:#}"), is_error: true })
    }

    fn role_chat(&mut self, main: &mut Backend, role: &str, request: &Chat, ui: &mut dyn Ui) -> Result<String> {
        let selection = self.config.model_roles.get(role).filter(|selection| !(selection.base_url.is_none() && self.config.endpoint.is_none() && selection.model == self.config.model && selection.provider == "mlx" && self.config.provider.as_deref().is_none_or(|p| p == "mlx")));
        let Some(selection) = selection else {
            let reply = if self.hermetic {
                main.chat(self.version.adapter.as_deref(), request, self.config.worker_timeout_secs, None)?
            } else {
                let adapter = self.version.adapter.clone();
                let timeout = self.config.worker_timeout_secs;
                let interrupt = self.interrupt.clone();
                chat_with_ui(main, adapter.as_deref(), request, timeout, interrupt, false, ui, &mut |ui| self.pump_subagents(ui))?
            };
            if let Some(cost) = main.reply_cost(&reply) { self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost); }
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
            backend.chat(None, request, self.config.worker_timeout_secs, None)
        } else {
            let timeout = self.config.worker_timeout_secs;
            let interrupt = self.interrupt.clone();
            chat_with_ui(&mut backend, None, request, timeout, interrupt, false, ui, &mut |ui| self.pump_subagents(ui))
        };
        if let Ok(reply) = &result {
            if let Some(cost) = backend.reply_cost(reply) { self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost); }
        }
        backend.release();
        self.role_backends.insert(role.to_owned(), backend);
        ui.role("main", &self.config.selection().display_name());
        result.map(|reply| reply.text)
    }
}

fn semantic_chat(request: &tools::navigation::SemanticRequest, max_tokens: u32) -> Chat {
    Chat { messages: vec![json!({"role":"system","content":request.system.as_str()}), json!({"role":"user","content":request.prompt.as_str()})],
        tools: vec![], temperature: 0.0, max_tokens, seed: None }
}

fn append_find_supplement(workspace: &Workspace, output: &mut ToolOutput, metadata: ToolOutput) -> Result<()> {
    output.text.push_str("\n[Supplementary real language-server metadata; outside model ranking]\n");
    output.text.push_str(&guard::clip_middle(&metadata.text, 4096));
    if metadata.text.len() > 4096 {
        let store = workspace.artifacts().context("retaining supplementary metadata requires artifact storage")?;
        output.text.push_str(&format!("\n[Complete language-server metadata: {}]\n", store.retain(&metadata.text)?));
    }
    output.is_error |= metadata.is_error;
    Ok(())
}

fn result_clone(report: &TurnReport) -> TurnReport {
    TurnReport { text: report.text.clone(), outcome: report.outcome, steps: report.steps, interrupted: report.interrupted, trajectory: report.trajectory }
}

fn configure(ws: Workspace, config: &Config, mode: crate::config::types::EditModeConfig, layout: Option<&Layout>, checkpoints: Option<Arc<Checkpoints>>) -> Result<Workspace> {
    let mut ws = ws.with_edit_mode(tools::EditMode::from(mode));
    if let Some(checkpoints) = checkpoints { ws = ws.with_observer(Arc::new(CheckpointObserver(checkpoints))); }
    configure_sandbox(ws, config, layout)
}

fn configure_strict(ws: Workspace, config: &Config, mode: crate::config::types::EditModeConfig) -> Result<Workspace> {
    configure_sandbox(ws.with_edit_mode(tools::EditMode::from(mode)), config, None)
}

fn configure_sandbox(ws: Workspace, config: &Config, layout: Option<&Layout>) -> Result<Workspace> {
    tools::apply_sandbox_config(ws, &config.sandbox, layout.map(|l| (l.state_dir.as_path(), l.user_dir.as_path())))
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
        Self { context, kinds: Vec::new(), rejected: Vec::new() }
    }

    fn reject(&mut self, message: Value) {
        if self.rejected.len() < MAX_REPAIR_REJECTIONS
            && serde_json::to_vec(&message).is_ok_and(|bytes| bytes.len() <= MAX_REPAIR_REJECTION_BYTES)
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
fn verified_repair_hints(store: &Store, project_root: &str, query: &str, offered: &[String]) -> Result<Vec<String>> {
    let query_terms = repair_terms(query);
    if query_terms.is_empty() { return Ok(Vec::new()); }
    let offered: HashSet<&str> = offered.iter().map(String::as_str).collect();
    let threshold = if query_terms.len() <= 3 { 1 } else { 2 };
    let mut candidates: Vec<(usize, i64, String, String)> = Vec::new();
    for (trajectory, payload) in store.recent_tool_recoveries(project_root, 8)? {
        if payload["project_root"].as_str() != Some(project_root) { continue; }
        let Some(context) = payload["context"].as_array() else { continue };
        let Some(prompt) = context.iter().rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str()) else { continue };
        let prior_terms = repair_terms(prompt);
        let overlap = query_terms.intersection(&prior_terms).count();
        if overlap < threshold { continue; }
        let kinds = payload["failure_kinds"].as_array().into_iter().flatten()
            .filter_map(Value::as_str)
            .map(|kind| kind.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(32).collect::<String>())
            .filter(|kind| !kind.is_empty()).take(3).collect::<Vec<_>>();
        let calls = payload.pointer("/corrected/tool_calls").and_then(Value::as_array).into_iter().flatten();
        for call in calls {
            let Some(name) = call.pointer("/function/name").and_then(Value::as_str) else { continue };
            if !offered.contains(name) { continue; }
            let argument = call.pointer("/function/arguments").and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            let mut keys = argument.as_ref().and_then(Value::as_object)
                .map(|object| object.keys().filter(|key| key.len() <= 32 && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))).cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            keys.sort();
            keys.truncate(5);
            let signature = format!("{name}:{}:{}", keys.join(","), kinds.join(","));
            let reason = if kinds.is_empty() { "a malformed call".to_owned() } else { kinds.join(", ") };
            let fields = if keys.is_empty() { "its schema".to_owned() } else { keys.join(", ") };
            let hint = format!("Similar verified repair used `{name}` with fields {fields} after {reason}. Re-read and validate current values.");
            candidates.push((overlap, trajectory, signature, truncate(&hint, 240)));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut seen = HashSet::new();
    Ok(candidates.into_iter().filter_map(|(_, _, signature, hint)| {
        seen.insert(signature).then_some(hint)
    }).take(3).collect())
}

/// Deterministic call-form check against the schema we offered. `None` means the call is well formed
/// (it may still fail at runtime, which is environment feedback rather than a model skill).
fn call_form_error(call: &ToolCall, schemas: &[Value], names: &[String]) -> Option<(&'static str, String)> {
    if call.malformed_arguments || call.name.is_empty() {
        return Some(("malformed_arguments", "tool arguments were not a valid JSON object".into()));
    }
    if !names.iter().any(|n| *n == call.name) {
        return Some(("unknown_tool", format!("unknown or unavailable tool: {}", call.name)));
    }
    let parameters = schemas.iter().find(|s| s.pointer("/function/name").and_then(Value::as_str) == Some(call.name.as_str()))
        .and_then(|s| s.pointer("/function/parameters"))?;
    calls::validate(parameters, &call.arguments).err().map(|why| ("schema", why))
}

fn summarize(call: &ToolCall) -> String {
    let a = &call.arguments;
    let text = match call.name.as_str() {
        "bash" => a["command"].as_str().unwrap_or("").to_owned(),
        "write" | "read" => a["path"].as_str().unwrap_or("").to_owned(),
        "edit" => a["input"].as_str().map(|s| s.to_owned()).or_else(|| a["path"].as_str().map(str::to_owned)).unwrap_or_default(),
        "task" => subagents::summarize(a),
        _ => a.to_string(),
    };
    truncate(&text, 1500)
}

fn render_transcript(messages: &[Value]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("?");
        match role {
            "tool" => out.push_str(&format!("[tool result] {}\n", guard::clip_middle(m["content"].as_str().unwrap_or(""), 1500))),
            _ => {
                if let Some(calls) = m["tool_calls"].as_array() {
                    for c in calls {
                        out.push_str(&format!("[assistant called {}] {}\n", c["function"]["name"].as_str().unwrap_or("?"),
                            truncate(c["function"]["arguments"].as_str().unwrap_or(""), 800)));
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
        ensure!(!rel.is_empty() && p.components().all(|c| matches!(c, std::path::Component::Normal(_))), "seeded file path must be relative");
        let target = dir.join(p);
        if let Some(parent) = target.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(target, content)?;
    }
    Ok(())
}

/// Run the agent hermetically on one task and return whether `check` passes on the final workspace
/// and answer. A stuck agent (step budget exhausted) fails the case; infrastructure errors propagate
/// so a broken model server aborts an evaluation instead of silently scoring zero.
pub fn check_case(runtime: &mut Runtime, config: &Config, version: &Version, prompt: &str, files: &BTreeMap<String, String>, check: &Check) -> Result<bool> {
    ensure!(!prompt.trim().is_empty() && prompt.len() <= 65536, "prompt must be 1..65536 bytes");
    let workspace = tempfile::tempdir()?;
    seed_workspace(workspace.path(), files)?;
    let mut agent = Agent::hermetic(config, runtime.take_backend(config)?, version, workspace.path(), None, None)?;
    let result = agent.turn(prompt, &mut NullUi);
    runtime.backend = agent.backend.take();
    let answer = match result {
        Ok(report) => report.text,
        Err(error) if format!("{error:#}").contains(BUDGET_MSG) => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(check.verify(workspace.path(), &answer))
}

/// Self-play: like `check_case`, but the trajectory is recorded (domain `selfplay:<family>`) with the
/// verifier's verdict as its outcome, so passing runs become training data and failures become signal.
pub fn selfplay_run(runtime: &mut Runtime, config: &Config, store: &Store, version: &Version,
                    family: &str, prompt: &str, files: &BTreeMap<String, String>, check: &Check) -> Result<bool> {
    let workspace = tempfile::tempdir()?;
    seed_workspace(workspace.path(), files)?;
    let mut agent = Agent::hermetic(config, runtime.take_backend(config)?, version, workspace.path(), Some(store), Some(format!("selfplay:{family}")))?;
    let result = agent.turn(prompt, &mut NullUi);
    runtime.backend = agent.backend.take();
    let (trajectory, text, completed) = match result {
        Ok(report) => (report.trajectory, report.text, !report.interrupted),
        Err(error) if format!("{error:#}").contains(BUDGET_MSG) => (agent.last_trajectory, String::new(), false),
        Err(error) => return Err(error),
    };
    let passed = completed && check.verify(workspace.path(), &text);
    if let Some(id) = trajectory {
        store.outcome(&crate::config::types::Outcome { trajectory: id, score: if passed { 100.0 } else { 0.0 },
            source: format!("verifier:{family}"), reference: None })?;
    }
    Ok(passed)
}

/// Commands whose exit status is a meaningful external verdict on the work (tests, builds, linters).
fn is_check_command(command: &str) -> bool {
    const MARKERS: &[&str] = &["pytest", "cargo test", "cargo check", "cargo build", "cargo clippy", "npm test", "npm run test",
        "npm run build", "npm run lint", "yarn test", "pnpm test", "go test", "go build", "go vet", "make test", "make check",
        "make build", "tsc", "mvn test", "gradle test", "ruff", "flake8", "mypy", "eslint", "unittest", "swift test",
        "dotnet test", "bun test", "deno test", "rspec", "phpunit", "ctest"];
    let c = command.to_lowercase();
    MARKERS.iter().any(|m| c.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_slow_reads_overlap_and_results_stay_ordered() {
        struct SlowRead { entered: std::sync::atomic::AtomicUsize }
        impl tools::ext::Tool for SlowRead {
            fn name(&self) -> &str { "web_fetch" }
            fn schema(&self) -> Value { json!({}) }
            fn needs_approval(&self, _: &Value) -> bool { false }
            fn call(&self, _: &ToolContext, args: &Value) -> ToolOutput {
                self.entered.fetch_add(1, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(2);
                while self.entered.load(Ordering::SeqCst) < 2 && Instant::now() < deadline { thread::sleep(Duration::from_millis(1)); }
                let overlap = self.entered.load(Ordering::SeqCst) >= 2;
                if args["index"] == 0 { thread::sleep(Duration::from_millis(30)); }
                ToolOutput { text: args["index"].to_string(), is_error: !overlap }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let mut ext = Registry::new();
        ext.register(Arc::new(SlowRead { entered: std::sync::atomic::AtomicUsize::new(0) }));
        let calls: Vec<_> = (0..2).map(|index| ToolCall { id:index.to_string(), name:"web_fetch".into(), arguments:json!({"index":index}), malformed_arguments:false }).collect();
        let outputs = parallel_tools(&workspace, &Config::default(), &ext, &calls, &AtomicBool::new(false), &mut NullUi);
        assert!(outputs.iter().all(|(out, _)| !out.is_error), "reads must overlap instead of timing out waiting for each other");
        assert_eq!(outputs.iter().map(|(out, _)| out.text.as_str()).collect::<Vec<_>>(), ["0", "1"]);
    }

    #[test]
    fn repair_hints_are_project_scoped_relevant_and_omit_old_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "test-model", "test-usecase", &Default::default()).unwrap();
        let version = store.active().unwrap().id;
        let trajectory = store.begin_trajectory(version, "coding", "repair", "input").unwrap();
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
        let hints = verified_repair_hints(&store, "/workspace/a", "Please fix the parser for emoji filenames", &offered).unwrap();
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("edit") && hints[0].contains("path") && hints[0].contains("schema"));
        assert!(!hints[0].contains("secret.txt") && !hints[0].contains("SECRET_VALUE"), "hints must not replay historical argument values");
        assert!(verified_repair_hints(&store, "/workspace/b", "fix the parser for emoji filenames", &offered).unwrap().is_empty());
        assert!(verified_repair_hints(&store, "/workspace/a", "weather forecast paris", &offered).unwrap().is_empty());
        assert!(verified_repair_hints(&store, "/workspace/a", "fix the parser for emoji filenames", &["read".into()]).unwrap().is_empty());
    }

    #[test]
    fn slow_streams_keep_the_ui_alive_before_the_first_token() {
        struct RecordingUi { heartbeats: usize, text: String }
        impl Ui for RecordingUi {
            fn heartbeat(&mut self) { self.heartbeats += 1; }
            fn text_delta(&mut self, text: &str) { self.text.push_str(text); }
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
        let mut ui = RecordingUi { heartbeats: 0, text: String::new() };
        let reply = chat_with_ui(&mut backend, None, &request, 5, Arc::new(AtomicBool::new(false)), true, &mut ui, &mut |_| {}).unwrap();
        assert_eq!(reply.text, "hello");
        assert_eq!(ui.text, "hello");
        assert!(ui.heartbeats >= 1, "the UI should repaint during the fake model's initial delay");
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn background_waits_keep_the_ui_pumping() {
        struct CountingUi(usize);
        impl Ui for CountingUi {
            fn heartbeat(&mut self) { self.0 += 1; }
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let _ = tx.send(());
        });
        let mut ui = CountingUi(0);
        receive_with_heartbeat(&rx, &mut ui).unwrap();
        assert!(ui.0 >= 1, "the UI must repaint while the operation is pending");
    }

    #[test]
    fn continuous_model_events_do_not_starve_ui_heartbeats() {
        struct RecordingUi { heartbeats: usize, events: usize }
        impl Ui for RecordingUi {
            fn heartbeat(&mut self) { self.heartbeats += 1; }
            fn reasoning_delta(&mut self, _: &str) {
                self.events += 1;
                // Simulate rendering while the worker queues the rest of a fast response.
                thread::sleep(Duration::from_millis(3));
            }
        }
        let event = format!("data: {}\n\n", json!({"choices":[{"delta":{"reasoning_content":"thinking"}}]}));
        let body = event.repeat(100) + &format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"delta":{},"finish_reason":"stop"}]}));
        let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Sse(body)]);
        let mut backend = Backend::new(ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url), model: "test".into(), api_key: None,
        }, PathBuf::new(), 5);
        let request = Chat { messages: vec![json!({"role":"user","content":"hello"})], tools: vec![], temperature: 0.2, max_tokens: 16, seed: None };
        let mut ui = RecordingUi { heartbeats: 0, events: 0 };
        chat_with_ui(&mut backend, None, &request, 5, Arc::new(AtomicBool::new(false)), false, &mut ui, &mut |_| {}).unwrap();
        assert_eq!(ui.events, 100);
        assert!(ui.heartbeats >= 2, "a continuously occupied event queue must still pump input");
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn only_real_check_commands_count_as_verification() {
        for yes in ["cargo test --lib", "python3 -m pytest -q", "npm test", "go test ./...", "python -m unittest"] { assert!(is_check_command(yes), "{yes}"); }
        for no in ["ls -la", "cat notes.txt", "echo hello", "git status", "python3 script.py"] { assert!(!is_check_command(no), "{no}"); }
    }
}

pub fn json_text(text: &str) -> &str {
    let text = text.trim();
    if let Some(body) = text.strip_prefix("```json").or_else(|| text.strip_prefix("```")) {
        body.trim().strip_suffix("```").unwrap_or(body).trim()
    } else { text }
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max { return text.to_owned(); }
    let mut end = max;
    while !text.is_char_boundary(end) { end -= 1; }
    format!("{} [truncated]", &text[..end])
}

fn looks_like_overflow(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    text.contains("context") && ["length", "window", "too long", "exceed", "too large"].iter().any(|k| text.contains(k))
}

/// A reply that is just a JSON-looking object naming an offered tool. Small models often emit
/// slightly broken ones (`{{"name": ...}` with unbalanced braces), so this matches the shape
/// rather than requiring valid JSON.
fn looks_like_text_call(text: &str, names: &[String]) -> bool {
    let t = text.trim();
    if !t.starts_with('{') || t.len() > 8000 || !t.contains("\"arguments\"") && !t.contains("\"parameters\"") { return false; }
    names.iter().any(|n| t.contains(&format!("\"name\": \"{n}\"")) || t.contains(&format!("\"name\":\"{n}\"")))
}
