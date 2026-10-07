//! Interactive controller: owns the terminal, the conversation view, the composer and the overlays, and
//! runs agent turns. Rendering is inline (scrollback-native, see `ui::term`): finished messages are
//! committed to the terminal's own scrollback, and only the live tail and the composer are repainted.
//!
//! Configuration is never edited here. Overlays emit semantic events (`OverlayEvent`); this module
//! persists them through `settings_store` (which validates the merged configuration before writing),
//! reloads the effective config, and applies the result live. A change that does not touch the agent
//! (theme, status line, approval) is applied in place; one that does (model, provider, sandbox, …) makes
//! the controller rebuild the agent around the same session.
use crate::{
    agent::{self, Agent, Decision, Ui, context, make_backend, todo::TodoList},
    config::types::{Approval, Config, Outcome},
    learn::auto,
    llm::{
        catalog,
        model_config::{ModelSelection, ProviderSpec, ROLES, UiConfig},
    },
    repl::{self, OutputFormat, PrintExit, PrintUi},
    storage::{
        paths::Layout,
        session::{Session, SessionMeta},
        settings_store::{self, Change, Scope},
        skills,
        store::Store,
    },
    tools::ToolOutput,
    ui::{
        Rows,
        composer::{CommandEntry, CommandKind, Composer, ComposerCtx, ComposerEvent},
        keys::{Action, Keymap},
        markdown::truncate_spans,
        overlay::{
            DrawCtx, OverlayEvent,
            compute::ComputeOverlay,
            models::ModelOverlay,
            palette::{PaletteEntry, PaletteOverlay},
            providers::ProviderOverlay,
            settings::SettingsOverlay,
            shortcuts::ShortcutOverlay,
        },
        settings_schema,
        status::{Lora, Phase, StatusData},
        statusline, tasks,
        term::{InlineTerminal, OverlayTerminal, Viewport},
        text,
        theme::{Sym, ThemeManager, Tok},
        transcript::{Transcript, TranscriptCtx, TurnStats},
        welcome::{self, WelcomeInfo},
    },
};
use anyhow::{Context, Result, ensure};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, poll, read};
use fs2::FileExt;
use ratatui::text::{Line, Span};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// How long a keystroke may go unnoticed while the UI is idle; also the animation frame time.
const TICK: Duration = Duration::from_millis(80);
/// A git call may never hold the interface hostage.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_TTL: Duration = Duration::from_secs(5);
/// Learning state is a store query, so it is refreshed on a timer, not per keystroke.
const STATUS_TTL: Duration = Duration::from_secs(2);
/// Streaming text repaints at most this often; tool events always repaint.
const PAINT_EVERY: Duration = Duration::from_millis(40);

pub struct Options {
    pub session: Option<Session>,
    pub prompt: String,
    pub interactive: bool,
    /// Command-line flag overrides, layered on top of the config files every time the config is reloaded.
    pub overlay: toml::Table,
    /// Startup messages for the transcript (learning monitor results, paused state, …).
    pub notices: Vec<String>,
}

/// Run the inline interface, or answer a single piped message.
pub fn run_interactive(layout: Layout, config: Config, options: Options) -> Result<()> {
    if !options.interactive {
        return run_plain(layout, config, options);
    }
    let mut app = App::new(layout, config, options.overlay)?;
    let mut shell = Shell::new(&app, options.prompt)?;
    for notice in &options.notices {
        shell.transcript.notice("Learning", notice);
    }
    let outcome = drive(&mut app, &mut shell, options.session);
    let resume = shell.finish();
    schedule(&app);
    if let Some(id) = resume {
        eprintln!("dial: session {} · continue with `dial -c`", short(&id));
    }
    outcome
}

/// Piped text uses the same status contract as explicit print mode.
fn run_plain(layout: Layout, config: Config, options: Options) -> Result<()> {
    let code = run_print(layout, config, options, OutputFormat::Text, Vec::new())?;
    if code != 0 {
        return Err(PrintExit(code).into());
    }
    Ok(())
}

/// Run one turn without terminal control sequences; JSON modes reserve stdout for their protocol.
pub fn run_print(
    layout: Layout,
    config: Config,
    options: Options,
    format: OutputFormat,
    allowed: Vec<String>,
) -> Result<i32> {
    let started = Instant::now();
    let mut ui = PrintUi::new(format, allowed);
    config.require_model()?;
    let _compute = if config.uses_local_compute() {
        Some(auto::compute_lock(&layout)?)
    } else {
        None
    };
    let _owner = if _compute.is_some() {
        Some(crate::learn::compute::owner_record(
            &layout.user_dir,
            "session",
            &layout.project_root,
            &config.state_dir,
            &config.model,
        )?)
    } else {
        None
    };
    let app = App::new(layout, config, options.overlay)?;
    let session = match options.session {
        Some(session) => session,
        None => Session::create(
            &app.layout,
            SessionMeta {
                model: app.config.model.clone(),
            },
        )?,
    };
    let session_id = session.id().to_owned();
    let mut agent = app.make_agent(session)?;
    ui.system(
        &session_id,
        &app.config.model,
        &app.layout.project_root,
        &agent.tool_names(),
    );
    for notice in &options.notices {
        ui.notice(notice);
    }
    let completed_before = agent.metrics().turns_completed;
    let result = agent.turn(options.prompt.trim(), &mut ui);
    ui.set_completed(agent.metrics().turns_completed > completed_before);
    schedule(&app);
    match result {
        Ok(report) => ui.finish(Some(&report), None, Some(&session_id), started.elapsed()),
        Err(error) => {
            let error = format!("{error:#}");
            eprintln!("error: {error}");
            ui.finish(None, Some(&error), Some(&session_id), started.elapsed())
        }
    }
}

// ---------------------------------------------------------------------------
// Application state. The agent borrows `config` and `store` for its whole lifetime, so changes
// to either are applied between agents (see `drive`).
// ---------------------------------------------------------------------------

struct App {
    layout: Layout,
    config: Config,
    overlay: toml::Table,
    store: Option<Store>,
    store_error: Option<String>,
    interrupt: Arc<AtomicBool>,
    compute: RefCell<Option<File>>,
}

impl App {
    fn new(layout: Layout, config: Config, overlay: toml::Table) -> Result<Self> {
        let mut app = Self {
            layout,
            config,
            overlay,
            store: None,
            store_error: None,
            interrupt: repl::install_sigint(),
            compute: RefCell::new(None),
        };
        app.reopen_store();
        Ok(app)
    }

    fn open_store(config: &Config) -> Result<Store> {
        ensure!(config.improve.enabled, "automatic improvement is disabled");
        Store::open(
            &config.state_dir,
            &config.model,
            &config.use_case,
            &config.harness,
        )
    }

    fn make_agent(&self, session: Session) -> Result<Agent<'_>> {
        let mut serving = self.config.clone();
        if let Some(store) = &self.store {
            crate::learn::improve::apply_pinned_model(&mut serving, store)?;
        }
        let backend = make_backend(&serving)?;
        Agent::interactive(
            &self.config,
            backend,
            self.layout.clone(),
            session,
            None,
            self.store.as_ref(),
            self.interrupt.clone(),
        )
    }

    /// Adopt a freshly loaded, validated configuration and keep the per-model store in step.
    fn adopt(&mut self, config: Config) {
        self.config = config;
        if !self.config.uses_local_compute() {
            self.compute.get_mut().take();
        }
        self.reopen_store();
    }

    fn reopen_store(&mut self) {
        if !self.config.improve.enabled || self.config.model.is_empty() {
            self.store = None;
            self.store_error = None;
            return;
        }
        match Self::open_store(&self.config) {
            Ok(store) => {
                self.store = Some(store);
                self.store_error = None;
            }
            Err(error) => {
                self.store = None;
                self.store_error = Some(format!("self-improvement recording disabled: {error:#}"));
            }
        }
    }

    /// The effective configuration as the files and the command-line flags define it right now.
    fn reload(&self) -> Result<Config> {
        Config::load(&self.layout, Some(self.overlay.clone()))
    }

    fn release_idle_compute(&self, agent: &mut Agent<'_>) -> bool {
        if !agent.release_idle_models() {
            return false;
        }
        // Stop model processes before allowing the training worker to load its weights.
        if let Some(lock) = self.compute.borrow_mut().take() {
            crate::learn::compute::clear_owner(&self.layout.user_dir);
            // Closing only our descriptor can leave the lock alive in a duplicate inherited
            // during a concurrent process spawn. Explicitly release the shared lock first.
            let _ = fs2::FileExt::unlock(&lock);
        }
        true
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn session_age(started: &str) -> String {
    let Some(stamp) = started
        .get(..19)
        .and_then(|s| std::ffi::CString::new(s).ok())
    else {
        return started.to_owned();
    };
    // SAFETY: both strings are NUL-terminated; strptime/timegm receive a valid writable tm.
    let seconds = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::strptime(stamp.as_ptr(), c"%Y-%m-%dT%H:%M:%S".as_ptr(), &mut tm).is_null() {
            return started.to_owned();
        }
        libc::time(std::ptr::null_mut())
            .saturating_sub(libc::timegm(&mut tm))
            .max(0) as u64
    };
    if seconds < 60 {
        "now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

fn schedule(app: &App) {
    let Some(store) = &app.store else { return };
    match auto::maybe_spawn(&app.layout, &app.config, store) {
        Ok(true) => eprintln!("dial: improvement queued; it starts when local compute is idle"),
        Ok(false) => {}
        Err(error) => eprintln!("dial: could not schedule improvement: {error:#}"),
    }
}

/// What must be rebuilt when the configuration changes. `ui` and `approval` apply in place; everything
/// else (model, providers, sandbox, harness, learning) needs a new agent.
fn agent_key(config: &Config) -> String {
    let mut table = toml::Table::try_from(config).unwrap_or_default();
    table.remove("ui");
    table.remove("approval");
    table.insert(
        "effective_context_window".into(),
        i64::from(config.context_window).into(),
    );
    table.to_string()
}

// ---------------------------------------------------------------------------
// The session loop: build an agent, run the shell until it quits or the config demands a rebuild.
// ---------------------------------------------------------------------------

enum Step {
    Done,
    Quit,
    Rebuild(Box<Config>),
}

fn drive(app: &mut App, shell: &mut Shell, session: Option<Session>) -> Result<()> {
    if !onboard(app, shell)? {
        return Ok(());
    }
    let resumed = session.is_some();
    let mut session = match session {
        Some(session) => session,
        None => Session::create(
            &app.layout,
            SessionMeta {
                model: app.config.model.clone(),
            },
        )?,
    };
    let mut todo: Option<Value> = None;
    let mut approval = app.config.approval;
    let mut plan_mode = false;
    let mut permission_rules = None;
    let mut session_cost = None;
    shell.begin(app, &session, resumed);
    loop {
        let step = {
            let mut agent = app.make_agent(session)?;
            if let Some(snapshot) = &todo {
                agent.restore_todo(snapshot)?;
            }
            agent.set_approval(approval);
            agent.set_session_cost(session_cost);
            agent.set_plan_mode(plan_mode);
            if let Some(rules) = permission_rules.take() {
                agent.set_permission_rules(rules);
            }
            let step = shell.run(app, &mut agent);
            session = std::mem::replace(agent.session_mut(), Session::ephemeral());
            todo = Some(agent.todo().snapshot());
            approval = agent.approval();
            plan_mode = agent.plan_mode();
            permission_rules = Some(agent.permission_rules().to_vec());
            session_cost = agent.session_cost();
            step
        };
        shell.session_id = session.id().to_owned();
        match step? {
            Step::Quit | Step::Done => break,
            Step::Rebuild(config) => {
                app.adopt(*config);
                shell.after_adopt(app);
            }
        }
    }
    Ok(())
}

/// First run (or a provider that cannot be used): let the user pick a model before any agent exists.
fn onboard(app: &mut App, shell: &mut Shell) -> Result<bool> {
    loop {
        let ready = !app.config.model.trim().is_empty() && make_backend(&app.config).is_ok();
        if ready {
            return Ok(true);
        }
        shell.note("Welcome to dial", if app.config.model.trim().is_empty() {
            "Pick a model to start: local MLX models and running LM Studio / Ollama servers are discovered automatically; Providers adds hosted APIs."
        } else {
            "The selected provider cannot be used yet (missing credentials?). Set it up in Providers."
        });
        let which = if app.config.model.trim().is_empty() {
            Which::Models
        } else {
            Which::Providers
        };
        let outcome = shell.run_overlay(app, which)?;
        match outcome.latest {
            Some(config) => {
                app.adopt(config);
                shell.after_adopt(app);
            }
            None => {
                if shell.auth_job.is_some() {
                    shell.note(
                        "ChatGPT",
                        "Sign-in continues in your browser; select a model after it completes.",
                    );
                    continue;
                }
                shell.note(
                    "No model selected",
                    "Run dial again and choose one (Alt-M), or run `dial setup -m <mlx-model>`.",
                );
                shell.flush()?;
                return Ok(false);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery: one background scan, cached until the user asks for a refresh.
// ---------------------------------------------------------------------------

struct Catalog {
    /// (newest request id, its result). A superseded scan never overwrites a newer one.
    slot: Arc<Mutex<(u64, Option<catalog::Discovery>)>>,
    request: u64,
    worker: Option<JoinHandle<()>>,
    generation: u64,
}

struct ProbeLease(std::fs::File);
impl Drop for ProbeLease {
    fn drop(&mut self) {
        // Closing our handle alone leaves flock held by any duplicate inherited during spawn.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn shared<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Catalog {
    fn new() -> Self {
        Self {
            slot: Arc::new(Mutex::new((0, None))),
            request: 0,
            worker: None,
            generation: 0,
        }
    }

    fn get(&self) -> Option<catalog::Discovery> {
        shared(&self.slot).1.clone()
    }

    /// Reap a finished scan so a new request can start; bumps `generation` when one completed.
    fn collect(&mut self) {
        if !self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        self.worker = None;
        self.generation = self.generation.wrapping_add(1);
    }

    fn start(&mut self, app: &App) {
        self.collect();
        if self.worker.is_some() || self.get().is_some() {
            return;
        }
        self.scan(app);
    }

    fn refresh(&mut self, app: &App) {
        self.collect();
        // Config changes must not opt an idle chat into provider discovery. Once the
        // user has opened a model/provider panel, keep that requested catalog fresh.
        if self.request != 0 {
            self.scan(app);
        }
    }

    fn scan(&mut self, app: &App) {
        let layout = app.layout.clone();
        let configured = app.config.providers.clone();
        let config = app.config.clone();
        let mut selections = vec![app.config.selection()];
        selections.extend(app.config.model_roles.values().cloned());
        self.launch(move || {
            let mut discovery = catalog::discover(&layout, &configured, &selections);
            for entry in &mut discovery.models {
                if entry.selection.provider != "mlx" || entry.selection.base_url.is_some() {
                    continue;
                }
                let selected = config.with_selection(&entry.selection);
                let caps = Store::open_read_only(&selected.learning_dir(&layout))
                    .ok()
                    .and_then(|store| store.get_meta("serving_capabilities").ok().flatten())
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
                if let Some(supported) = caps
                    .filter(crate::learn::worker::current_lora_probe)
                    .and_then(|caps| caps["lora"].as_bool())
                {
                    entry.metadata.lora = Some(supported);
                    entry.metadata.lora_verified = true;
                }
            }
            discovery
        });
    }

    fn launch(&mut self, discover: impl FnOnce() -> catalog::Discovery + Send + 'static) {
        self.request += 1;
        let request = self.request;
        shared(&self.slot).0 = request;
        let slot = self.slot.clone();
        self.worker = Some(std::thread::spawn(move || {
            let found = discover();
            let mut slot = shared(&slot);
            if slot.0 == request {
                slot.1 = Some(found);
            }
        }));
    }
}

// ---------------------------------------------------------------------------
// Git and learning status, cached so a repaint never spawns a process.
// ---------------------------------------------------------------------------

/// Git status shown in the status line: (branch, optional `(ahead, behind, dirty)` counts).
type GitStatus = (String, Option<(u32, u32, u32)>);

/// Refresh optional status on a worker, keeping the last value while a query is pending.
struct StatusCache<T> {
    at: Option<Instant>,
    worker: Option<JoinHandle<T>>,
    value: T,
}

impl<T: Default> Default for StatusCache<T> {
    fn default() -> Self {
        Self {
            at: None,
            worker: None,
            value: T::default(),
        }
    }
}

impl<T: Send + 'static> StatusCache<T> {
    fn refresh(&mut self, ttl: Duration, query: impl FnOnce() -> T + Send + 'static) {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            if let Ok(value) = self.worker.take().unwrap().join() {
                self.value = value;
            }
            self.at = Some(Instant::now());
        }
        if self.worker.is_some() || self.at.is_some_and(|at| at.elapsed() < ttl) {
            return;
        }
        self.at = Some(Instant::now());
        self.worker = std::thread::Builder::new()
            .name("dial-status".into())
            .spawn(query)
            .ok();
    }
}

#[derive(Default)]
struct Git {
    cache: StatusCache<GitStatus>,
    files: StatusCache<Vec<crate::ui::rail::FileDiff>>,
}

impl Git {
    fn files(&mut self, root: &Path) -> Vec<crate::ui::rail::FileDiff> {
        let root = root.to_owned();
        self.files.refresh(GIT_TTL, move || {
            let mut files = git_output(
                &root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--numstat",
                    "-z",
                    "HEAD",
                ],
            )
            .map(|s| parse_file_diffs(&s))
            .unwrap_or_else(|| {
                // A newly initialized repository has no HEAD; show staged files without invented counts.
                git_output(&root, &["ls-files", "-z"])
                    .unwrap_or_default()
                    .split('\0')
                    .filter(|p| !p.is_empty())
                    .map(|p| crate::ui::rail::FileDiff {
                        path: p.into(),
                        untracked: true,
                        ..Default::default()
                    })
                    .collect()
            });
            for path in git_output(&root, &["ls-files", "--others", "--exclude-standard", "-z"])
                .unwrap_or_default()
                .split('\0')
                .filter(|p| !p.is_empty())
            {
                files.push(crate::ui::rail::FileDiff {
                    path: path.into(),
                    untracked: true,
                    ..Default::default()
                });
            }
            files.sort_by(|a, b| a.path.cmp(&b.path));
            files.dedup_by(|a, b| a.path == b.path);
            files
        });
        self.files.value.clone()
    }
    fn read(&mut self, root: &Path) -> GitStatus {
        let root = root.to_owned();
        self.cache.refresh(GIT_TTL, move || {
            let branch = git_output(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                .map(|b| b.trim().to_owned())
                .filter(|b| !b.is_empty() && b != "HEAD");
            let counts = branch
                .as_ref()
                .and_then(|_| {
                    git_output(
                        &root,
                        &["status", "--porcelain=v1", "--untracked-files=normal"],
                    )
                })
                .map(|out| parse_status(&out));
            (branch.unwrap_or_default(), counts)
        });
        self.cache.value.clone()
    }
}

/// (staged, modified, untracked) from `git status --porcelain=v1`.
fn parse_status(output: &str) -> (u32, u32, u32) {
    let (mut staged, mut modified, mut untracked) = (0, 0, 0);
    for line in output.lines() {
        let mut chars = line.chars();
        let (x, y) = (chars.next().unwrap_or(' '), chars.next().unwrap_or(' '));
        if x == '?' && y == '?' {
            untracked += 1;
            continue;
        }
        if x != ' ' && x != '?' {
            staged += 1;
        }
        if y != ' ' && y != '?' {
            modified += 1;
        }
    }
    (staged, modified, untracked)
}

fn parse_file_diffs(output: &str) -> Vec<crate::ui::rail::FileDiff> {
    let mut fields = output.split('\0');
    let mut out = Vec::new();
    while let Some(field) = fields.next() {
        let mut parts = field.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = fields.next();
            fields.next().unwrap_or("")
        } else {
            path
        };
        if !path.is_empty() {
            out.push(crate::ui::rail::FileDiff {
                path: path.into(),
                added: added.parse().ok(),
                removed: removed.parse().ok(),
                untracked: false,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    String::from_utf8(child.wait_with_output().ok()?.stdout).ok()
}

#[derive(Default)]
struct Learning {
    cache: StatusCache<(Option<i64>, String, Lora)>,
    state_dir: Option<std::path::PathBuf>,
}

impl Learning {
    fn read(&mut self, app: &App) {
        let state_dir = app.config.state_dir.clone();
        if self.state_dir.as_ref() != Some(&state_dir) {
            self.cache = StatusCache::default();
            self.state_dir = Some(state_dir.clone());
        }
        let config = app.config.clone();
        let available = app.store.is_some();
        self.cache.refresh(STATUS_TTL, move || {
            let store = available
                .then(|| Store::open_read_only(&state_dir).ok())
                .flatten();
            let label = improvement_label(&config, store.as_ref());
            let active = store.as_ref().and_then(|store| store.active().ok());
            let lora = lora_state(&config, store.as_ref(), active.as_ref());
            (active.map(|version| version.id), label, lora)
        });
    }
}

/// LoRA status of the main model. Endpoint models can never take an adapter. Otherwise the capability the
/// learning loop recorded wins over the static architecture check. Admission alone never proves runtime use.
fn lora_state(
    config: &Config,
    store: Option<&Store>,
    active: Option<&crate::config::types::Version>,
) -> Lora {
    if config.endpoint.is_some() {
        return Lora::Unsupported;
    }
    if let Some(phase) = store.and_then(|store| crate::learn::improve::lora_phase(config, store)) {
        match phase.as_str() {
            "training" => return Lora::Training,
            "paused" => return Lora::Paused,
            "evaluating" => return Lora::Evaluating,
            _ => {}
        }
    }
    if let Some(version) = active.filter(|version| version.adapter.is_some()) {
        return Lora::Active(version.id);
    }
    let recorded = store
        .and_then(|store| store.get_meta("serving_capabilities").ok().flatten())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(crate::learn::worker::current_lora_probe)
        .and_then(|caps| caps["lora"].as_bool());
    let compatible = recorded
        .or_else(|| catalog::local_lora_support(&config.user_dir, Path::new(&config.model)));
    match compatible {
        Some(false) => Lora::Unsupported,
        None => Lora::Unknown,
        Some(true) if recorded != Some(true) => Lora::Unverified,
        Some(true) if !config.training.enabled || !config.improve.enabled => Lora::Off,
        Some(true) => match active.filter(|version| version.adapter.is_some()) {
            Some(version) => Lora::Active(version.id),
            None => Lora::Ready,
        },
    }
}

fn improvement_label(config: &Config, store: Option<&Store>) -> String {
    if !config.improve.enabled {
        return "learning off".into();
    }
    if store.is_some_and(|s| !auto::enabled(config, s).unwrap_or(false)) {
        return "learning paused".into();
    }
    let state = store
        .and_then(|store| store.get_meta("auto_status").ok().flatten())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    match state.as_deref() {
        Some("queued") => "improvement queued".into(),
        Some("running") => store
            .and_then(|store| store.get_meta("cycle_progress").ok().flatten())
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|value| {
                value["stage"]
                    .as_str()
                    .map(|stage| format!("improving: {}", agent::truncate(stage, 80)))
            })
            .unwrap_or_else(|| "improving".into()),
        Some("failed") => "learning failed".into(),
        Some("disabled") => "learning paused".into(),
        Some(_) => store
            .and_then(|store| auto::readiness(config, store).ok())
            .unwrap_or_else(|| "learning idle".into()),
        None if store.is_none() => "learning unavailable".into(),
        None => "learning idle".into(),
    }
}

/// `HH:MM`-style local time without a date library.
fn local_time(format: &str) -> String {
    let Ok(format) = std::ffi::CString::new(format) else {
        return String::new();
    };
    // SAFETY: `localtime_r` and `strftime` write only into the zeroed `tm` and the bounded buffer.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return String::new();
        }
        let mut buffer = [0u8; 64];
        let written = libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            format.as_ptr(),
            &tm,
        );
        String::from_utf8_lossy(&buffer[..written]).into_owned()
    }
}

/// Dark or light terminal background from `COLORFGBG` (`fg;bg`, bg index < 8 is dark); `None` = unknown.
fn terminal_dark(value: Option<&str>) -> Option<bool> {
    let background = value?.rsplit(';').next()?.trim().parse::<u32>().ok()?;
    Some(background < 8 || background == 16)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Name and one-line description of every built-in slash command, in the order the list shows them.
const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    (
        "palette",
        "search actions, sessions, models, tools and settings",
    ),
    ("compute", "open compute owners and background job controls"),
    (
        "changes",
        "list changed files and line counts for this workspace",
    ),
    ("help", "list commands and shortcuts"),
    ("model", "choose the main model and assign roles"),
    ("providers", "set up providers, keys and endpoints"),
    ("settings", "all options: appearance, tools, learning"),
    ("theme", "list or switch the color theme"),
    ("hotkeys", "view and edit keyboard shortcuts"),
    ("new", "start a fresh session"),
    ("sessions", "search and resume sessions"),
    ("resume", "search sessions or resume a unique id prefix"),
    ("fork", "copy this conversation into a new session"),
    ("rename", "name this session: /rename <title>"),
    ("export", "export conversation: /export [path] [--html]"),
    ("tree", "browse session forks"),
    ("copy", "copy the last assistant answer"),
    ("plan", "toggle read-only planning mode"),
    (
        "profile",
        "list/select capability profile: /profile [name|off]",
    ),
    (
        "reasoning",
        "show or set reasoning level: /reasoning [level]",
    ),
    ("thinking", "show or set thinking: /thinking [level|on|off]"),
    (
        "permissions",
        "list rules; allow/deny [--persist] <tool> [prefix]; remove <number>",
    ),
    ("cost", "show this session's API cost"),
    ("jobs", "show live subagents: /jobs [id]"),
    ("message", "steer a live subagent: /message <id> <text>"),
    ("cancel", "cancel a live subagent: /cancel <id>"),
    ("compact", "summarize older messages to free context"),
    ("undo", "drop the last turn and restore its file changes"),
    ("rollback", "restore files changed in the last n turns"),
    (
        "clear",
        "clear conversation context and start a fresh session",
    ),
    ("todo", "show the task list"),
    ("skills", "browse, import and enable/disable skills"),
    ("memory", "show project memory"),
    (
        "reliability",
        "show live tool success, correction attempts, capability results and verification",
    ),
    (
        "policy",
        "show live task phase, effective settings, selection reason and recent request metrics",
    ),
    (
        "steering",
        "inspect activation vector, evaluation, progress and confirmed runtime use",
    ),
    (
        "experience",
        "inspect learned recipes, failures and investigation: /experience [ledger]",
    ),
    (
        "understanding",
        "inspect saved knowledge: /understanding [architecture|build|query]",
    ),
    (
        "laya",
        "local decision advice: /laya [status|check|start|on|off]",
    ),
    ("commands", "list custom commands"),
    ("search", "search past sessions"),
    ("context", "show token usage"),
    ("approval", "ask, auto or readonly"),
    ("outcome", "record your verdict on the last answer (0-100)"),
    ("improve", "self-improvement status and job controls"),
    ("exit", "quit"),
];

/// Built-ins always win at dispatch, so a custom command with a built-in's name is never offered.
fn command_entries(custom: &[context::CommandDef], skills: &[skills::Skill]) -> Vec<CommandEntry> {
    let mut entries: Vec<CommandEntry> = BUILTIN_COMMANDS
        .iter()
        .map(|(name, description)| CommandEntry {
            name: (*name).into(),
            description: (*description).into(),
            kind: CommandKind::Builtin,
            args_hint: matches!(
                *name,
                "resume"
                    | "search"
                    | "rollback"
                    | "approval"
                    | "outcome"
                    | "theme"
                    | "rename"
                    | "message"
                    | "cancel"
            )
            .then(|| {
                match *name {
                    "resume" => "[id]",
                    "search" => "<query>",
                    "rename" => "<title>",
                    "message" => "<id> <text>",
                    "cancel" => "<id>",
                    "rollback" => "[n]",
                    "approval" => "[ask|auto|readonly]",
                    "outcome" => "<0-100> [note]",
                    _ => "[name]",
                }
                .to_owned()
            }),
        })
        .collect();
    let reserved = |name: &str| {
        BUILTIN_COMMANDS.iter().any(|(builtin, _)| *builtin == name)
            || matches!(name, "quit" | "q" | "?" | "models" | "roles")
    };
    for def in custom.iter().filter(|def| !reserved(&def.name)) {
        if entries.iter().any(|e| e.name == def.name) {
            continue;
        }
        entries.push(CommandEntry {
            name: def.name.clone(),
            description: if def.description.trim().is_empty() {
                "custom command".into()
            } else {
                def.description.clone()
            },
            kind: CommandKind::Custom,
            args_hint: None,
        });
    }
    for skill in skills {
        entries.push(CommandEntry {
            name: format!("skill:{}", skill.name),
            description: if skill.description.trim().is_empty() {
                "run this skill".into()
            } else {
                skill.description.clone()
            },
            kind: CommandKind::Skill,
            args_hint: Some("[task]".into()),
        });
    }
    entries
}

fn palette_entries(custom: &[context::CommandDef], skills: &[skills::Skill]) -> Vec<PaletteEntry> {
    command_entries(custom, skills)
        .into_iter()
        .map(|command| {
            let during_turn = if Shell::stops_turn(&command.name) {
                "Stops the current turn before running."
            } else if matches!(
                command.name.as_str(),
                "model" | "providers" | "settings" | "compute"
            ) {
                "Opens now; changes to model/config apply after this turn."
            } else if matches!(
                command.name.as_str(),
                "help"
                    | "hotkeys"
                    | "todo"
                    | "skills"
                    | "commands"
                    | "context"
                    | "clear"
                    | "policy"
                    | "reliability"
                    | "jobs"
                    | "message"
                    | "cancel"
                    | "search"
            ) {
                "Runs now without stopping the current turn."
            } else {
                "Runs after the current turn finishes."
            };
            PaletteEntry {
                command,
                during_turn,
            }
        })
        .collect()
}

/// Concrete argument choices, not usage placeholders. Keep these next to the command registry;
/// the composer only matches/renders continuations and never knows command semantics.
fn command_argument_completions(config: &Config, skills: &[skills::Skill]) -> Vec<String> {
    let mut candidates = Vec::new();
    for (name, args) in [
        ("understanding", &["architecture", "build"][..]),
        ("experience", &["ledger"]),
        ("laya", &["status", "check", "start", "on", "off"]),
        (
            "skills",
            &["list", "sources", "import", "enable", "disable"],
        ),
        ("skills import", &["--dry-run", "--user", "--update"]),
        ("approval", &["ask", "auto", "readonly"]),
        ("plan", &["on", "off"]),
        ("permissions", &["allow", "deny", "remove"]),
        ("permissions allow", &["--persist"]),
        ("permissions deny", &["--persist"]),
        (
            "improve",
            &[
                "status", "jobs", "stop", "cycle", "enable", "disable", "versions", "tasks",
            ],
        ),
        ("theme", &["auto"]),
        ("export", &["--html"]),
        ("profile", &["off"]),
    ] {
        candidates.extend(args.iter().map(|arg| format!("/{name} {arg}")));
    }
    for skill in skills {
        for action in ["enable", "disable"] {
            candidates.push(format!("/skills {action} {}", skill.name));
            candidates.push(format!("/skills {action} {} --user", skill.name));
        }
    }
    candidates.extend(
        config
            .model_profiles
            .keys()
            .map(|name| format!("/profile {name}")),
    );
    if let Ok(Some(profile)) = reasoning_profile(config) {
        for command in ["reasoning", "thinking"] {
            candidates.extend(
                profile
                    .capabilities
                    .reasoning_efforts
                    .iter()
                    .map(|effort| format!("/{command} {}", effort.as_str())),
            );
        }
    }
    candidates
}

// ---------------------------------------------------------------------------
// Configuration changes as data (pure, so they can be tested without a terminal)
// ---------------------------------------------------------------------------

/// The settings writes that make `selection` the model for `role` (`main` or a helper role).
fn selection_changes(
    config: &Config,
    selection: &ModelSelection,
    role: &str,
) -> Result<Vec<Change>> {
    ensure!(!selection.model.trim().is_empty(), "a model id is required");
    let mut changes = Vec::new();
    let set = |path: String, value: toml::Value| Change::Set { path, value };
    if role == "main" {
        changes.push(set("model".into(), selection.model.clone().into()));
        changes.push(set("provider".into(), selection.provider.clone().into()));
        if config.model_profile.as_ref().is_some_and(|name| {
            config.model_profiles.get(name).is_some_and(|profile| {
                profile.provider != selection.provider || profile.model != selection.model
            })
        }) {
            // An explicit empty marker clears a profile inherited from the other settings layer.
            changes.push(set("model_profile".into(), "".into()));
        }
        match &selection.base_url {
            Some(base) => {
                changes.push(set("endpoint.base_url".into(), base.clone().into()));
                // An empty variable name clears an inherited one (the loader treats it as unset).
                changes.push(set(
                    "endpoint.api_key_env".into(),
                    selection.api_key_env.clone().unwrap_or_default().into(),
                ));
                changes.push(set(
                    "endpoint.allow_insecure".into(),
                    selection.allow_insecure.into(),
                ));
                if !config.providers.contains_key(&selection.provider) {
                    let prefix = format!("providers.{}", selection.provider);
                    changes.push(set(
                        format!("{prefix}.label"),
                        selection.provider.clone().into(),
                    ));
                    changes.push(set(format!("{prefix}.base_url"), base.clone().into()));
                    if let Some(variable) = &selection.api_key_env {
                        changes.push(set(
                            format!("{prefix}.api_key_env"),
                            variable.clone().into(),
                        ));
                    }
                    changes.push(set(format!("{prefix}.enabled"), true.into()));
                    changes.push(set(
                        format!("{prefix}.allow_insecure"),
                        selection.allow_insecure.into(),
                    ));
                }
            }
            None => changes.push(Change::Remove {
                path: "endpoint".into(),
            }),
        }
        // Capacity is metadata, not a user override. An explicit Auto marker also clears
        // a numeric cap inherited from the other settings layer when switching models.
        changes.push(set("context_window".into(), "auto".into()));
    } else {
        ensure!(
            ROLES.iter().any(|(name, _)| *name == role) && role != "main",
            "unknown model role {role:?}"
        );
        let prefix = format!("model_roles.{role}");
        changes.push(set(
            format!("{prefix}.provider"),
            selection.provider.clone().into(),
        ));
        changes.push(set(
            format!("{prefix}.model"),
            selection.model.clone().into(),
        ));
        match &selection.base_url {
            Some(base) => {
                changes.push(set(format!("{prefix}.base_url"), base.clone().into()));
                changes.push(set(
                    format!("{prefix}.api_key_env"),
                    selection.api_key_env.clone().unwrap_or_default().into(),
                ));
                changes.push(set(
                    format!("{prefix}.allow_insecure"),
                    selection.allow_insecure.into(),
                ));
            }
            None => changes.push(Change::Remove {
                path: format!("{prefix}.base_url"),
            }),
        }
        changes.push(set(format!("{prefix}.context_window"), "auto".into()));
    }
    Ok(changes)
}

/// Make `role` follow the main model again. In the project layer an explicit marker is needed so a
/// global assignment does not show through.
fn clear_role_changes(role: &str, scope: Scope) -> Result<Vec<Change>> {
    ensure!(
        ROLES.iter().any(|(name, _)| *name == role) && role != "main",
        "the main model cannot be cleared"
    );
    let mut changes = match scope {
        Scope::Global => vec![Change::Remove {
            path: format!("model_roles.{role}"),
        }],
        Scope::Project => vec![
            Change::Set {
                path: format!("model_roles.{role}.provider"),
                value: "inherit".into(),
            },
            Change::Set {
                path: format!("model_roles.{role}.model"),
                value: "".into(),
            },
        ],
    };
    if role == "architecture" {
        changes.push(Change::Set {
            path: "codebase.architecture_model".into(),
            value: "".into(),
        });
    }
    Ok(changes)
}

fn reasoning_profile(config: &Config) -> Result<Option<crate::llm::profiles::ModelProfile>> {
    if let Some(profile) = config.active_model_profile() {
        return Ok(Some(profile.clone()));
    }
    if config.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER) {
        return crate::llm::chatgpt::model_profile(&config.user_dir, &config.model);
    }
    Ok(None)
}

fn thinking_label(profile: &crate::llm::profiles::ModelProfile) -> String {
    use crate::llm::profiles::{ReasoningApi, ReasoningEffort};
    if profile.capabilities.reasoning == ReasoningApi::None {
        return "unsupported".into();
    }
    if profile.thinking == Some(false) || profile.reasoning_effort == Some(ReasoningEffort::None) {
        return "off".into();
    }
    if let Some(effort) = profile.reasoning_effort {
        return effort.as_str().into();
    }
    if profile.thinking == Some(true) {
        return "on".into();
    }
    "default".into()
}

fn reasoning_changes(config: &Config, args: &str) -> Result<(Vec<Change>, String)> {
    use crate::llm::profiles::{ReasoningApi, ReasoningEffort};
    let mut profile=reasoning_profile(config)?.ok_or_else(||anyhow::anyhow!("reasoning metadata is unavailable; open /models and rescan, or select an explicit /profile"))?;
    let supported = profile
        .capabilities
        .reasoning_efforts
        .iter()
        .map(|effort| effort.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    let label = if matches!(
        profile.capabilities.reasoning,
        ReasoningApi::AnthropicBudget | ReasoningApi::AnthropicAdaptive
    ) && matches!(args, "on" | "off")
    {
        profile.thinking = Some(args == "on");
        args.into()
    } else {
        let effort = match args {
            "on" => profile
                .reasoning_effort
                .filter(|effort| *effort != ReasoningEffort::None)
                .or_else(|| {
                    profile
                        .capabilities
                        .reasoning_efforts
                        .iter()
                        .copied()
                        .find(|effort| *effort != ReasoningEffort::None)
                }),
            "off" => Some(ReasoningEffort::None),
            level => serde_json::from_value::<ReasoningEffort>(serde_json::json!(level)).ok(),
        }
        .filter(|effort| profile.capabilities.reasoning_efforts.contains(effort))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "supported reasoning levels for {}: {supported}",
                config.model
            )
        })?;
        profile.reasoning_effort = Some(effort);
        profile.thinking = Some(effort != ReasoningEffort::None);
        effort.as_str().to_owned()
    };
    profile.validate()?;
    let name = config
        .model_profile
        .clone()
        .or_else(|| {
            config.active_model_profile().and_then(|_| {
                config
                    .model_profiles
                    .iter()
                    .find(|(_, saved)| {
                        saved.provider == profile.provider && saved.model == profile.model
                    })
                    .map(|(name, _)| name.clone())
            })
        })
        .unwrap_or_else(|| {
            use sha2::{Digest, Sha256};
            format!(
                "reasoning_{}",
                &format!(
                    "{:x}",
                    Sha256::digest(format!("{}/{}", profile.provider, profile.model))
                )[..16]
            )
        });
    Ok((
        vec![
            Change::Set {
                path: format!("model_profiles.{name}"),
                value: toml::Value::try_from(profile)?,
            },
            Change::Set {
                path: "model_profile".into(),
                value: name.into(),
            },
        ],
        label,
    ))
}

fn provider_changes(id: &str, spec: &ProviderSpec) -> Vec<Change> {
    let set = |field: &str, value: toml::Value| Change::Set {
        path: format!("providers.{id}.{field}"),
        value,
    };
    let mut changes = vec![
        set("label", spec.label.clone().into()),
        set("base_url", spec.base_url.clone().into()),
        set("enabled", spec.enabled.into()),
        set("allow_insecure", spec.allow_insecure.into()),
    ];
    changes.push(match &spec.api_key_env {
        Some(variable) => set("api_key_env", variable.clone().into()),
        None => Change::Remove {
            path: format!("providers.{id}.api_key_env"),
        },
    });
    changes
}

// ---------------------------------------------------------------------------
// The shell: everything visible and interactive
// ---------------------------------------------------------------------------

struct Shell {
    runtime: crate::runtime::Tree,
    pending_result: Option<crate::runtime::Body>,
    shared_prose: bool,
    auth_job: Option<AuthJob>,
    term: InlineTerminal,
    themes: ThemeManager,
    keys: Keymap,
    composer: Composer,
    paste_guard: crate::ui::input::PasteGuard,
    transcript: Transcript,
    status: StatusData,
    ui: UiConfig,
    catalog: Catalog,
    git: Git,
    learning: Learning,
    yield_compute: bool,
    adapter_monitor: Option<crate::llm::provider::AdapterMonitor>,
    request_policy: Value,
    commands: Vec<context::CommandDef>,
    scope: Scope,
    tools_expanded: bool,
    started: Instant,
    last_trajectory: Option<i64>,
    startup: Option<String>,
    /// The configured approval mode last applied to the agent (session cycling may differ from it).
    cfg_approval: Approval,
    dark: Option<bool>,
    session_id: String,
    title: String,
    turn_output_tokens: u64,
    /// The agent's task list as last reported; drawn above the composer while any task is open.
    todo: TodoList,
    /// Skills found at the last command refresh, so `/help` and `/skills` work while a turn runs.
    skills: Vec<skills::Skill>,
    skills_listing: String,
    /// Commands typed during a turn that need the agent but are not meant to stop it; they run, in order,
    /// as soon as the turn finishes.
    deferred: std::collections::VecDeque<String>,
    /// Handle to the agent's live jobs, usable while a turn holds the agent.
    live_tasks: Option<crate::agent::LiveTaskControl>,
    pending_config: Option<Config>,
    pending_verification: Option<ModelSelection>,
}

enum Which {
    Palette,
    Compute,
    Settings,
    Models,
    Providers,
    Shortcuts,
}

#[derive(Default)]
struct OverlayOutcome {
    verify_lora: Option<ModelSelection>,
    /// The effective config after the last successful change made inside the overlay.
    latest: Option<Config>,
    /// A request from inside the overlay to open another one (`open:models`, `open:providers`, …).
    next: Option<String>,
}

impl Shell {
    fn new(app: &App, startup: String) -> Result<Shell> {
        let mut term = InlineTerminal::enter()?;
        term.clear_screen()?;
        let dark = terminal_dark(std::env::var("COLORFGBG").ok().as_deref());
        let (keys, warnings) = Keymap::load(&app.layout.user_dir);
        let themes = ThemeManager::new(&app.layout.user_dir, &app.config.ui, dark);
        let history = settings_store::load_history(&app.layout, app.config.ui.history_limit)
            .unwrap_or_default();
        let commands = context::discover_commands(&app.layout);
        let mut shell = Shell {
            runtime: Default::default(),
            pending_result: None,
            shared_prose: false,
            auth_job: None,
            term,
            themes,
            keys,
            composer: Composer::new(history, app.config.ui.history_limit),
            paste_guard: Default::default(),
            transcript: Transcript::new(),
            status: StatusData::default(),
            ui: app.config.ui.clone(),
            catalog: Catalog::new(),
            git: Git::default(),
            learning: Learning::default(),
            yield_compute: false,
            adapter_monitor: None,
            request_policy: Value::Null,
            commands,
            scope: Scope::Global,
            tools_expanded: false,
            started: Instant::now(),
            last_trajectory: None,
            startup: (!startup.trim().is_empty()).then_some(startup),
            cfg_approval: app.config.approval,
            dark,
            session_id: String::new(),
            title: format!(
                "dial · {}",
                app.layout
                    .project_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            turn_output_tokens: 0,
            todo: TodoList::new(),
            skills: Vec::new(),
            deferred: Default::default(),
            live_tasks: None,
            skills_listing: String::new(),
            pending_config: None,
            pending_verification: None,
        };
        for warning in warnings {
            shell.note("Key bindings", &warning);
        }
        if let Some(error) = shell.themes.last_error() {
            let error = error.to_owned();
            shell.note("Theme", &error);
        }
        if let Some(error) = &app.store_error {
            shell.note("Learning", error);
        }
        shell.refresh_commands(app);
        Ok(shell)
    }

    fn refresh_commands(&mut self, app: &App) {
        let found = skills::discover(&app.layout);
        self.composer
            .set_commands(command_entries(&self.commands, &found));
        let mut arguments =
            command_argument_completions(&app.config, &skills::discover_all(&app.layout));
        arguments.extend(
            self.themes
                .names()
                .into_iter()
                .filter(|entry| entry.error.is_none())
                .map(|entry| format!("/theme {}", entry.name)),
        );
        self.composer.set_argument_completions(arguments);
        self.skills = found;
        self.skills_listing =
            skills::management::list(&app.layout).unwrap_or_else(|error| format!("{error:#}"));
        let paths = ignore::WalkBuilder::new(&app.layout.project_root)
            .follow_links(false)
            .require_git(false)
            .build()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter_map(|entry| {
                entry
                    .path()
                    .strip_prefix(&app.layout.project_root)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .filter(|path| !path.chars().any(|c| c.is_control() || c.is_whitespace()))
            .collect();
        self.composer.set_files(paths);
    }

    /// Welcome banner and, for a resumed session, its earlier conversation.
    fn begin(&mut self, app: &App, session: &Session, resumed: bool) {
        self.session_id = session.id().to_owned();
        let (cols, _) = self.term.size();
        if self.ui.welcome {
            let recent: Vec<(String, String)> = Session::list(&app.layout, 3)
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.id != session.id())
                .map(|s| (s.title, s.started.chars().take(10).collect()))
                .collect();
            let tips = if self.ui.tips {
                vec![
                    (
                        self.keys.label(Action::CommandPalette),
                        "actions".to_owned(),
                    ),
                    ("/".to_owned(), "commands".to_owned()),
                    ("@".to_owned(), "files".to_owned()),
                    ("!".to_owned(), "shell".to_owned()),
                    (self.keys.label(Action::ModelSelect), "model".to_owned()),
                ]
                .into_iter()
                .filter(|(key, _)| !key.is_empty())
                .collect()
            } else {
                Vec::new()
            };
            let selection = app.config.selection();
            let info = WelcomeInfo {
                version: env!("CARGO_PKG_VERSION").into(),
                provider: selection.provider.clone(),
                model: selection
                    .display_name()
                    .trim_start_matches(&format!("{}/", selection.provider))
                    .to_owned(),
                project: {
                    let root = app.layout.project_root.display().to_string();
                    match std::env::var("HOME") {
                        Ok(home)
                            if !home.is_empty()
                                && (root == home || root.starts_with(&format!("{home}/"))) =>
                        {
                            format!("~{}", &root[home.len()..])
                        }
                        _ => root,
                    }
                },
                tips,
                recent,
            };
            let rows = welcome::rows(&info, cols, self.themes.current());
            self.term.commit(rows);
        }
        if resumed {
            replay(&mut self.transcript, &session.messages());
        }
    }

    fn after_adopt(&mut self, app: &App) {
        self.ui = app.config.ui.clone();
        self.cfg_approval = app.config.approval;
        self.catalog.refresh(app);
        self.refresh_commands(app);
    }

    fn finish(&mut self) -> Option<String> {
        self.auth_job.take();
        // Whatever is still pending belongs in the scrollback before the live region is erased.
        let _ = self.flush();
        self.term.leave();
        (!self.session_id.is_empty()).then(|| self.session_id.clone())
    }

    /// Commit everything finalized so the conversation survives in scrollback.
    fn flush(&mut self) -> Result<()> {
        self.transcript.finish_turn();
        self.paint(None)
    }

    // ----- painting -------------------------------------------------------------------------

    fn paint(&mut self, panel: Option<Rows>) -> Result<()> {
        let files = if self.ui.session_rail {
            self.git.files(Path::new(&self.status.project_path))
        } else {
            Vec::new()
        };
        let workers = if self.ui.worker_shards {
            let mut bodies = self.runtime.bodies("worker");
            bodies.retain(|b| {
                self.status.phase == Phase::Working
                    || b.read(|n| n.result.phase == crate::runtime::Phase::Running)
            });
            bodies.sort_by_key(|b| {
                b.read(|n| {
                    (
                        n.result.phase != crate::runtime::Phase::Running,
                        std::cmp::Reverse(
                            n.id.strip_prefix("runtime-")
                                .and_then(|n| n.parse::<u64>().ok())
                                .unwrap_or(0),
                        ),
                    )
                })
            });
            bodies
                .iter()
                .take(5)
                .map(|b| {
                    b.read(|n| crate::runtime::Node {
                        id: n.id.clone(),
                        parent: n.parent.clone(),
                        kind: n.kind.clone(),
                        label: n.label.clone(),
                        result: crate::runtime::ResultBody {
                            text: n
                                .result
                                .text
                                .lines()
                                .rev()
                                .find(|l| !l.trim().is_empty())
                                .map(|l| l.chars().take(200).collect())
                                .unwrap_or_default(),
                            data: n.result.data.as_ref().map(
                                |d| serde_json::json!({"phase":d["phase"],"steps":d["steps"]}),
                            ),
                            phase: n.result.phase,
                            revision: n.result.revision,
                        },
                    })
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        self.status.tick = (self.started.elapsed().as_millis() / 83) as u64;
        self.status.clock = local_time("%H:%M");
        self.status.mode = self.composer.mode();
        let (cols, rows) = self.term.size();
        let Shell {
            term,
            themes,
            keys,
            composer,
            transcript,
            status,
            ui,
            tools_expanded,
            title,
            todo,
            ..
        } = self;
        let theme = themes.current();
        let tctx = TranscriptCtx {
            theme,
            ui,
            tick: status.tick,
            tools_expanded: *tools_expanded,
        };
        let (settled, images) = transcript.take_settled_with_images(cols, &tctx);
        if !settled.is_empty() {
            if let Some(protocol) = term.image_protocol() {
                term.commit_with_images(settled, images, protocol);
            } else {
                term.commit(settled);
            }
        }
        let cctx = ComposerCtx {
            theme,
            keys,
            ui,
            status,
        };
        let (bottom, cursor) = match panel {
            Some(rows) => (rows, None),
            None => {
                let view = composer.view(cols, &cctx);
                let spinner = (status.phase == Phase::Working).then(|| theme.spinner(status.tick));
                let cap = (rows as usize / 3).min(10);
                let available = (rows as usize).saturating_sub(view.rows.len() + 3);
                let mut bottom = if ui.session_rail && available > 0 {
                    crate::ui::rail::session(
                        &self.session_id,
                        title,
                        &files,
                        cols as usize,
                        (rows as usize / 8).clamp(1, 4).min(available),
                        theme,
                    )
                } else {
                    Vec::new()
                };
                let worker_cap = (rows as usize / 5)
                    .clamp(2, 6)
                    .min(available.saturating_sub(bottom.len()));
                if ui.worker_shards {
                    bottom.extend(crate::ui::rail::workers(
                        &workers,
                        cols as usize,
                        worker_cap,
                        status.tick,
                        theme,
                    ));
                }
                bottom.extend(tasks::rows(
                    todo,
                    cols as usize,
                    theme,
                    spinner,
                    cap.min(available.saturating_sub(bottom.len())),
                ));
                let above = bottom.len() as u16;
                bottom.extend(view.rows);
                (bottom, Some((view.cursor.0 + above, view.cursor.1)))
            }
        };
        let room = (rows as usize).saturating_sub(bottom.len()).max(1);
        let (spilled, mut live) = transcript.live(cols, &tctx, room);
        if !spilled.is_empty() {
            term.commit(spilled);
        }
        let above = live.len() as u16;
        live.extend(bottom);
        term.paint(&Viewport {
            rows: &live,
            cursor: cursor.map(|(row, col)| (above + row, col)),
            title: Some(title.as_str()),
        })
    }

    fn refresh_status(&mut self, app: &App, agent: &Agent<'_>) {
        self.adapter_monitor = agent.adapter_monitor();
        self.request_policy = agent.session().control_state();
        let selection = app.config.selection();
        let (branch, counts) = self.git.read(&app.layout.project_root);
        self.learning.read(app);
        let status = &mut self.status;
        status.provider = selection.provider.clone();
        status.model = selection
            .display_name()
            .trim_start_matches(&format!("{}/", selection.provider))
            .to_owned();
        status.thinking = reasoning_profile(&app.config)
            .ok()
            .flatten()
            .as_ref()
            .map(thinking_label)
            .unwrap_or_default();
        status.project_path = app.layout.project_root.display().to_string();
        if agent.plan_mode() {
            status.model.push_str(" [plan]");
        }
        status.branch = branch;
        status.git_counts = counts;
        status.session = short(agent.session().id());
        let (tokens, estimated) = agent.context_usage();
        status.prompt_tokens = (tokens > 0).then_some(tokens);
        status.context_estimated = estimated;
        status.context_window = agent.context_window();
        status.approval = format!("{:?}", agent.approval()).to_lowercase();
        status.sandbox = format!("{:?}", app.config.sandbox.mode).to_lowercase();
        status.version = self.learning.cache.value.0;
        status.learning = self.learning.cache.value.1.clone();
        status.lora = self.learning.cache.value.2;
        if let Some(monitor) = &self.adapter_monitor {
            status.lora = status.lora.with_runtime(&monitor.lock());
        }
        status.role.clear();
        if self.todo != *agent.todo() {
            self.todo = agent.todo().clone();
        }
    }

    // ----- idle loop ----------------------------------------------------------------------

    fn run(&mut self, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
        let mut dirty = true;
        let mut previous_status = None;
        loop {
            if let Some(config) = self.pending_config.take() {
                if let step @ Step::Rebuild(_) = self.config_changed(app, agent, config) {
                    return Ok(step);
                }
                dirty = true;
            }
            if let Some(selection) = self.pending_verification.take() {
                self.verify_lora(app, agent, &selection)?;
                dirty = true;
            }
            if let Some((provider, result)) = AuthJob::collect(&mut self.auth_job) {
                self.note(
                    provider,
                    match result {
                        Ok(message) => message,
                        Err(error) => format!("{error:#}"),
                    },
                );
                self.catalog.refresh(app);
                dirty = true;
            }
            {
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                agent.pump_subagents(&mut bridge);
                if bridge.failed {
                    anyhow::bail!("terminal draw failed");
                }
            }
            if self.startup.is_none() {
                self.startup = self
                    .deferred
                    .pop_front()
                    .or_else(|| self.composer.take_follow_up());
            }
            let handoff = self.yield_compute
                || ((crate::learn::compute::background_pending(&app.layout.user_dir)?
                    || crate::learn::compute::foreground_pending(&app.layout.user_dir)?)
                    && app.compute.borrow().is_some());
            // Free resident models while waiting for input, including remote chats with local
            // role models. Running child agents keep their models and compute lease.
            if self.startup.is_none() && app.release_idle_compute(agent) {
                if handoff {
                    self.yield_compute = false;
                    self.note("Compute", "Local models unloaded for queued work. This chat stays open; the next turn reloads the model.");
                    dirty = true;
                }
            }
            self.refresh_status(app, agent);
            if self.themes.reload_if_changed() {
                self.after_theme_reload();
                dirty = true;
            }
            self.status.phase = Phase::Idle;
            self.status.model_progress = None;
            self.status.model_silence_secs = 0;
            self.status.clock = local_time("%H:%M");
            let status = statusline::preview_row(
                &self.ui,
                &self.status,
                self.themes.current(),
                self.term.size().0,
            );
            if dirty || previous_status.as_ref() != Some(&status) {
                self.paint(None)?;
            }
            previous_status = Some(status);
            dirty = false;
            if let Some(text) = self.startup.take() {
                dirty = true;
                match self.submit(app, agent, text)? {
                    Step::Done => continue,
                    other => return Ok(other),
                }
            }
            let (left, right) = statusline::segments(&self.ui);
            let status_pending = self.git.cache.worker.is_some()
                || self.learning.cache.worker.is_some()
                || self.auth_job.is_some()
                || self.catalog.worker.is_some();
            let timeout = if status_pending {
                TICK
            } else if left.iter().chain(&right).any(|s| s == "time") {
                Duration::from_secs(1)
            } else {
                STATUS_TTL
            };
            if !poll(timeout)? {
                continue;
            }
            dirty = true;
            match read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let event = self.composer_key(key);
                    let step = match event {
                        ComposerEvent::None | ComposerEvent::Interrupt => Step::Done,
                        ComposerEvent::Exit => Step::Quit,
                        ComposerEvent::Submit(text) => self.submit(app, agent, text)?,
                        ComposerEvent::Action(action) => self.on_action(action, app, agent)?,
                    };
                    if !matches!(step, Step::Done) {
                        return Ok(step);
                    }
                }
                Event::Paste(text) => self.composer_paste(&text),
                Event::Resize(..) => self.term.on_resize(),
                _ => {}
            }
        }
    }

    fn composer_key(&mut self, key: crossterm::event::KeyEvent) -> ComposerEvent {
        let buffered = poll(Duration::ZERO).unwrap_or(false);
        if let Some(text) = self.paste_guard.text_for_key(key, buffered, Instant::now()) {
            self.composer.handle_paste(text);
            return ComposerEvent::None;
        }
        let ctx = ComposerCtx {
            theme: self.themes.current(),
            keys: &self.keys,
            ui: &self.ui,
            status: &self.status,
        };
        self.composer.handle_key(key, &ctx)
    }

    fn composer_paste(&mut self, text: &str) {
        self.paste_guard.pasted(Instant::now());
        self.composer.handle_paste(text);
    }

    fn after_theme_reload(&mut self) {
        if let Some(error) = self.themes.last_error() {
            let error = error.to_owned();
            self.note("Theme", &error);
        }
    }

    fn save_history(&self, app: &App) {
        let _ = settings_store::save_history(
            &app.layout,
            &self.composer.history(),
            self.ui.history_limit,
        );
    }

    fn on_action(&mut self, action: Action, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
        match action {
            Action::CommandPalette => self.open(app, agent, Which::Palette),
            Action::ModelSelect | Action::ModelRoles => self.open(app, agent, Which::Models),
            Action::ProviderSetup => self.open(app, agent, Which::Providers),
            Action::Settings => self.open(app, agent, Which::Settings),
            Action::ToolsExpand => {
                self.tools_expanded = !self.tools_expanded;
                self.note(
                    "Tools",
                    if self.tools_expanded {
                        "tool output expanded for new output (clear repaints history)"
                    } else {
                        "tool output collapsed"
                    },
                );
                Ok(Step::Done)
            }
            Action::ToolsToggle => {
                self.ui.show_tools = !self.ui.show_tools;
                self.note(
                    "Tools",
                    if self.ui.show_tools {
                        "tool activity shown"
                    } else {
                        "tool activity hidden (errors still show)"
                    },
                );
                Ok(Step::Done)
            }
            Action::ThinkingToggle => {
                self.ui.show_thinking = !self.ui.show_thinking;
                self.note(
                    "Reasoning",
                    if self.ui.show_thinking {
                        "reasoning text shown"
                    } else {
                        "reasoning text collapsed"
                    },
                );
                Ok(Step::Done)
            }
            Action::ApprovalCycle => {
                let next = match agent.approval() {
                    Approval::Ask => Approval::Auto,
                    Approval::Auto => Approval::Readonly,
                    Approval::Readonly => Approval::Ask,
                };
                agent.set_approval(next);
                self.note("Approval", format!("{next:?}").to_lowercase());
                Ok(Step::Done)
            }
            Action::DisplayReset => {
                self.display_reset();
                Ok(Step::Done)
            }
            Action::EditExternal => {
                self.edit_externally();
                Ok(Step::Done)
            }
            _ => Ok(Step::Done),
        }
    }

    /// Erase the screen and scrollback, then repaint the whole conversation from the transcript.
    fn display_reset(&mut self) {
        if self.term.clear_screen().is_err() {
            return;
        }
        let (cols, _) = self.term.size();
        let Shell {
            term,
            themes,
            transcript,
            ui,
            status,
            tools_expanded,
            ..
        } = self;
        let ctx = TranscriptCtx {
            theme: themes.current(),
            ui,
            tick: status.tick,
            tools_expanded: *tools_expanded,
        };
        // Only what already reached scrollback is replayed; the still-streaming tail stays live.
        let rows = transcript.committed_rows(cols, &ctx);
        term.commit(rows);
    }

    /// Edit the draft in `$VISUAL`/`$EDITOR` on the real terminal.
    fn edit_externally(&mut self) {
        let editor = std::env::var("VISUAL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::env::var("EDITOR")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            });
        let Some(editor) = editor else {
            self.note(
                "Editor",
                "set $VISUAL or $EDITOR to edit the draft externally",
            );
            return;
        };
        let path = std::env::temp_dir().join(format!(
            "dial-draft-{}-{}.md",
            std::process::id(),
            self.started.elapsed().as_nanos()
        ));
        if std::fs::write(&path, self.composer.draft()).is_err() {
            return;
        }
        let ran = self.term.suspend(|| {
            // The editor setting may carry arguments (`code --wait`), so it goes through the shell.
            Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{editor} \"$1\""))
                .arg("dial-editor")
                .arg(&path)
                .status()
        });
        match ran {
            Ok(Ok(status)) if status.success() => {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    self.composer.set_draft(text.trim_end_matches('\n'));
                }
            }
            Ok(Ok(status)) => self.note(
                "Editor",
                format!("editor exited with {status}; draft unchanged"),
            ),
            Ok(Err(error)) => self.note("Editor", format!("could not start the editor: {error}")),
            Err(error) => self.note("Editor", format!("terminal error: {error:#}")),
        }
        let _ = std::fs::remove_file(&path);
    }

    // ----- submitting -----------------------------------------------------------------------

    fn submit(&mut self, app: &App, agent: &mut Agent<'_>, text: String) -> Result<Step> {
        let trimmed = text.trim().to_owned();
        if trimmed.is_empty() {
            return Ok(Step::Done);
        }
        self.save_history(app);
        if trimmed.starts_with('/') {
            return self.command(app, agent, &trimmed);
        }
        let (shell, include) = if let Some(command) = trimmed.strip_prefix("!!") {
            (Some(command), false)
        } else {
            (trimmed.strip_prefix('!'), true)
        };
        if let Some(command) = shell.map(str::trim).filter(|c| !c.is_empty()) {
            return self.run_user_shell(app, agent, command, include);
        }
        self.run_turn(app, agent, &trimmed)
    }

    /// `!cmd`: the user's own shell command, run in the project under the configured sandbox. Its output
    /// joins the conversation so the model can refer to it.
    fn run_user_shell(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        command: &str,
        include: bool,
    ) -> Result<Step> {
        use crate::tools::{self, Workspace};
        let allowed = {
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started: Instant::now(),
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            agent.approve_user_shell(command, &mut bridge)
        };
        if !allowed {
            self.note("Shell", "command denied by approval policy");
            return Ok(Step::Done);
        }
        self.transcript
            .push_user(&format!("{}{command}", if include { "!" } else { "!!" }));
        let started = Instant::now();
        app.layout.ensure_state_outside_project()?;
        let ws = Workspace::new(&app.layout.project_root)
            .and_then(|ws| {
                tools::apply_sandbox_config(
                    ws,
                    &app.config.sandbox,
                    Some((
                        app.layout.state_dir.as_path(),
                        app.layout.user_dir.as_path(),
                    )),
                )
            })
            .and_then(|ws| {
                Ok(ws.with_artifacts(tools::artifact::ArtifactStore::new(
                    &app.layout.state_dir.join("artifacts"),
                )?))
            });
        let output = match ws {
            Ok(ws) => {
                app.interrupt.store(false, Ordering::SeqCst);
                self.composer.set_busy(true);
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started,
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                bridge.tool_call("bash", command);
                let output = std::thread::scope(|scope| {
                    let (tx, rx) = std::sync::mpsc::sync_channel(16);
                    let cancel = app.interrupt.clone();
                    scope.spawn(move || {
                        let on_output = |chunk: &[u8]| {
                            let _ = tx.send(Ok(chunk.to_vec()));
                        };
                        let result = tools::shell::bash_tool_streaming(
                            &ws,
                            &json!({"command":command}),
                            120,
                            &on_output,
                            &cancel,
                        );
                        let output = match result {
                            Ok(output) => output,
                            Err(text) => ToolOutput {
                                data: None,
                                text,
                                is_error: true,
                            },
                        };
                        let _ = tx.send(Err(output));
                    });
                    loop {
                        match rx.recv_timeout(Duration::from_millis(80)) {
                            Ok(Ok(chunk)) => {
                                bridge.tool_output("bash", &String::from_utf8_lossy(&chunk))
                            }
                            Ok(Err(output)) => break output,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => bridge.heartbeat(),
                            Err(_) => {
                                break ToolOutput {
                                    data: None,
                                    text: "shell worker disconnected".into(),
                                    is_error: true,
                                };
                            }
                        }
                    }
                });
                bridge.tool_result("bash", &output);
                bridge.shell.composer.set_busy(false);
                output
            }
            Err(error) => ToolOutput {
                data: None,
                text: format!("{error:#}"),
                is_error: true,
            },
        };
        self.transcript.finish_turn();
        if include {
            agent.session_mut().append(json!({"role": "user", "content": format!("I ran `{command}` myself:\n{}", agent::guard::clip_middle(&output.text, 8000))}))?;
        }
        Ok(Step::Done)
    }

    fn run_turn(&mut self, app: &App, agent: &mut Agent<'_>, text: &str) -> Result<Step> {
        repl::clear_interrupt();
        self.live_tasks = Some(agent.live_task_control());
        let started = Instant::now();
        self.status.elapsed_secs = 0.0;
        self.transcript.push_user(text);
        self.composer.set_busy(true);
        self.status.phase = Phase::Working;
        let ready = self.wait_compute(app).and_then(|ready| {
            if ready {
                self.prepare_local(app)
            } else {
                Ok(false)
            }
        });
        match ready {
            Ok(true) => {}
            Ok(false) => {
                self.composer.set_busy(false);
                return Ok(Step::Done);
            }
            Err(error) => {
                self.transcript.error(&format!("{error:#}"));
                self.composer.set_busy(false);
                return Ok(Step::Done);
            }
        }
        self.status.phase = Phase::Working;
        self.turn_output_tokens = 0;
        self.status.output_tokens = 0;
        self.status.tokens_per_second = None;
        self.status.activity = "thinking".into();
        let (result, failed, streamed_tokens) = {
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started,
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            bridge.repaint(true);
            let result = agent.turn(text, &mut bridge);
            (result, bridge.failed, bridge.shell.turn_output_tokens)
        };
        app.interrupt.store(false, Ordering::SeqCst);
        self.composer.set_busy(false);
        self.save_history(app);
        self.status.activity.clear();
        self.status.model_progress = None;
        self.status.model_silence_secs = 0;
        let elapsed = started.elapsed().as_secs_f64();
        self.refresh_status(app, agent);
        match &result {
            Ok(report) => {
                if report.interrupted {
                    self.note("Interrupted", "the turn stopped at the next safe point");
                }
                if report.trajectory.is_some() {
                    self.last_trajectory = report.trajectory;
                }
            }
            Err(error) => self.transcript.error(&format!("{error:#}")),
        }
        self.transcript.finish_turn();
        if self.ui.show_turn_stats && result.is_ok() {
            self.transcript.turn_stats(TurnStats {
                clock: local_time("%Y-%m-%d %H:%M:%S"),
                elapsed_secs: elapsed,
                prompt_tokens: self.status.prompt_tokens,
                output_tokens: (streamed_tokens > 0).then_some(streamed_tokens),
                tokens_per_second: self.status.tokens_per_second,
            });
        }
        self.schedule_improvement(app);
        if failed {
            anyhow::bail!("terminal draw failed; the screen was restored");
        }
        Ok(Step::Done)
    }

    fn schedule_improvement(&mut self, app: &App) {
        let Some(store) = &app.store else { return };
        match auto::maybe_spawn(&app.layout, &app.config, store) {
            Ok(true) => {
                self.yield_compute = app.config.uses_local_compute();
                self.note("Improvement", if self.yield_compute {
                    "queued; this session will release compute once its running child agents finish"
                } else { "queued in the background; this session keeps the admitted version" });
            }
            Ok(false) => {}
            Err(error) => self.note("Improvement", format!("could not schedule: {error:#}")),
        }
    }

    // ----- asking the user ------------------------------------------------------------------

    /// A modal choice drawn in place of the composer. `None` means the user cancelled.
    fn ask(&mut self, title: &str, body: &str, options: &[(&str, char)]) -> Option<usize> {
        let mut selected = 0usize;
        loop {
            let (cols, _) = self.term.size();
            let rows = panel_rows(self.themes.current(), cols, title, body, options, selected);
            if self.paint(Some(rows)).is_err() {
                return None;
            }
            if !poll(TICK).unwrap_or(false) {
                continue;
            }
            match read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        KeyCode::Up => selected = (selected + options.len() - 1) % options.len(),
                        KeyCode::Down | KeyCode::Tab => selected = (selected + 1) % options.len(),
                        KeyCode::Enter => return Some(selected),
                        KeyCode::Esc => return None,
                        KeyCode::Char('c') if ctrl => return None,
                        KeyCode::Char(c) if !ctrl => {
                            if let Some(index) = options
                                .iter()
                                .position(|(_, hotkey)| hotkey.eq_ignore_ascii_case(&c))
                            {
                                return Some(index);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Resize(..)) => self.term.on_resize(),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    fn wait_compute(&mut self, app: &App) -> Result<bool> {
        if !app.config.uses_local_compute() || app.compute.borrow().is_some() {
            return Ok(true);
        }
        let _request = crate::learn::compute::Request::foreground(&app.layout.user_dir)?;
        let lock = auto::lock_file(&app.layout.user_dir.join("compute.lock"))?;
        let waiting_since = Instant::now();
        let elapsed_before_wait = self.status.elapsed_secs;
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => {
                    crate::learn::compute::register(
                        &app.layout.user_dir,
                        "session",
                        &app.layout.project_root,
                        &app.config.state_dir,
                        &app.config.model,
                    )?;
                    *app.compute.borrow_mut() = Some(lock);
                    return Ok(true);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            self.status.phase = Phase::WaitingCompute;
            self.status.elapsed_secs = elapsed_before_wait + waiting_since.elapsed().as_secs_f64();
            self.status.activity = "waiting for compute · Ctrl+, or /compute opens controls".into();
            self.paint(None)?;
            if poll(TICK)? {
                match read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        match self.composer_key(key) {
                            ComposerEvent::Interrupt | ComposerEvent::Exit => {
                                self.note("Cancelled", "Local compute is occupied by another session or an improvement cycle.");
                                return Ok(false);
                            }
                            ComposerEvent::Submit(text) => {
                                if self.route_busy_command(app, text) {
                                    return Ok(false);
                                }
                            }
                            ComposerEvent::Action(Action::CommandPalette) => {
                                if self.route_busy_command(app, "/palette".into()) {
                                    return Ok(false);
                                }
                            }
                            ComposerEvent::Action(Action::ModelSelect | Action::ModelRoles) => {
                                self.route_busy_command(app, "/model".into());
                            }
                            ComposerEvent::Action(Action::Settings) => {
                                self.route_busy_command(app, "/compute".into());
                            }
                            ComposerEvent::Action(Action::EditExternal) => self.edit_externally(),
                            _ => {}
                        }
                    }
                    Event::Paste(text) => self.composer_paste(&text),
                    Event::Resize(..) => self.term.on_resize(),
                    _ => {}
                }
            }
        }
    }

    fn prepare_local(&mut self, app: &App) -> Result<bool> {
        if app.config.endpoint.is_some() {
            return Ok(true);
        }
        if !app.config.python.is_file() || !app.config.worker.is_file() {
            let choice = self.ask("Set up local MLX",
                "Installs a private Python environment with uv under your Dial home directory. Downloads need network access.",
                &[("Install MLX runtime", 'y'), ("Cancel", 'n')]);
            if choice != Some(0) {
                return Ok(false);
            }
            self.status.phase = Phase::Setup;
            self.note("MLX", "installing the local runtime…");
            self.paint(None)?;
            crate::setup::ensure_runtime(&app.layout)?;
            ensure!(
                app.config.python.is_file() && app.config.worker.is_file(),
                "custom Python/worker paths are missing; update them or use the default local setup paths"
            );
        }
        if !Path::new(&app.config.model).exists() {
            let needs_pin = app
                .store
                .as_ref()
                .map(|s| s.get_meta("serving_capabilities"))
                .transpose()?
                .flatten()
                .is_none();
            if needs_pin {
                self.status.phase = Phase::Setup;
                self.note(
                    "MLX",
                    "resolving the local model; the first use may download weights…",
                );
                self.paint(None)?;
                let caps = crate::learn::worker::capabilities(&app.config)?;
                ensure!(
                    caps["inference"] == true,
                    "local model is not supported by this MLX runtime"
                );
                if let Some(store) = &app.store {
                    store.set_meta("serving_capabilities", &caps.to_string())?;
                }
            }
        }
        Ok(true)
    }

    // ----- overlays -------------------------------------------------------------------------

    fn verify_lora(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        selection: &ModelSelection,
    ) -> Result<()> {
        if selection.provider != "mlx"
            || selection.base_url.is_some()
            || !Path::new(&selection.model).is_absolute()
            || !Path::new(&selection.model).is_dir()
        {
            self.note(
                "LoRA verification",
                "Select a downloaded local MLX model. Remote endpoints cannot be probed here.",
            );
            return Ok(());
        }
        if !app.release_idle_compute(agent) {
            self.note(
                "LoRA verification",
                "Wait for running child agents to finish, then retry F6.",
            );
            return Ok(());
        }
        let lock = auto::lock_file(&app.layout.user_dir.join("compute.lock"))?;
        if let Err(error) = lock.try_lock_exclusive() {
            self.note("LoRA verification", format!("Local compute is busy; retry F6 after the other session or improvement cycle finishes. {error}"));
            return Ok(());
        }
        let _lease = ProbeLease(lock);
        let mut config = app.config.with_selection(selection);
        config.state_dir = config.learning_dir(&app.layout);
        app.interrupt.store(false, Ordering::SeqCst);
        self.note("LoRA verification", format!("Loading {} to verify adapter gradients and save/reload with a tiny temporary update. Esc cancels.", selection.display_name()));
        let started = Instant::now();
        let result = std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let cancel = app.interrupt.clone();
            let config = &config;
            scope.spawn(move || {
                let _ = tx.send(crate::learn::worker::capabilities_controlled(
                    config, &cancel,
                ));
            });
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started,
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            bridge.set_activity("verifying LoRA support");
            loop {
                bridge.heartbeat();
                match rx.recv_timeout(Duration::from_millis(80)) {
                    Ok(result) => break result,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => {
                        break Err(anyhow::anyhow!("capability worker ended without a result"));
                    }
                }
            }
        });
        let cancelled = app.interrupt.swap(false, Ordering::SeqCst);
        match result {
            Ok(caps) if !cancelled && caps["inference"] != true => {
                self.note("LoRA verification", format!("Could not load the model to verify adapter targets: {}. The previous result was preserved.", caps["reason"].as_str().unwrap_or("model loading failed")));
            }
            Ok(caps) if !cancelled => {
                let store = Store::open(
                    &config.state_dir,
                    &config.model,
                    &config.use_case,
                    &config.harness,
                )?;
                store.set_meta("serving_capabilities", &caps.to_string())?;
                self.note(
                    "LoRA verification",
                    format!(
                        "{}: {}\n{}",
                        selection.display_name(),
                        if caps["lora"] == true {
                            "supported by Dial's trainer"
                        } else {
                            "unsupported by Dial's trainer"
                        },
                        caps["reason"].as_str().unwrap_or("No reason returned")
                    ),
                );
                self.learning = Learning::default();
                self.catalog.refresh(app);
            }
            _ if cancelled => self.note(
                "LoRA verification",
                "Cancelled; the previous result was preserved.",
            ),
            Err(error) => self.note(
                "LoRA verification",
                format!("Could not verify: {error:#}. The previous result was preserved."),
            ),
            _ => {}
        }
        drop(_lease);
        self.status.activity.clear();
        self.refresh_status(app, agent);
        Ok(())
    }

    fn open(&mut self, app: &App, agent: &mut Agent<'_>, which: Which) -> Result<Step> {
        if matches!(which, Which::Models) {
            app.release_idle_compute(agent);
        }
        let mut which = which;
        loop {
            if matches!(which, Which::Compute) {
                app.release_idle_compute(agent);
            }
            let outcome = self.run_overlay(app, which)?;
            let mut step = Step::Done;
            if let Some(config) = outcome.latest {
                step = self.config_changed(app, agent, config);
            }
            if let Some(selection) = outcome.verify_lora {
                self.verify_lora(app, agent, &selection)?;
                return Ok(step);
            }
            if let Some(command) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("command:"))
            {
                return self.command(app, agent, command);
            }
            match (outcome.next.as_deref(), &step) {
                (Some("open:models"), Step::Done) => which = Which::Models,
                (Some("open:providers"), Step::Done) => which = Which::Providers,
                (Some("open:settings"), Step::Done) => which = Which::Settings,
                (Some("open:shortcuts"), Step::Done) => which = Which::Shortcuts,
                (Some("open:compute"), Step::Done) => which = Which::Compute,
                _ => return Ok(step),
            }
        }
    }

    /// Apply a freshly loaded config: UI and approval in place, everything else by rebuilding the agent.
    fn config_changed(&mut self, app: &App, agent: &mut Agent<'_>, fresh: Config) -> Step {
        self.ui = fresh.ui.clone();
        if let Err(error) = self.themes.apply_config(&fresh.ui, self.dark) {
            self.note("Theme", &error);
        }
        if fresh.approval != self.cfg_approval {
            self.cfg_approval = fresh.approval;
            agent.set_approval(fresh.approval);
        }
        if agent_key(&fresh) != agent_key(&app.config) {
            Step::Rebuild(Box::new(fresh))
        } else {
            Step::Done
        }
    }

    fn run_overlay(&mut self, app: &App, which: Which) -> Result<OverlayOutcome> {
        if matches!(which, Which::Models | Which::Providers) {
            self.catalog.start(app);
        }
        let effective = app.reload().unwrap_or_else(|_| app.config.clone());
        let discovery = self.catalog.get();
        let mut overlay = match which {
            Which::Palette => ActiveOverlay::Palette(PaletteOverlay::new(
                palette_entries(&self.commands, &self.skills),
                self.status.phase != Phase::Idle,
            )),
            Which::Compute => ActiveOverlay::Compute(ComputeOverlay::new()),
            Which::Shortcuts => ActiveOverlay::Shortcuts(ShortcutOverlay::new()),
            Which::Settings => {
                let (values, global, project) = settings_tables(app, &effective);
                let themes = self
                    .themes
                    .names()
                    .into_iter()
                    .filter(|e| e.error.is_none())
                    .map(|e| e.name)
                    .collect();
                ActiveOverlay::Settings(SettingsOverlay::new(
                    values, project, global, self.scope, themes,
                ))
            }
            Which::Models => ActiveOverlay::Models(
                ModelOverlay::new(discovery, role_rows(&effective), self.scope)
                    .with_main_lora(self.status.lora),
            ),
            Which::Providers => {
                let user_dir = app.layout.user_dir.clone();
                let has_key = Box::new(move |url: &str| {
                    catalog::credential_token(&user_dir, url)
                        .ok()
                        .flatten()
                        .is_some()
                });
                let mut providers = ProviderOverlay::new(
                    discovery,
                    effective.providers.clone(),
                    has_key,
                    self.scope,
                );
                providers.set_chatgpt_accounts(
                    crate::llm::chatgpt::accounts(&app.layout.user_dir).unwrap_or_default(),
                );
                providers.set_anthropic_signed_in(crate::llm::anthropic_oauth::signed_in(
                    &app.layout.user_dir,
                ));
                providers.set_chatgpt_pending(
                    self.auth_job
                        .as_ref()
                        .is_some_and(|job| job.label == "ChatGPT"),
                );
                ActiveOverlay::Providers(providers)
            }
        };
        let Shell {
            term,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark,
            auth_job,
            ..
        } = self;
        let mut env = OverlayEnv {
            app,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark: *dark,
            latest: None,
            next: None,
            verify_lora: None,
            seen_generation: 0,
            auth_job,
            compute_monitor: if matches!(which, Which::Compute) {
                Some(ComputeMonitor::new(app))
            } else {
                None
            },
        };
        env.seen_generation = env.catalog.generation;
        term.with_overlay(|ot| run_overlay(ot, &mut env, &mut overlay))?;
        env.themes.restore();
        Ok(OverlayOutcome {
            latest: env.latest,
            next: env.next,
            verify_lora: env.verify_lora,
        })
    }
}

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------

impl Shell {
    fn note(&mut self, title: &str, text: impl AsRef<str>) {
        self.transcript.notice(title, text.as_ref());
    }

    /// Persist settings writes, reload the effective config and apply it live.
    fn persist(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        changes: &[Change],
        scope: Scope,
    ) -> Result<Step> {
        settings_store::save_config(&app.layout, scope, changes)?;
        let fresh = app.reload()?;
        Ok(self.config_changed(app, agent, fresh))
    }

    fn help_text(&self) -> String {
        let found = &self.skills;
        let mut lines = vec!["Commands".to_owned()];
        for entry in command_entries(&self.commands, found) {
            let name = match &entry.args_hint {
                Some(hint) => format!("/{} {hint}", entry.name),
                None => format!("/{}", entry.name),
            };
            lines.push(format!("  {name:<28} {}", entry.description));
        }
        lines.push(String::new());
        lines.push(
            "Keys (/hotkeys to view and edit; keybindings.toml for manual configuration)".into(),
        );
        for action in [
            Action::CommandPalette,
            Action::ModelSelect,
            Action::ProviderSetup,
            Action::Settings,
            Action::HistorySearch,
            Action::Newline,
            Action::ToolsExpand,
            Action::ApprovalCycle,
            Action::DisplayReset,
            Action::EditExternal,
            Action::Interrupt,
            Action::Exit,
        ] {
            lines.push(format!(
                "  {:<22} {}",
                self.keys.labels(action),
                action.describe()
            ));
        }
        lines.push(
            "  !<command>             run a shell command yourself (output joins the conversation)"
                .into(),
        );
        lines.push("  !!<command>            run without adding output to the conversation".into());
        lines.push("  Busy: Enter steers · Alt-Enter queues follow-up · Up edits last queued item · Esc cancels that edit".into());
        lines.push("  Busy: /help /hotkeys /todo /skills /commands /context /clear run at once; /exit /new /resume /fork /undo /rollback /compact /plan stop the turn; other commands run when it finishes".into());
        lines.push(
            "  Busy: /model or Alt+M opens the picker now; selections apply after the current turn"
                .into(),
        );
        lines.join("\n")
    }

    /// Commands whose purpose is to end or replace what the running turn is doing, so typing one during a turn
    /// stops it. Every other command leaves the turn alone: it runs at once or right after the turn.
    fn stops_turn(line: &str) -> bool {
        let name = line
            .trim()
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        matches!(
            name,
            "exit"
                | "quit"
                | "q"
                | "new"
                | "clear"
                | "resume"
                | "sessions"
                | "tree"
                | "fork"
                | "undo"
                | "rollback"
                | "compact"
                | "plan"
        )
    }

    /// Shared by inference and local-compute waiting. Only explicit session/work changes cancel.
    fn route_busy_command(&mut self, app: &App, text: String) -> bool {
        let overlay = match text.trim() {
            "/palette" => Some(Which::Palette),
            "/model" | "/models" => Some(Which::Models),
            "/providers" | "/provider" => Some(Which::Providers),
            "/settings" => Some(Which::Settings),
            "/compute" => Some(Which::Compute),
            _ => None,
        };
        if let Some(mut which) = overlay {
            loop {
                match self.run_overlay(app, which) {
                    Ok(outcome) => {
                        if let Some(config) = outcome.latest {
                            self.pending_config = Some(config);
                            self.note(
                                "Model",
                                "Selection saved; it will apply after the current turn.",
                            );
                        }
                        if let Some(selection) = outcome.verify_lora {
                            self.pending_verification = Some(selection);
                            self.note(
                                "LoRA verification",
                                "Verification will run after the current turn releases compute.",
                            );
                        }
                        if let Some(command) = outcome
                            .next
                            .as_deref()
                            .and_then(|next| next.strip_prefix("command:"))
                        {
                            return self.route_busy_command(app, command.to_owned());
                        }
                        which = match outcome.next.as_deref() {
                            Some("open:models") => Which::Models,
                            Some("open:providers") => Which::Providers,
                            Some("open:settings") => Which::Settings,
                            Some("open:shortcuts") => Which::Shortcuts,
                            Some("open:compute") => Which::Compute,
                            _ => break,
                        };
                    }
                    Err(error) => {
                        self.note("Model picker", format!("{error:#}"));
                        break;
                    }
                }
            }
            return false;
        }
        if self.live_command(app, &text) {
            return false;
        }
        let name = text.split_whitespace().next().unwrap_or("");
        if Self::stops_turn(&text) {
            self.note(
                "Command",
                format!("stopping the current turn to run {name}"),
            );
            self.startup = Some(text);
            true
        } else {
            self.note(
                "Command",
                format!("{name} will run when this turn finishes"),
            );
            self.deferred.push_back(text);
            false
        }
    }

    /// Run a command that only reads what the shell already holds, so it works while a turn runs. Returns false
    /// for commands that need the agent: those stop the turn ([`Shell::stops_turn`]) or run right after it.
    fn live_command(&mut self, app: &App, line: &str) -> bool {
        let Some(rest) = line.trim().strip_prefix('/') else {
            return false;
        };
        let (name, args) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, args)| (name, args.trim()));
        match (name, args) {
            ("reliability", "" | "status") => {
                self.note(
                    "Tool reliability",
                    crate::reliability::render(&crate::reliability::live_report(
                        &app.config,
                        &self.request_policy,
                    )),
                );
            }
            ("policy", "" | "status") => {
                self.note(
                    "Adaptive policy",
                    crate::adaptive::render(&crate::adaptive::report(
                        &app.config,
                        &self.request_policy,
                    )),
                );
            }
            ("help" | "?", "") => {
                let text = self.help_text();
                self.note("Help", text);
            }
            ("hotkeys", "") => {
                let text = self.hotkeys_text();
                self.note("Hotkeys", text);
            }
            ("todo", "") => {
                let text = if self.todo.is_empty() {
                    "No tasks.".to_owned()
                } else {
                    self.todo.render()
                };
                self.note("Todo", text);
            }
            ("search", query) if !query.is_empty() => {
                match Session::search(&app.layout, query, 8) {
                    Ok(hits) if hits.is_empty() => self.note("Search", "no matches"),
                    Ok(hits) => {
                        let text = hits
                            .iter()
                            .map(|hit| {
                                format!("{} [{}] {}", short(&hit.session_id), hit.role, hit.snippet)
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.note("Search", text);
                    }
                    Err(error) => self.note("Search", format!("error: {error:#}")),
                }
            }
            ("jobs", "") => {
                let text = match self.live_tasks.as_ref().map(|control| control.list()) {
                    Some(jobs) if !jobs.is_empty() => jobs
                        .iter()
                        .map(|(id, running)| {
                            format!("{id}: {}", if *running { "running" } else { "finished" })
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => "no live jobs".to_owned(),
                };
                self.note("Jobs", text);
            }
            ("message", rest) if rest.split_once(char::is_whitespace).is_some() => {
                let (id, text) = rest.split_once(char::is_whitespace).unwrap_or_default();
                let result = match &self.live_tasks {
                    Some(control) => control.send(id, text.trim()),
                    None => Err(anyhow::anyhow!("no live jobs")),
                };
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", format!("{error:#}")),
                }
            }
            ("cancel", id) if !id.is_empty() => {
                let result = match &self.live_tasks {
                    Some(control) => control.cancel(id),
                    None => Err(anyhow::anyhow!("no live jobs")),
                };
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", format!("{error:#}")),
                }
            }
            ("skills", "") => {
                self.note("Skills", self.skills_listing.clone());
            }
            ("commands", "") => {
                let text = if self.commands.is_empty() {
                    "no custom commands (add .dial/commands/<name>.md)".to_owned()
                } else {
                    self.commands
                        .iter()
                        .map(|def| format!("/{}  {}", def.name, def.description))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.note("Commands", text);
            }
            ("context", "") => {
                let used = self
                    .status
                    .prompt_tokens
                    .map_or_else(|| "unknown".to_owned(), |n| n.to_string());
                self.note(
                    "Context",
                    format!(
                        "last prompt: {used} tokens (window {})",
                        self.status.context_window
                    ),
                );
            }
            _ => return false,
        }
        true
    }

    fn hotkeys_text(&self) -> String {
        let mut lines = Vec::new();
        let mut group = "";
        for row in self.keys.hotkeys() {
            if row.group != group {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push(row.group.to_owned());
                group = row.group;
            }
            lines.push(format!(
                "  {:<24} {:<22} {}{}",
                row.id,
                row.keys,
                row.description,
                if row.customized { "  (customized)" } else { "" }
            ));
        }
        lines.push(String::new());
        lines.push("Run /hotkeys when idle to open the shortcut editor.".to_owned());
        lines.push(format!(
            "Remap in {}/keybindings.toml, for example:  \"app.model.select\" = \"Ctrl+K\"",
            app_home_hint()
        ));
        lines.join("\n")
    }

    /// Show a different session's conversation: clear the screen and replay it from the session file.
    fn show_session(&mut self, session: &Session) {
        let _ = self.term.clear_screen();
        self.transcript = Transcript::new();
        replay(&mut self.transcript, &session.messages());
        self.session_id = session.id().to_owned();
    }

    fn pick_session(
        &mut self,
        app: &App,
        query: &str,
        tree: bool,
        direct: bool,
    ) -> Result<Option<Session>> {
        let mut sessions = Session::list(&app.layout, usize::MAX)?;
        if sessions.is_empty() {
            self.note("Sessions", "no sessions yet");
            return Ok(None);
        }
        if direct && !query.is_empty() {
            let matches: Vec<_> = sessions
                .iter()
                .filter(|s| s.id.starts_with(query))
                .collect();
            if matches.len() == 1 {
                return Session::open(&matches[0].path).map(Some);
            }
            if matches.len() > 1 {
                anyhow::bail!("ambiguous session prefix {query:?}");
            }
        }
        if tree {
            let ancestry = |s: &crate::storage::session::SessionInfo| {
                let mut chain = vec![s.id.clone()];
                let mut parent = s.parent_id.as_deref();
                while let Some(id) = parent {
                    if chain.iter().any(|p| p == id) {
                        break;
                    }
                    chain.push(id.to_owned());
                    parent = sessions
                        .iter()
                        .find(|s| s.id == id)
                        .and_then(|s| s.parent_id.as_deref());
                }
                chain.reverse();
                chain
            };
            let order: HashMap<_, _> = sessions
                .iter()
                .map(|s| (s.id.clone(), ancestry(s)))
                .collect();
            sessions.sort_by(|a, b| order[&a.id].cmp(&order[&b.id]));
        }
        let searchable: Vec<_> = sessions
            .iter()
            .map(|s| {
                format!(
                    "{} {} {} {} {} {}",
                    s.id,
                    s.title,
                    s.first_message,
                    s.started,
                    s.path.display(),
                    s.model
                )
            })
            .collect();
        let mut filter = query.to_owned();
        let mut selected = 0usize;
        loop {
            let found: Vec<_> = searchable
                .iter()
                .enumerate()
                .filter(|(_, s)| text::fuzzy(&filter, s).is_some())
                .map(|(i, _)| i)
                .collect();
            selected = selected.min(found.len().saturating_sub(1));
            let (cols, height) = self.term.size();
            let visible = (height as usize).saturating_sub(6).max(1);
            let start = selected.saturating_sub(visible - 1);
            let theme = self.themes.current();
            let mut rows = vec![Line::from(Span::styled(
                text::ellipsize(
                    &format!(
                        "{}  /{}  ({} matches)",
                        if tree { "Session tree" } else { "Sessions" },
                        text::one_line(&filter),
                        found.len()
                    ),
                    cols as usize,
                    theme.ascii(),
                ),
                theme.fg(Tok::Accent),
            ))];
            for (position, &index) in found.iter().enumerate().skip(start).take(visible) {
                let session = &sessions[index];
                let branch = session
                    .parent_id
                    .as_ref()
                    .map(|id| format!("fork of {} ", short(id)))
                    .unwrap_or_default();
                let label = format!(
                    "{} {}  {}  {} msgs  {}  {}{}",
                    if position == selected { ">" } else { " " },
                    short(&session.id),
                    session_age(&session.started),
                    session.messages,
                    session.model,
                    branch,
                    session.title
                );
                rows.push(Line::from(Span::styled(
                    text::ellipsize(&text::one_line(&label), cols as usize, theme.ascii()),
                    theme.fg(if position == selected {
                        Tok::Accent
                    } else {
                        Tok::Text
                    }),
                )));
            }
            rows.push(Line::from(Span::styled(
                text::clip(
                    "Type to filter · Up/Down · Enter resume · Esc cancel",
                    cols as usize,
                ),
                theme.fg(Tok::Dim),
            )));
            self.paint(Some(rows))?;
            match read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None);
                    }
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down => selected = (selected + 1).min(found.len().saturating_sub(1)),
                    KeyCode::PageUp => selected = selected.saturating_sub(visible),
                    KeyCode::PageDown => {
                        selected = (selected + visible).min(found.len().saturating_sub(1))
                    }
                    KeyCode::Enter => {
                        if let Some(&index) = found.get(selected) {
                            return Session::open(&sessions[index].path).map(Some);
                        }
                    }
                    KeyCode::Backspace => {
                        filter.pop();
                        selected = 0;
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        filter.push(c);
                        selected = 0;
                    }
                    _ => {}
                },
                Event::Paste(value) => {
                    filter.push_str(&text::one_line(&value));
                    selected = 0;
                }
                Event::Resize(..) => self.term.on_resize(),
                _ => {}
            }
        }
    }

    fn copy_answer(&mut self, agent: &Agent<'_>) {
        use std::io::Write;
        let messages = agent.session().messages();
        let Some(answer) = messages
            .iter()
            .rev()
            .filter(|m| m["role"] == "assistant")
            .find_map(|m| m["content"].as_str().filter(|s| !s.is_empty()))
        else {
            self.note("Copy", "no assistant answer yet");
            return;
        };
        for (program, args) in [
            ("pbcopy", &[][..]),
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
        ] {
            let Ok(mut child) = Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            let wrote = child
                .stdin
                .take()
                .is_some_and(|mut stdin| stdin.write_all(answer.as_bytes()).is_ok());
            if child.wait().is_ok_and(|status| status.success()) && wrote {
                self.note("Copy", "answer copied");
                return;
            }
        }
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(answer);
        let mut stdout = std::io::stdout().lock();
        let sent = write!(stdout, "\x1b]52;c;{encoded}\x07").and_then(|_| stdout.flush());
        drop(stdout);
        match sent {
            Ok(()) => self.note(
                "Copy",
                "clipboard sent via OSC52 (terminal support required)",
            ),
            Err(error) => self.note("Copy", format!("{error}")),
        }
    }

    fn command(&mut self, app: &App, agent: &mut Agent<'_>, line: &str) -> Result<Step> {
        let Some(rest) = line.strip_prefix('/') else {
            return self.run_turn(app, agent, line);
        };
        let (name, args) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, args)| (name, args.trim()));
        match name {
            "compute" => return self.open(app, agent, Which::Compute),
            "exit" | "quit" | "q" => return Ok(Step::Quit),
            "help" | "?" => {
                let text = self.help_text();
                self.note("Help", text);
            }
            "model" | "models" | "roles" => return self.open(app, agent, Which::Models),
            "profile" => {
                if args.is_empty() {
                    self.note(
                        "Profiles",
                        app.config
                            .model_profiles
                            .iter()
                            .map(|(name, profile)| {
                                format!("{name}: {} / {}", profile.provider, profile.model)
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                } else {
                    let changes = if args == "off" {
                        vec![Change::Remove {
                            path: "model_profile".into(),
                        }]
                    } else {
                        vec![Change::Set {
                            path: "model_profile".into(),
                            value: args.into(),
                        }]
                    };
                    match self.persist(app, agent, &changes, Scope::Project) {
                        Ok(step) => return Ok(step),
                        Err(error) => self.note("Profile", format!("not changed: {error:#}")),
                    }
                }
            }
            "reasoning" | "thinking" => {
                if args.is_empty() {
                    match reasoning_profile(&app.config) {
                        Ok(Some(profile))=>self.note("Reasoning",format!("{}: {}\nAvailable: {}\nSet with /reasoning <level> or /thinking <level>",app.config.model,
                            thinking_label(&profile),
                            profile.capabilities.reasoning_efforts.iter().map(|effort|effort.as_str()).collect::<Vec<_>>().join(" | "))),
                        Ok(None)=>self.note("Reasoning","Open /models and rescan to load this model's reasoning levels, or select an explicit /profile"),
                        Err(error)=>self.note("Reasoning",format!("{error:#}")),
                    }
                } else {
                    match reasoning_changes(&app.config, args).and_then(|(changes, effort)| {
                        self.persist(app, agent, &changes, Scope::Project)
                            .map(|step| (step, effort))
                    }) {
                        Ok((step, effort)) => {
                            self.note(
                                "Reasoning",
                                format!("{} reasoning set to {effort}", app.config.model),
                            );
                            return Ok(step);
                        }
                        Err(error) => self.note("Reasoning", format!("not changed: {error:#}")),
                    }
                }
            }
            "providers" | "provider" => return self.open(app, agent, Which::Providers),
            "palette" => return self.open(app, agent, Which::Palette),
            "settings" => return self.open(app, agent, Which::Settings),
            "hotkeys" => return self.open(app, agent, Which::Shortcuts),
            "improve" if args == "jobs" || args.starts_with("stop ") => {
                let result = if args == "jobs" {
                    auto::jobs_summary(&app.layout)
                } else {
                    auto::request_stop(&app.layout, args.trim_start_matches("stop ").trim()).map(
                        |value| {
                            value["message"]
                                .as_str()
                                .unwrap_or("stop requested")
                                .to_owned()
                        },
                    )
                };
                self.note(
                    "Improvement",
                    result.unwrap_or_else(|error| format!("{error:#}")),
                );
            }
            "improve" if matches!(args, "cycle" | "enable" | "disable" | "versions" | "tasks") => {
                let result = (|| -> Result<String> {
                    let store = app.store.as_ref().context(
                        "self-improvement is unavailable (disabled, or no model selected)",
                    )?;
                    match args {
                        "cycle" => {
                            ensure!(
                                !crate::learn::compute::paused(&app.layout.user_dir),
                                "Resume background work first"
                            );
                            ensure!(
                                auto::start_cycle(&app.layout, &app.config, store)?,
                                "A cycle is already running or queued"
                            );
                            Ok("Improvement cycle started in the background; coding keeps priority. Follow it with /improve jobs.".into())
                        }
                        "enable" => {
                            auto::resume(&app.config.state_dir)?;
                            store.set_meta("auto_enabled", "true")?;
                            auto::maybe_spawn(&app.layout, &app.config, store)?;
                            Ok("Automatic improvement enabled for this project.".into())
                        }
                        "disable" => {
                            store.set_meta("auto_enabled", "false")?;
                            Ok("Automatic improvement disabled; a running cycle is not interrupted.".into())
                        }
                        "versions" => Ok(serde_json::to_string_pretty(&store.versions()?)?),
                        _ => {
                            let tasks = crate::learn::repository::load(&app.config)?
                                .iter()
                                .map(|t| serde_json::json!({"id":t.spec.id,"split":t.spec.split,"digest":t.digest,"files":t.files.len()}))
                                .collect::<Vec<_>>();
                            Ok(serde_json::to_string_pretty(&tasks)?)
                        }
                    }
                })();
                self.note(
                    "Improvement",
                    result.unwrap_or_else(|error| format!("{error:#}")),
                );
            }
            "improve" => {
                let mut summary = improve_summary(app);
                if let Some(monitor) = &self.adapter_monitor {
                    let runtime = monitor.lock();
                    if let Some(path) = &runtime.adapter {
                        summary.push_str(&format!(
                            "\nThis session: {}\nAdapter: {}",
                            Lora::Unknown.with_runtime(&runtime).label(),
                            path.display()
                        ));
                    } else {
                        summary.push_str("\nThis session: no adapter use confirmed.");
                    }
                }
                self.note("Improvement", summary);
            }
            "changes" => {
                let files = self.git.files(&app.layout.project_root);
                let text = if files.is_empty() {
                    "No changes in the current Git snapshot.".into()
                } else {
                    files
                        .iter()
                        .map(|file| {
                            let stats = if file.untracked {
                                "new".into()
                            } else {
                                match (file.added, file.removed) {
                                    (Some(a), Some(r)) => format!("+{a} −{r}"),
                                    _ => "binary".into(),
                                }
                            };
                            format!("{}  {stats}", crate::ui::text::one_line(&file.path))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.note("Changes", text);
            }
            "theme" => return self.theme_command(app, agent, args),
            "new" | "clear" => match Session::create(
                &app.layout,
                SessionMeta {
                    model: app.config.model.clone(),
                },
            ) {
                Ok(session) => {
                    let id = session.id().to_owned();
                    agent.swap_session(session);
                    if name == "clear" {
                        self.show_session(agent.session());
                    }
                    self.session_id = id.clone();
                    self.note("Session", format!("new session {}", short(&id)));
                }
                Err(error) => self.note("Session", format!("error: {error:#}")),
            },
            "sessions" | "resume" | "tree" => {
                match self.pick_session(app, args, name == "tree", name == "resume") {
                    Ok(Some(session)) => {
                        self.show_session(&session);
                        agent.swap_session(session);
                        self.note(
                            "Session",
                            format!("resumed {}", short(agent.session().id())),
                        );
                    }
                    Ok(None) => {}
                    Err(error) => self.note("Sessions", format!("{error:#}")),
                }
            }
            "rename" => match agent.session_mut().rename(args) {
                Ok(()) => self.note("Session", format!("renamed to {}", agent.session().title())),
                Err(error) => self.note("Session", format!("{error:#}")),
            },
            "export" => {
                let html = args.split_whitespace().any(|a| a == "--html");
                let path = args
                    .split_whitespace()
                    .filter(|a| *a != "--html")
                    .collect::<Vec<_>>()
                    .join(" ");
                let path = if path.is_empty() {
                    format!(
                        "session-{}.{}",
                        agent.session().id(),
                        if html { "html" } else { "md" }
                    )
                } else {
                    path
                };
                match agent.session().export(Path::new(&path), html) {
                    Ok(()) => self.note("Export", format!("saved {path}")),
                    Err(error) => self.note("Export", format!("{error:#}")),
                }
            }
            "copy" => self.copy_answer(agent),
            "plan" => {
                let next = match args {
                    "" => !agent.plan_mode(),
                    "on" => true,
                    "off" => false,
                    _ => {
                        self.note("Plan", "usage: /plan [on|off]");
                        return Ok(Step::Done);
                    }
                };
                agent.set_plan_mode(next);
                self.note(
                    "Plan",
                    if next {
                        "read-only planning enabled"
                    } else {
                        "planning disabled"
                    },
                );
            }
            "permissions" => {
                if let Some(rest) = args.strip_prefix("remove ") {
                    let mut words = rest.split_whitespace();
                    let index = words
                        .next()
                        .and_then(|s| s.parse::<usize>().ok())
                        .and_then(|n| n.checked_sub(1));
                    let persist = words.any(|s| s == "--persist");
                    match index {
                        Some(index) => match agent.remove_permission_rule(index, persist) {
                            Ok(()) => self.note("Permissions", "rule removed"),
                            Err(error) => self.note("Permissions", format!("{error:#}")),
                        },
                        None => self.note(
                            "Permissions",
                            "usage: /permissions remove <number> [--persist]",
                        ),
                    }
                } else if args.starts_with("allow ") || args.starts_with("deny ") {
                    use crate::agent::approval::{Effect, PermissionRule};
                    let mut words = args.split_whitespace();
                    let effect = if words.next() == Some("allow") {
                        Effect::Allow
                    } else {
                        Effect::Deny
                    };
                    let first = words.next().unwrap_or("");
                    let persist = first == "--persist";
                    let tool = if persist {
                        words.next().unwrap_or("")
                    } else {
                        first
                    };
                    // `--path <glob>` (repeatable) scopes file tools; remaining words are a bash command prefix.
                    let (mut paths, mut command_prefix) = (Vec::new(), Vec::new());
                    while let Some(word) = words.next() {
                        if word == "--path" {
                            paths.extend(words.next().map(str::to_owned));
                        } else {
                            command_prefix.push(word.to_owned());
                        }
                    }
                    let rule = PermissionRule {
                        effect,
                        tool: tool.into(),
                        command_prefix,
                        paths,
                    };
                    match agent.add_permission_rule(rule, persist) {
                        Ok(()) => self.note(
                            "Permissions",
                            if persist {
                                "rule saved"
                            } else {
                                "session rule added"
                            },
                        ),
                        Err(error) => self.note("Permissions", format!("{error:#}")),
                    }
                } else if args.is_empty() {
                    let rules = agent
                        .permission_rules()
                        .iter()
                        .enumerate()
                        .map(|(i, rule)| {
                            format!(
                                "{}: {:?} {} {}{}",
                                i + 1,
                                rule.effect,
                                rule.tool,
                                rule.command_prefix.join(" "),
                                rule.paths
                                    .iter()
                                    .map(|p| format!(" --path {p}"))
                                    .collect::<String>()
                            )
                        })
                        .collect::<Vec<_>>();
                    self.note(
                        "Permissions",
                        if rules.is_empty() {
                            "no allow/deny rules".into()
                        } else {
                            rules.join("\n")
                        },
                    );
                } else {
                    self.note("Permissions", "usage: /permissions; /permissions allow|deny [--persist] <tool> [prefix words | --path <glob>…]; /permissions remove <number> [--persist]");
                }
            }
            "cost" => {
                let cost = agent
                    .session_cost()
                    .map(|cost| format!("${cost:.6} USD"))
                    .unwrap_or_else(|| "pricing unavailable for this session".into());
                let m = agent.metrics();
                self.note("Cost and usage", format!("{cost}\nMain agent, current runtime: {} responses (usage reported for {}).\nInput: {} · cached reads: {} · cache writes: {}\nOutput: {} · reasoning: {} (provider-reported; may overlap output).\nWorker usage and transport retries are not included. Missing usage is unknown, not free.", m.responses, m.usage_reported_responses, m.prompt_tokens, m.cached_tokens, m.cache_write_tokens, m.output_tokens, m.reasoning_tokens));
            }
            "jobs" => {
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                let result = agent
                    .subagent_status(if args.is_empty() { None } else { Some(args) }, &mut bridge);
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", format!("{error:#}")),
                }
            }
            "message" => {
                let result = match args.split_once(char::is_whitespace) {
                    Some((id, text)) => agent.subagent_message(id, text.trim()),
                    None => Err(anyhow::anyhow!("usage: /message <id> <text>")),
                };
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", format!("{error:#}")),
                }
            }
            "cancel" => match agent.cancel_subagent(args) {
                Ok(value) => self.note("Jobs", value.to_string()),
                Err(error) => self.note("Jobs", format!("{error:#}")),
            },
            "fork" => match agent.session().fork(&app.layout) {
                Ok(session) => {
                    let id = session.id().to_owned();
                    agent.swap_session(session);
                    self.session_id = id.clone();
                    self.note("Session", format!("forked into {}", short(&id)));
                }
                Err(error) => self.note("Session", format!("error: {error:#}")),
            },
            "compact" => {
                if !self.wait_compute(app)? {
                    return Ok(Step::Done);
                }
                self.composer.set_busy(true);
                self.status.phase = Phase::Compacting;
                self.status.activity = "compacting".into();
                let result = {
                    let mut bridge = Bridge {
                        app,
                        shell: self,
                        interrupt: app.interrupt.clone(),
                        started: Instant::now(),
                        last_paint: Instant::now() - PAINT_EVERY,
                        failed: false,
                        pending: Vec::new(),
                        next_id: 0,
                    };
                    bridge.repaint(true);
                    agent.compact(&mut bridge)
                };
                self.composer.set_busy(false);
                if let Err(error) = result {
                    self.transcript.error(&format!("{error:#}"));
                }
            }
            "undo" => {
                if let (Some(trajectory), Some(store)) = (self.last_trajectory, app.store.as_ref())
                {
                    let outcome = Outcome {
                        trajectory,
                        score: 0.0,
                        source: "user:undo".into(),
                        reference: None,
                    };
                    if let Err(error) = store.outcome(&outcome) {
                        self.note("Undo", format!("could not record undo feedback: {error:#}"));
                    }
                }
                let mut lines = Vec::new();
                if let Some(checkpoints) = agent.checkpoints() {
                    match checkpoints.rollback(1) {
                        Ok(report) => lines.push(format!(
                            "restored {} file(s), removed {}",
                            report.restored.len(),
                            report.removed.len()
                        )),
                        Err(error) => lines.push(format!("rollback: {error:#}")),
                    }
                }
                match agent.session_mut().rewind(1) {
                    Ok(dropped) => lines.push(format!("dropped {dropped} message(s)")),
                    Err(error) => lines.push(format!("rewind: {error:#}")),
                }
                self.note("Undo", lines.join("\n"));
            }
            "rollback" => {
                let turns = args.parse().unwrap_or(1);
                match agent.checkpoints() {
                    Some(checkpoints) => match checkpoints.rollback(turns) {
                        Ok(report) => self.note(
                            "Rollback",
                            format!(
                                "restored {:?}, removed {:?}",
                                report.restored, report.removed
                            ),
                        ),
                        Err(error) => self.note("Rollback", format!("{error:#}")),
                    },
                    None => self.note("Rollback", "checkpoints unavailable"),
                }
            }
            "todo" => self.note("Todo", agent.todo().render()),
            "skills" => {
                match skills::management::command(&app.layout, context::split_words(args)) {
                    Ok(text) => self.note("Skills", text),
                    Err(error) => self.note("Skills", format!("{error:#}")),
                }
                self.refresh_commands(app);
            }
            "laya" => {
                if matches!(args, "on" | "off") {
                    let enabled = args == "on";
                    let step = self.persist(
                        app,
                        agent,
                        &[Change::Set {
                            path: "laya.enabled".into(),
                            value: toml::Value::Boolean(enabled),
                        }],
                        Scope::Project,
                    )?;
                    self.note("Laya",if enabled {"Decision advice enabled for this project. /laya check probes Dial's managed service."} else {"Decision advice disabled for this project. The managed service can shut down when idle."});
                    return Ok(step);
                }
                if !matches!(args, "" | "status" | "check" | "start") {
                    self.note("Laya", "Usage: /laya status | check | start | on | off");
                } else {
                    app.interrupt.store(false, Ordering::SeqCst);
                    let mut bridge = Bridge {
                        app,
                        shell: self,
                        interrupt: app.interrupt.clone(),
                        started: Instant::now(),
                        last_paint: Instant::now() - PAINT_EVERY,
                        failed: false,
                        pending: Vec::new(),
                        next_id: 0,
                    };
                    bridge.shell.composer.set_busy(true);
                    let result = agent.laya_status(args == "check", args == "start", &mut bridge);
                    bridge.shell.composer.set_busy(false);
                    match result {
                        Ok(text) => bridge.shell.note("Laya", text),
                        Err(error) => bridge.shell.note("Laya", format!("{error:#}")),
                    }
                }
            }
            "understanding" if args == "build" => {
                app.interrupt.store(false, Ordering::SeqCst);
                if !self.wait_compute(app)? || !self.prepare_local(app)? {
                    return Ok(Step::Done);
                }
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                bridge.shell.composer.set_busy(true);
                let result = agent.synthesize_architecture(true, &mut bridge);
                bridge.shell.composer.set_busy(false);
                if let Err(error) = result {
                    bridge.shell.note("Architecture", format!("{error:#}"));
                }
            }
            "reliability" => {
                self.request_policy = agent.session().control_state();
                self.note(
                    "Tool reliability",
                    crate::reliability::render(&crate::reliability::live_report(
                        &app.config,
                        &self.request_policy,
                    )),
                );
            }
            "policy" => {
                self.request_policy = agent.session().control_state();
                self.note(
                    "Adaptive policy",
                    crate::adaptive::render(&crate::adaptive::report(
                        &app.config,
                        &self.request_policy,
                    )),
                );
            }
            "steering" => {
                let mut value = crate::steering::status(&app.config);
                value["runtime"] = agent.steering_runtime();
                self.note("Activation steering", crate::steering::render(&value));
            }
            "experience" => {
                if args == "ledger" {
                    self.note(
                        "Investigation",
                        serde_json::to_string_pretty(
                            &agent.session().control_state()["investigation_ledger"],
                        )
                        .unwrap_or_default(),
                    );
                } else {
                    let result = std::thread::scope(|scope| {
                        let (tx, rx) = std::sync::mpsc::channel();
                        let layout = &app.layout;
                        scope.spawn(move || {
                            let _ = tx.send(
                                crate::storage::experience::Experience::open(layout)
                                    .and_then(|store| store.inspect())
                                    .map(|report| crate::storage::experience::render(&report)),
                            );
                        });
                        let mut bridge = Bridge {
                            app,
                            shell: self,
                            interrupt: app.interrupt.clone(),
                            started: Instant::now(),
                            last_paint: Instant::now() - PAINT_EVERY,
                            failed: false,
                            pending: Vec::new(),
                            next_id: 0,
                        };
                        loop {
                            match rx.recv_timeout(Duration::from_millis(80)) {
                                Ok(result) => break result,
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                    bridge.heartbeat()
                                }
                                Err(_) => {
                                    break Err(anyhow::anyhow!("experience worker disconnected"));
                                }
                            }
                        }
                    });
                    let mut text = result.unwrap_or_else(|error| format!("{error:#}"));
                    text.push_str(&format!("\nAssistance: {} · syntax guard {} · related tests {} · compact tools {}\nTeacher: {} · hosted experience role required · {}s budget\nEdit controls in /settings; model role in Alt+M → Roles → experience.",app.config.assistance.enabled,app.config.assistance.syntax_guard,app.config.assistance.selective_verification,app.config.assistance.compact_tools,app.config.assistance.teacher_enabled,app.config.assistance.teacher_timeout_secs));
                    self.note("Experience", text);
                }
            }
            "understanding" => {
                app.interrupt.store(false, Ordering::SeqCst);
                self.composer.set_busy(true);
                let result = std::thread::scope(|scope| {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let layout = &app.layout;
                    let cancel = app.interrupt.clone();
                    scope.spawn(move || {
                        let _ = tx.send(
                            crate::storage::codebase::Codebase::open(layout)
                                .and_then(|cache| cache.understanding(args, &cancel))
                                .map(|report| {
                                    crate::storage::codebase::render_understanding(&report, 40)
                                }),
                        );
                    });
                    let mut bridge = Bridge {
                        app,
                        shell: self,
                        interrupt: app.interrupt.clone(),
                        started: Instant::now(),
                        last_paint: Instant::now() - PAINT_EVERY,
                        failed: false,
                        pending: Vec::new(),
                        next_id: 0,
                    };
                    bridge.set_activity("inspecting saved understanding");
                    loop {
                        match rx.recv_timeout(Duration::from_millis(80)) {
                            Ok(result) => break result,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => bridge.heartbeat(),
                            Err(_) => {
                                break Err(anyhow::anyhow!("understanding worker disconnected"));
                            }
                        }
                    }
                });
                self.composer.set_busy(false);
                self.note(
                    "Understanding",
                    result.unwrap_or_else(|error| format!("{error:#}")),
                );
            }
            "memory" => {
                let result = crate::storage::provenance::ProvenanceMemory::open(&app.layout)
                    .and_then(|memory| memory.recall(args, None, 20, 24000))
                    .and_then(|result| serde_json::to_string_pretty(&result).map_err(Into::into));
                match result {
                    Ok(text) => self.note("Memory", text),
                    Err(error) => self.note("Memory", format!("error: {error:#}")),
                }
            }
            "commands" => {
                let text = if self.commands.is_empty() {
                    "no custom commands (add .dial/commands/<name>.md)".to_owned()
                } else {
                    self.commands
                        .iter()
                        .map(|def| format!("/{}  {}", def.name, def.description))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.note("Commands", text);
            }
            "search" => match Session::search(&app.layout, args, 8) {
                Ok(hits) if hits.is_empty() => self.note("Search", "no matches"),
                Ok(hits) => self.note(
                    "Search",
                    hits.iter()
                        .map(|hit| {
                            format!("{} [{}] {}", short(&hit.session_id), hit.role, hit.snippet)
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(error) => self.note("Search", format!("error: {error:#}")),
            },
            "context" => self.note(
                "Context",
                format!(
                    "last prompt: {} tokens (window {}), session {} messages",
                    agent.last_prompt_tokens,
                    agent.context_window(),
                    agent.session().messages().len()
                ),
            ),
            "approval" => {
                match args {
                    "ask" => agent.set_approval(Approval::Ask),
                    "auto" => agent.set_approval(Approval::Auto),
                    "readonly" => agent.set_approval(Approval::Readonly),
                    "" => {}
                    _ => {
                        self.note("Approval", "usage: /approval ask|auto|readonly");
                        return Ok(Step::Done);
                    }
                }
                self.note("Approval", format!("{:?}", agent.approval()).to_lowercase());
            }
            "outcome" => {
                let (score, comment) = args
                    .split_once(char::is_whitespace)
                    .map_or((args, ""), |(score, comment)| (score, comment.trim()));
                match (
                    score.parse::<f64>().ok(),
                    self.last_trajectory,
                    app.store.as_ref(),
                ) {
                    (Some(score), Some(trajectory), Some(store)) => {
                        let outcome = Outcome {
                            trajectory,
                            score,
                            source: "user".into(),
                            reference: (!comment.is_empty()).then(|| comment.to_owned()),
                        };
                        match store.outcome(&outcome) {
                            Ok(()) => self.note(
                                "Outcome",
                                format!("recorded {score} for trajectory {trajectory}"),
                            ),
                            Err(error) => self.transcript.error(&format!("{error:#}")),
                        }
                    }
                    (None, _, _) => self.note("Outcome", "usage: /outcome <0-100> [note]"),
                    (_, None, _) => self.note("Outcome", "no completed turn to score yet"),
                    (_, _, None) => self.note(
                        "Outcome",
                        "self-improvement is not configured, so there is nowhere to record this",
                    ),
                }
            }
            other if other.starts_with("skill:") => {
                let skill = &other["skill:".len()..];
                let known = skills::discover(&app.layout)
                    .into_iter()
                    .any(|s| s.name == skill);
                if !known {
                    self.note(
                        "Skill",
                        format!("unknown skill {skill:?}; /skills lists them"),
                    );
                    return Ok(Step::Done);
                }
                let task = if args.is_empty() {
                    String::new()
                } else {
                    format!(" Task: {args}")
                };
                return self.run_turn(
                    app,
                    agent,
                    &format!(
                        "Use the \"{skill}\" skill (load it with the skill tool first).{task}"
                    ),
                );
            }
            _ => match self.commands.iter().find(|def| def.name == name) {
                Some(def) => {
                    let prompt = context::expand_command(def, args);
                    return self.run_turn(app, agent, &prompt);
                }
                None => self.note(
                    "Command",
                    format!("unknown command /{name}; /help lists commands"),
                ),
            },
        }
        Ok(Step::Done)
    }

    fn theme_command(&mut self, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
        if args.is_empty() {
            let active = self.themes.current().name().to_owned();
            let mut lines: Vec<String> = self
                .themes
                .names()
                .into_iter()
                .map(|entry| {
                    let mark = if entry.name == active { "●" } else { " " };
                    match entry.error {
                        Some(error) => format!("{mark} {}  (cannot load: {error})", entry.name),
                        None => format!(
                            "{mark} {}{}",
                            entry.name,
                            if entry.builtin { "" } else { "  (custom)" }
                        ),
                    }
                })
                .collect();
            lines.push(String::new());
            lines.push(
                "/theme <name> switches (saved globally). Custom themes are JSON files in:".into(),
            );
            lines.push(format!("  {}", self.themes.dir().display()));
            lines.push("Create a custom JSON theme in that directory; it reloads live.".into());
            self.note("Themes", lines.join("\n"));
            return Ok(Step::Done);
        }
        if self.themes.names().iter().all(|entry| entry.name != args) && args != "auto" {
            self.note(
                "Theme",
                format!("unknown theme {args:?}; /theme lists them"),
            );
            return Ok(Step::Done);
        }
        let changes = [Change::Set {
            path: "ui.theme".into(),
            value: args.to_owned().into(),
        }];
        match self.persist(app, agent, &changes, Scope::Global) {
            Ok(step) => {
                self.note("Theme", format!("switched to {args}"));
                Ok(step)
            }
            Err(error) => {
                self.note("Theme", format!("not changed: {error:#}"));
                Ok(Step::Done)
            }
        }
    }
}

fn app_home_hint() -> String {
    std::env::var("DIAL_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "~/.dial".into())
}

fn improve_summary(app: &App) -> String {
    let Some(store) = &app.store else {
        return format!(
            "self-improvement recording is unavailable: {}",
            app.store_error.as_deref().unwrap_or("no store")
        );
    };
    let version = store
        .active()
        .map(|active| format!("v{}", active.id))
        .unwrap_or_else(|error| format!("unavailable ({error:#})"));
    let state = store
        .get_meta("auto_status")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "idle".into());
    let enabled = auto::enabled(&app.config, store).unwrap_or(false);
    let readiness = auto::readiness(&app.config, store)
        .unwrap_or_else(|error| format!("schedule unavailable: {error:#}"));
    let mut summary = format!(
        "Selected model: {} · version {version} · {state} · next cycle: {}\nlearning records stay in {}",
        if enabled {
            "automatic improvement enabled"
        } else {
            "automatic improvement paused"
        },
        readiness,
        app.config.state_dir.display()
    );
    match auto::remaining(store, "last_training_attempt", app.config.training.interval_secs, crate::learn::improve::now()) {
        Ok(0) => summary.push_str("\nNext LoRA attempt: cooldown elapsed; training still requires verified data and a passing evaluation."),
        Ok(wait) => summary.push_str(&format!("\nNext LoRA attempt: cooldown {} remaining.", auto::duration(wait))),
        Err(error) => summary.push_str(&format!("\nLoRA cooldown unavailable: {error:#}")),
    }
    summary.push_str(&format!("\nTraining requires at least {} verified examples and a validation split. Cooldowns are eligibility delays, not training-duration estimates.", app.config.improve.lora_min_examples.max(app.config.training.batch_size)));
    if state == "running"
        && let Ok(Some(raw)) = store.get_meta("cycle_progress")
        && let Ok(value) = serde_json::from_str::<Value>(&raw)
    {
        let elapsed = crate::learn::improve::now().saturating_sub(
            value["started"]
                .as_u64()
                .unwrap_or(crate::learn::improve::now()),
        );
        summary.push_str(&format!(
            "\nCurrent stage: {}\nCycle elapsed: {}. Completion ETA is not yet available.",
            value["stage"].as_str().unwrap_or("unknown"),
            auto::duration(elapsed)
        ));
        if let Some(peak) = value["training"]["peak_memory_gb"].as_f64() {
            summary.push_str(&format!(
                "\nLast training report: MLX peak allocated memory {peak:.2} GB."
            ));
        }
    }
    summary.push_str(&format!(
        "\n{}",
        lora_state(&app.config, Some(store), store.active().ok().as_ref()).label()
    ));
    if let Ok(Some(raw)) = store.get_meta("serving_capabilities")
        && let Ok(caps) = serde_json::from_str::<Value>(&raw)
        && crate::learn::worker::current_lora_probe(&caps)
        && caps["lora"] == false
    {
        summary.push_str(&format!("\nLoRA unavailable in Dial's current trainer: {}. This cycle can still evaluate harness improvements.", caps["reason"].as_str().unwrap_or("the model capability probe rejected adapter training")));
    }
    if let Ok(Some((report, activated, decision))) = store.latest_lora_admission() {
        summary.push_str(&format!(
            "\nLatest LoRA evaluation: v{} against v{} — {} ({decision})\n{}",
            report.candidate,
            report.baseline,
            if activated {
                "accepted"
            } else {
                "not activated"
            },
            report.reason
        ));
        for score in report.scores {
            summary.push_str(&format!(
                "\n{}: baseline {:.3} → candidate {:.3} ({} cases)",
                score.name, score.baseline, score.candidate, score.cases
            ));
        }
        summary.push_str("\nGate scores measure the evaluated tasks; adapter use alone does not prove improvement.");
    } else {
        summary.push_str("\nNo LoRA evaluation recorded yet.");
    }
    if let Ok(Some(raw)) = store.get_meta("last_cycle_report")
        && let Ok(report) = serde_json::from_str::<Value>(&raw)
        && let Some(steps) = report["steps"].as_array()
    {
        for step in steps {
            if step["step"] == "lora_skipped" {
                summary.push_str(&format!(
                    "\nLast training skip: {}",
                    step["data"].as_str().unwrap_or("unknown")
                ));
            }
        }
    }
    if let Ok(tasks) = crate::learn::repository::load(&app.config)
        && !tasks.is_empty()
    {
        summary.push_str(&format!(
            "\nRepository exercises: {} training, {} sealed evaluation.",
            tasks.iter().filter(|t| t.spec.split == "train").count(),
            tasks
                .iter()
                .filter(|t| t.spec.split == "evaluation")
                .count()
        ));
    }
    if let Ok(profiles) = crate::learn::profiles::summary(store) {
        summary.push_str(&format!("\n{profiles}"));
    }
    match auto::jobs_summary(&app.layout) {
        Ok(jobs) => summary.push_str(&format!("\n{jobs}")),
        Err(error) => summary.push_str(&format!("\nShared compute status unavailable: {error:#}")),
    }
    summary.push_str("\nShared compute includes other models and sessions. Use /improve jobs for project-wide status; /improve stop <state_id> requests a safe stop and pauses that model.");
    summary
}

enum ActiveOverlay {
    Palette(PaletteOverlay),
    Compute(ComputeOverlay),
    Settings(SettingsOverlay),
    Models(ModelOverlay),
    Providers(ProviderOverlay),
    Shortcuts(ShortcutOverlay),
}

impl ActiveOverlay {
    fn draw(&mut self, frame: &mut ratatui::Frame, ctx: &DrawCtx) {
        match self {
            Self::Palette(o) => o.draw(frame, ctx),
            Self::Settings(o) => o.draw(frame, ctx),
            Self::Compute(o) => o.draw(frame, ctx),
            Self::Models(o) => o.draw(frame, ctx),
            Self::Providers(o) => o.draw(frame, ctx),
            Self::Shortcuts(o) => o.draw(frame, ctx),
        }
    }
    fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        match self {
            Self::Palette(o) => o.handle(event, keys),
            Self::Settings(o) => o.handle(event, keys),
            Self::Compute(o) => o.handle(event, keys),
            Self::Models(o) => o.handle(event, keys),
            Self::Providers(o) => o.handle(event, keys),
            Self::Shortcuts(o) => o.handle(event, keys),
        }
    }
    fn banner(&mut self, message: String, is_error: bool) {
        match self {
            Self::Models(o) => o.set_status(message, is_error),
            Self::Compute(o) => o.banner(message),
            Self::Providers(o) => o.set_status(message, is_error),
            Self::Palette(_) | Self::Settings(_) | Self::Shortcuts(_) => {}
        }
    }
}

struct AuthJob {
    label: &'static str,
    receiver: std::sync::mpsc::Receiver<Result<String>>,
    cancel: Arc<AtomicBool>,
}
impl Drop for AuthJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}
impl AuthJob {
    fn collect(slot: &mut Option<Self>) -> Option<(&'static str, Result<String>)> {
        let result = slot.as_ref().and_then(|job| match job.receiver.try_recv() {
            Ok(result) => Some((job.label, result)),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(_) => Some((
                job.label,
                Err(anyhow::anyhow!(
                    "{} authentication worker stopped unexpectedly",
                    job.label
                )),
            )),
        });
        if result.is_some() {
            slot.take();
        }
        result
    }
}

struct ComputeMonitor {
    snapshot: Arc<Mutex<Option<Value>>>,
    message: Arc<Mutex<Option<String>>>,
    commands: std::sync::mpsc::SyncSender<Value>,
    cancel: Arc<AtomicBool>,
}
impl ComputeMonitor {
    fn new(app: &App) -> Self {
        let snapshot = Arc::new(Mutex::new(None));
        let message = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let (commands, rx) = std::sync::mpsc::sync_channel::<Value>(8);
        let output = snapshot.clone();
        let status = message.clone();
        let stop = cancel.clone();
        let layout = app.layout.clone();
        let mut config = app.config.clone();
        std::thread::spawn(move || {
            let mut next = Instant::now();
            while !stop.load(Ordering::SeqCst) {
                if let Ok(action) = rx.recv_timeout(Duration::from_millis(80)) {
                    let name = action["action"].as_str().unwrap_or("");
                    let result = (|| -> Result<String> {
                        match name {
                            "start_current" => {
                                ensure!(
                                    !crate::learn::compute::paused(&layout.user_dir),
                                    "Resume background work first"
                                );
                                auto::resume(&config.state_dir)?;
                                config.improve.enabled = true;
                                let store = Store::open(
                                    &config.state_dir,
                                    &config.model,
                                    &config.use_case,
                                    &config.harness,
                                )?;
                                store.set_meta("auto_enabled", "true")?;
                                ensure!(
                                    auto::start_cycle(&layout, &config, &store)?,
                                    "A cycle is already running or queued"
                                );
                                Ok("Improvement started; coding keeps priority.".into())
                            }
                            "stop_now" => crate::learn::compute_controls::stop_legacy(
                                &layout.user_dir,
                                action["state"].as_str().context("select a job")?,
                            ),
                            "stop_process" => crate::learn::processes::stop(
                                &layout.user_dir,
                                action["pid"]
                                    .as_i64()
                                    .and_then(|pid| i32::try_from(pid).ok())
                                    .context("select a process")?,
                                action["identity"]
                                    .as_str()
                                    .context("missing process identity")?,
                            ),
                            _ => crate::learn::compute_controls::action(
                                &layout.user_dir,
                                action["state"].as_str(),
                                name,
                            ),
                        }
                    })();
                    *shared(&status) = Some(result.unwrap_or_else(|error| format!("{error:#}")));
                    next = Instant::now();
                }
                if Instant::now() >= next {
                    match crate::learn::compute_controls::snapshot(&layout.user_dir) {
                        Ok(value) => *shared(&output) = Some(value),
                        Err(error) => {
                            *shared(&status) =
                                Some(format!("Compute status unavailable: {error:#}"))
                        }
                    }
                    next = Instant::now() + Duration::from_millis(750);
                }
            }
        });
        Self {
            snapshot,
            message,
            commands,
            cancel,
        }
    }
}
impl Drop for ComputeMonitor {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

struct OverlayEnv<'a> {
    compute_monitor: Option<ComputeMonitor>,
    verify_lora: Option<ModelSelection>,
    auth_job: &'a mut Option<AuthJob>,
    app: &'a App,
    themes: &'a mut ThemeManager,
    keys: &'a mut Keymap,
    status: &'a StatusData,
    catalog: &'a mut Catalog,
    scope: &'a mut Scope,
    dark: Option<bool>,
    latest: Option<Config>,
    next: Option<String>,
    seen_generation: u64,
}

fn settings_tables(app: &App, effective: &Config) -> (toml::Table, toml::Table, toml::Table) {
    let values = toml::Table::try_from(effective).unwrap_or_default();
    let layer = |scope| settings_store::load_layer(&app.layout, scope).unwrap_or_default();
    (values, layer(Scope::Global), layer(Scope::Project))
}

fn role_rows(config: &Config) -> Vec<(String, Option<ModelSelection>)> {
    ROLES
        .iter()
        .map(|(role, _)| {
            let selection = if *role == "main" {
                (!config.model.is_empty()).then(|| config.selection())
            } else if *role == "architecture"
                && !config.codebase.architecture_model.trim().is_empty()
            {
                Some(config.for_role(role).selection())
            } else {
                config.model_roles.get(*role).cloned()
            };
            ((*role).to_owned(), selection)
        })
        .collect()
}

impl OverlayEnv<'_> {
    fn start_auth(
        &mut self,
        overlay: &mut ActiveOverlay,
        label: &'static str,
        action: impl FnOnce(Arc<AtomicBool>) -> Result<String> + Send + 'static,
    ) {
        if self.auth_job.is_some() {
            self.report(
                overlay,
                Err(anyhow::anyhow!(
                    "An authentication action is already running"
                )),
            );
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (send, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(action(worker_cancel));
        });
        *self.auth_job = Some(AuthJob {
            label,
            receiver,
            cancel,
        });
        if let ActiveOverlay::Providers(providers) = overlay {
            providers.set_chatgpt_pending(label == "ChatGPT");
        }
        self.report(overlay, Ok(format!("Waiting for {label} sign-in in your browser; sign-in continues if you close this panel")));
    }
    fn collect_auth(&mut self, overlay: &mut ActiveOverlay) {
        let result = AuthJob::collect(self.auth_job);
        if let Some((provider, result)) = result {
            if let ActiveOverlay::Providers(providers) = overlay {
                providers.set_chatgpt_pending(false);
                providers.set_chatgpt_accounts(
                    crate::llm::chatgpt::accounts(&self.app.layout.user_dir).unwrap_or_default(),
                );
                providers.set_anthropic_signed_in(crate::llm::anthropic_oauth::signed_in(
                    &self.app.layout.user_dir,
                ));
            }
            let _ = provider;
            self.report(overlay, result);
            self.catalog.refresh(self.app);
        }
    }
    /// Persist, then reload the effective config (files plus command-line flags) and apply the UI part live.
    fn save(&mut self, changes: &[Change], scope: Scope) -> Result<Config> {
        settings_store::save_config(&self.app.layout, scope, changes)?;
        let fresh = self.app.reload()?;
        if let Err(error) = self.themes.apply_config(&fresh.ui, self.dark) {
            // The setting was saved and is valid JSON-wise; the theme itself is what failed. Surface it.
            self.latest = Some(fresh.clone());
            return Err(anyhow::anyhow!(
                "saved, but the theme could not load: {error}"
            ));
        }
        self.latest = Some(fresh.clone());
        Ok(fresh)
    }

    fn effective_value(config: &Config, path: &str) -> toml::Value {
        let table = toml::Table::try_from(config).unwrap_or_default();
        settings_schema::get(&table, path)
            .cloned()
            .unwrap_or_else(|| toml::Value::String(String::new()))
    }

    /// Returns true when the overlay should close.
    fn apply(&mut self, event: OverlayEvent, overlay: &mut ActiveOverlay) -> bool {
        match event {
            OverlayEvent::None => {}
            OverlayEvent::Close => return true,
            OverlayEvent::Set { path, value, scope } => {
                *self.scope = scope;
                self.setting(
                    overlay,
                    &path,
                    &[Change::Set {
                        path: path.clone(),
                        value,
                    }],
                    scope,
                );
            }
            OverlayEvent::Unset { path, scope } => {
                *self.scope = scope;
                self.setting(
                    overlay,
                    &path,
                    &[Change::Remove { path: path.clone() }],
                    scope,
                );
            }
            OverlayEvent::AssignModel {
                role,
                selection,
                scope,
            } => {
                *self.scope = scope;
                let config = self
                    .latest
                    .clone()
                    .unwrap_or_else(|| self.app.config.clone());
                let outcome = selection_changes(&config, &selection, &role)
                    .and_then(|changes| self.save(&changes, scope));
                self.report(
                    overlay,
                    outcome.map(|_| {
                        if role == "main" {
                            format!("switched to {}", selection.display_name())
                        } else {
                            format!("{role} set to {}", selection.display_name())
                        }
                    }),
                );
                self.refresh_roles(overlay);
            }
            OverlayEvent::ClearRole { role, scope } => {
                *self.scope = scope;
                let outcome =
                    clear_role_changes(&role, scope).and_then(|changes| self.save(&changes, scope));
                self.report(
                    overlay,
                    outcome.map(|_| format!("{role} follows the main model")),
                );
                self.refresh_roles(overlay);
            }
            OverlayEvent::AddProvider { id, spec, scope } => {
                *self.scope = scope;
                let outcome = self.save(&provider_changes(&id, &spec), scope);
                self.report(overlay, outcome.map(|_| format!("provider {id} added")));
                self.catalog.refresh(self.app);
            }
            OverlayEvent::RemoveProvider { id, scope } => {
                *self.scope = scope;
                let outcome = self.save(
                    &[Change::Remove {
                        path: format!("providers.{id}"),
                    }],
                    scope,
                );
                self.report(overlay, outcome.map(|_| format!("provider {id} removed")));
                self.catalog.refresh(self.app);
            }
            OverlayEvent::SaveCredential {
                provider,
                base_url,
                mut token,
            } => {
                let verified = matches!(provider.as_str(), "openai" | "anthropic");
                let outcome = catalog::verify_and_save_credential(
                    &self.app.layout.user_dir,
                    &provider,
                    &base_url,
                    &mut token,
                );
                let success = if verified {
                    format!("{provider} API key verified and saved (owner-only file)")
                } else {
                    "API key saved (owner-only file)".to_owned()
                };
                self.report(overlay, outcome.map(|_| success));
                self.catalog.refresh(self.app);
            }
            OverlayEvent::OpenRouterLogin => {
                let outcome = match crate::llm::auth::openrouter_login() {
                    Ok(mut key) => catalog::verify_and_save_credential(
                        &self.app.layout.user_dir,
                        "openrouter",
                        catalog::OPENROUTER_BASE_URL,
                        key.as_mut_string(),
                    )
                    .map(|()| "OpenRouter sign-in verified; key saved locally".to_owned()),
                    Err(error) => Err(error),
                };
                self.report(overlay, outcome);
                self.catalog.refresh(self.app);
            }
            OverlayEvent::ChatGptLogin { client_id } => {
                let dir = self.app.layout.user_dir.clone();
                self.start_auth(overlay, "ChatGPT", move |cancel| {
                    crate::llm::chatgpt::login(&dir, client_id.as_deref(), cancel)
                });
            }
            OverlayEvent::ChatGptCancel => {
                self.auth_job.take();
                if let ActiveOverlay::Providers(providers) = overlay {
                    providers.set_chatgpt_pending(false);
                }
                self.report(overlay, Ok("Cancelled pending ChatGPT sign-in".into()));
            }
            OverlayEvent::ChatGptSelect { client_id } => {
                let dir = self.app.layout.user_dir.clone();
                self.start_auth(overlay, "ChatGPT", move |_| {
                    crate::llm::chatgpt::select_account(&dir, &client_id).map(|_| {
                        "ChatGPT account selected; choose an available model in /models".into()
                    })
                });
            }
            OverlayEvent::ChatGptSignOut { client_id } => {
                let dir = self.app.layout.user_dir.clone();
                self.start_auth(overlay, "ChatGPT", move |cancel| {
                    crate::llm::chatgpt::sign_out(&dir, &client_id, cancel)
                });
            }
            OverlayEvent::AnthropicLogin => {
                let dir = self.app.layout.user_dir.clone();
                self.start_auth(overlay, "Claude", move |cancel| {
                    crate::llm::anthropic_oauth::login(&dir, cancel)
                });
            }
            OverlayEvent::AnthropicSignOut => {
                let outcome = crate::llm::anthropic_oauth::sign_out(&self.app.layout.user_dir);
                self.report(overlay, outcome);
                self.catalog.refresh(self.app);
            }
            OverlayEvent::RemoveCredential { base_url } => {
                let outcome = catalog::remove_credential(&self.app.layout.user_dir, &base_url);
                self.report(overlay, outcome.map(|_| "API key removed".to_owned()));
                self.catalog.refresh(self.app);
            }
            OverlayEvent::Refresh => self.catalog.refresh(self.app),
            OverlayEvent::VerifyLora(selection) => {
                self.verify_lora = Some(selection);
                return true;
            }
            OverlayEvent::PreviewTheme(Some(name)) => {
                let _ = self.themes.preview(&name);
            }
            OverlayEvent::PreviewTheme(None) => self.themes.restore(),
            OverlayEvent::Rebind { action, chords } => {
                let chords: Vec<&str> = chords.iter().map(String::as_str).collect();
                let outcome = self
                    .keys
                    .edit_and_save(&self.app.layout.user_dir, action, &chords);
                if let ActiveOverlay::Shortcuts(shortcuts) = overlay {
                    shortcuts.settled(
                        outcome
                            .map(|_| "Saved for all projects; shortcuts are active now".to_owned())
                            .map_err(|error| format!("{error:#}")),
                    );
                }
            }
            OverlayEvent::Action(action) => {
                if let Some(payload) = action.strip_prefix("compute:") {
                    if let Some(monitor) = &self.compute_monitor {
                        match serde_json::from_str(payload)
                            .map_err(anyhow::Error::from)
                            .and_then(|action| {
                                monitor
                                    .commands
                                    .try_send(action)
                                    .map_err(anyhow::Error::from)
                            }) {
                            Ok(()) => overlay.banner("Applying compute control…".into(), false),
                            Err(error) => overlay.banner(error.to_string(), true),
                        }
                    }
                    return false;
                }
                if action.starts_with("open:") || action.starts_with("command:") {
                    self.next = Some(action);
                    return true;
                }
            }
        }
        false
    }

    fn setting(
        &mut self,
        overlay: &mut ActiveOverlay,
        path: &str,
        changes: &[Change],
        scope: Scope,
    ) {
        let ActiveOverlay::Settings(settings) = overlay else {
            return;
        };
        match self.save(changes, scope) {
            Ok(fresh) => {
                settings.settled(path, Ok(Self::effective_value(&fresh, path)));
                let (values, global, project) = settings_tables(self.app, &fresh);
                settings.replace_values(values, project, global);
            }
            Err(error) => settings.settled(path, Err(format!("{error:#}"))),
        }
    }

    fn report(&mut self, overlay: &mut ActiveOverlay, outcome: Result<String>) {
        match outcome {
            Ok(message) => overlay.banner(message, false),
            Err(error) => overlay.banner(format!("{error:#}"), true),
        }
    }

    fn refresh_roles(&mut self, overlay: &mut ActiveOverlay) {
        if let (ActiveOverlay::Models(models), Some(config)) = (overlay, &self.latest) {
            models.set_roles(role_rows(config));
        }
    }
}

fn run_overlay(
    ot: &mut OverlayTerminal<'_>,
    env: &mut OverlayEnv<'_>,
    overlay: &mut ActiveOverlay,
) -> Result<()> {
    let mut paste_guard = crate::ui::input::PasteGuard::default();
    loop {
        if let (Some(monitor), ActiveOverlay::Compute(panel)) =
            (&env.compute_monitor, &mut *overlay)
        {
            if let Some(value) = shared(&monitor.snapshot).take() {
                panel.update(value);
            }
            if let Some(message) = shared(&monitor.message).take() {
                panel.banner(message);
            }
        }
        env.collect_auth(overlay);
        env.catalog.collect();
        if env.catalog.generation != env.seen_generation {
            env.seen_generation = env.catalog.generation;
            if let Some(discovery) = env.catalog.get() {
                match overlay {
                    ActiveOverlay::Models(models) => models.set_discovery(discovery),
                    ActiveOverlay::Providers(providers) => providers.set_discovery(discovery),
                    ActiveOverlay::Palette(_)
                    | ActiveOverlay::Settings(_)
                    | ActiveOverlay::Shortcuts(_)
                    | ActiveOverlay::Compute(_) => {}
                }
            }
        }
        if env.themes.reload_if_changed() { /* the next draw uses the reloaded theme */ }
        {
            let themes = &*env.themes;
            let status = env.status;
            let preview = |cfg: &UiConfig, width: u16| {
                statusline::preview_row(cfg, status, themes.current(), width)
            };
            ot.draw(|frame| {
                let ctx = DrawCtx {
                    theme: themes.current(),
                    keys: env.keys,
                    status,
                    preview_status: &preview,
                };
                overlay.draw(frame, &ctx);
            })?;
        }
        if !poll(TICK)? {
            continue;
        }
        let mut event = read()?;
        // The palette can dispatch actions. A pasted newline must never run the selected action.
        if matches!(overlay, ActiveOverlay::Palette(_)) {
            match &event {
                Event::Paste(_) => paste_guard.pasted(Instant::now()),
                Event::Key(key) => {
                    if let Some(value) = paste_guard.text_for_key(
                        *key,
                        poll(Duration::ZERO).unwrap_or(false),
                        Instant::now(),
                    ) {
                        if value.is_empty() {
                            continue;
                        }
                        event = Event::Paste(value.into());
                    }
                }
                _ => {}
            }
        }
        if matches!(event, Event::Resize(..)) {
            continue;
        }
        let outcome = overlay.handle(&event, env.keys);
        if env.apply(outcome, overlay) {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// The agent <-> screen bridge
// ---------------------------------------------------------------------------

struct Bridge<'a> {
    app: &'a App,
    shell: &'a mut Shell,
    interrupt: Arc<AtomicBool>,
    started: Instant,
    last_paint: Instant,
    failed: bool,
    /// (id, tool name, started) of tool calls awaiting their result, oldest first.
    pending: Vec<(String, String, Instant)>,
    next_id: u64,
}

impl Bridge<'_> {
    fn repaint(&mut self, force: bool) {
        if self.failed || (!force && self.last_paint.elapsed() < PAINT_EVERY) {
            return;
        }
        self.last_paint = Instant::now();
        self.pump();
        self.shell.status.elapsed_secs = self.started.elapsed().as_secs_f64();
        if let Some(monitor) = &self.shell.adapter_monitor {
            self.shell.status.lora = self
                .shell
                .learning
                .cache
                .value
                .2
                .with_runtime(&monitor.lock());
        }
        if self.shell.paint(None).is_err() {
            self.failed = true;
            self.interrupt.store(true, Ordering::SeqCst);
        }
    }

    fn set_activity(&mut self, activity: &str) {
        if self.shell.status.activity != activity {
            self.shell.status.activity = activity.to_owned();
        }
    }

    /// Drain pending keys so Ctrl-C/Esc reach the agent and the next draft stays editable.
    fn pump(&mut self) {
        // A submitted command owns the next idle dispatch. Leave subsequent input in the
        // terminal until the active operation has unwound, rather than overwriting it.
        if self.shell.startup.is_some() {
            return;
        }
        while let Ok(true) = poll(Duration::ZERO) {
            let Ok(event) = read() else { break };
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let shell = &mut *self.shell;
                    match shell.composer_key(key) {
                        ComposerEvent::Action(Action::CommandPalette) => {
                            if shell.route_busy_command(self.app, "/palette".into()) {
                                self.interrupt.store(true, Ordering::SeqCst);
                                return;
                            }
                        }
                        ComposerEvent::Interrupt | ComposerEvent::Exit => {
                            self.interrupt.store(true, Ordering::SeqCst)
                        }
                        ComposerEvent::Submit(text) => {
                            if shell.route_busy_command(self.app, text) {
                                self.interrupt.store(true, Ordering::SeqCst);
                                return;
                            }
                            continue;
                        }
                        ComposerEvent::Action(Action::ModelSelect | Action::ModelRoles) => {
                            shell.route_busy_command(self.app, "/model".into());
                        }
                        ComposerEvent::Action(Action::ToolsExpand) => {
                            shell.tools_expanded = !shell.tools_expanded
                        }
                        ComposerEvent::Action(Action::ToolsToggle) => {
                            shell.ui.show_tools = !shell.ui.show_tools
                        }
                        ComposerEvent::Action(Action::ThinkingToggle) => {
                            shell.ui.show_thinking = !shell.ui.show_thinking
                        }
                        ComposerEvent::Action(Action::EditExternal) => shell.edit_externally(),
                        _ => {}
                    }
                }
                Event::Paste(text) => self.shell.composer_paste(&text),
                Event::Resize(..) => self.shell.term.on_resize(),
                _ => {}
            }
        }
    }
}

impl Ui for Bridge<'_> {
    fn runtime(&mut self, tree: &crate::runtime::Tree) {
        self.shell.runtime = tree.clone();
    }
    fn result_node(&mut self, body: &crate::runtime::Body) {
        let (kind, running) = body.read(|n| {
            (
                n.kind.clone(),
                n.result.phase == crate::runtime::Phase::Running,
            )
        });
        match kind.as_str() {
            "tool" => self.shell.pending_result = running.then(|| body.clone()),
            "assistant" | "reasoning" => {
                self.shell.transcript.bind_prose(body);
                self.shell.shared_prose = running;
            }
            _ => {}
        }
    }
    fn maintenance_ready(&mut self) -> bool {
        self.pump();
        !self.shell.composer.has_queued_work()
    }
    fn take_steering(&mut self) -> Vec<String> {
        self.pump();
        let messages = self.shell.composer.take_steering();
        for message in &messages {
            self.shell.transcript.push_user(message);
        }
        if !messages.is_empty() {
            self.repaint(true);
        }
        messages
    }

    fn context_usage(&mut self, tokens: u64, estimated: bool) {
        self.shell.status.prompt_tokens = Some(tokens);
        self.shell.status.context_estimated = estimated;
        self.repaint(false);
    }
    fn usage(&mut self, _prompt: Option<u64>, output: Option<u64>, seconds: f64) {
        let status = &mut self.shell.status;
        let generation_rate = status
            .model_progress
            .take()
            .filter(|p| !p.estimated)
            .and_then(|p| p.tokens_per_second);
        status.model_silence_secs = 0;
        if let Some(tokens) = output {
            self.shell.turn_output_tokens = self.shell.turn_output_tokens.saturating_add(tokens);
            status.output_tokens = self.shell.turn_output_tokens;
            status.tokens_per_second = generation_rate
                .or_else(|| (seconds > 0.0 && tokens > 0).then_some(tokens as f64 / seconds));
        }
        self.repaint(true);
    }
    fn model_progress(&mut self, progress: &crate::llm::ModelProgress) {
        self.shell.status.model_progress = Some(progress.clone());
        self.shell.status.model_silence_secs = 0;
        self.repaint(false);
    }
    fn reasoning_delta(&mut self, text: &str) {
        if !std::mem::take(&mut self.shell.shared_prose) {
            self.shell.transcript.thinking_delta(text);
        }
        self.set_activity("reasoning");
        self.repaint(false);
    }
    fn tool_arguments_delta(&mut self, _index: u64, _id: &str, name: &str, _delta: &str) {
        self.set_activity(if name.is_empty() {
            "generating tool"
        } else {
            name
        });
        self.repaint(false);
    }
    fn tool_output(&mut self, name: &str, text: &str) {
        if let Some((id, _, _)) = self.pending.iter().rev().find(|(_, tool, _)| tool == name) {
            self.shell.transcript.tool_delta(id, text);
        }
        self.repaint(false);
    }
    fn background_notice(&mut self, message: &str) {
        self.shell.transcript.notice("Background", message);
        self.repaint(true);
    }
    fn cached_usage(&mut self, read: Option<u64>, write: Option<u64>) {
        if read.is_some_and(|n| n != 0) || write.is_some_and(|n| n != 0) {
            self.shell.transcript.notice(
                "Prompt cache",
                &format!(
                    "{} tokens read, {} tokens written",
                    read.map_or("unknown".into(), |n| n.to_string()),
                    write.map_or("unknown".into(), |n| n.to_string())
                ),
            );
            self.repaint(false);
        }
    }
    fn extension_panel(&mut self, title: &str, markdown: &str) {
        self.shell.transcript.notice(title, "");
        self.shell.transcript.assistant_delta(markdown);
        self.shell.transcript.finish_turn();
        self.repaint(true);
    }

    fn role(&mut self, role: &str, model: &str) {
        let status = &mut self.shell.status;
        status.model_progress = None;
        status.model_silence_secs = 0;
        status.role = if role == "main" {
            String::new()
        } else {
            role.to_owned()
        };
        if let Some((provider, name)) = model.split_once('/') {
            status.provider = provider.to_owned();
            status.model = name.to_owned();
        }
        self.repaint(true);
    }

    fn text_delta(&mut self, text: &str) {
        if !std::mem::take(&mut self.shell.shared_prose) {
            self.shell.transcript.assistant_delta(text);
        }
        self.set_activity("writing");
        self.repaint(false);
    }

    fn tool_call(&mut self, name: &str, summary: &str) {
        self.shell.status.model_progress = None;
        self.next_id += 1;
        let id = format!("call-{}", self.next_id);
        self.shell
            .transcript
            .tool_start(&id, name, summary, summary);
        if let Some(body) = self.shell.pending_result.take() {
            self.shell.transcript.bind_tool(&id, body);
        }
        self.pending.push((id, name.to_owned(), Instant::now()));
        self.set_activity(name);
        self.repaint(true);
    }

    fn tool_result(&mut self, name: &str, output: &ToolOutput) {
        let at = self.pending.iter().rposition(|(_, n, _)| n == name);
        let (id, started) = match at {
            Some(i) => {
                let (id, _, started) = self.pending.remove(i);
                (id, started)
            }
            None => {
                self.next_id += 1;
                (format!("call-{}", self.next_id), Instant::now())
            }
        };
        self.shell.transcript.tool_end(
            &id,
            &output.text,
            output.is_error,
            started.elapsed().as_secs_f64(),
        );
        self.set_activity("thinking");
        self.repaint(true);
    }

    fn request_policy(&mut self, control: &Value) {
        self.shell.request_policy = control.clone();
    }
    fn notice(&mut self, message: &str) {
        self.shell.note("Notice", message);
        self.repaint(true);
    }
    /// Policy and index bookkeeping stays out of the transcript; `/policy` shows the same data on demand.
    fn diagnostic(&mut self, _message: &str) {}

    fn image(&mut self, reference: &Value) {
        if self.interrupt.load(Ordering::SeqCst) {
            return;
        }
        let reference = reference.clone();
        let native = self.shell.term.image_protocol().is_some();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("dial-image-preview".into())
            .spawn(move || {
                let _ = tx.send(crate::ui::images::Thumbnail::load_with_native(
                    &reference, native,
                ));
            });
        if worker.is_err() {
            return;
        }
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Ok(thumbnail)) => {
                    self.shell.transcript.image(thumbnail);
                    self.repaint(true);
                    break;
                }
                Ok(Err(error)) => {
                    self.notice(&format!("Terminal image preview unavailable: {error:#}"));
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    self.heartbeat();
                    if self.interrupt.load(Ordering::SeqCst) {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn laya_activity(&mut self, status: &str, message: &str) {
        self.shell.note("Laya", format!("{status} · {message}"));
        self.set_activity(if status == "checking" {
            "Laya checking"
        } else if status == "loading" {
            "Laya loading"
        } else {
            "thinking"
        });
        self.repaint(true);
    }

    fn todo(&mut self, todo: &TodoList) {
        if let Some((id, _, _)) = self
            .pending
            .iter()
            .rev()
            .find(|(_, name, _)| name == "todo")
        {
            self.shell
                .transcript
                .tool_todo(id, tasks::changes(&self.shell.todo, todo));
        }
        self.shell.todo = todo.clone();
        self.repaint(true);
    }

    fn heartbeat(&mut self) {
        self.repaint(false);
    }

    fn model_wait(&mut self, silence: Duration) {
        if self.shell.status.model_progress.is_some() {
            self.shell.status.model_silence_secs = silence.as_secs();
            return;
        }
        if silence.is_zero() {
            self.set_activity("waiting for model");
        } else if silence >= Duration::from_secs(15) {
            self.set_activity(&format!("waiting · no output for {}s", silence.as_secs()));
        }
    }

    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        self.shell.status.phase = Phase::Working;
        let choice = self.shell.ask(
            &format!("Allow {tool}?"),
            &agent::truncate(summary, 1500),
            &[
                ("Allow once", 'y'),
                ("Always allow this session", 'a'),
                ("Deny", 'n'),
            ],
        );
        let decision = match choice {
            Some(0) => Decision::Allow,
            Some(1) => Decision::AllowAlways,
            _ => Decision::Deny,
        };
        self.repaint(true);
        decision
    }

    fn approval_reason(&mut self) -> Option<String> {
        let mut editor = crate::ui::editor::Editor::default();
        loop {
            let (width, _) = self.shell.term.size();
            let theme = self.shell.themes.current();
            let rows = vec![
                Line::from(Span::styled(
                    text::clip(
                        "Plan denied: feedback (Enter sends, Esc skips)",
                        width as usize,
                    ),
                    theme.fg(Tok::Warning),
                )),
                Line::from(text::clip(&text::one_line(&editor.text), width as usize)),
            ];
            if self.shell.paint(Some(rows)).is_err() {
                return None;
            }
            match read().ok()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Enter => {
                        return (!editor.text.trim().is_empty()).then_some(editor.text);
                    }
                    KeyCode::Esc => return None,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return None;
                    }
                    _ => {
                        editor.key(key);
                    }
                },
                Event::Paste(value) => editor.insert(&text::one_line(&value)),
                Event::Resize(..) => self.shell.term.on_resize(),
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Panels, replay
// ---------------------------------------------------------------------------

/// A rounded panel with a question and choices, exactly `width` cells wide on every row.
fn panel_rows(
    theme: &crate::ui::theme::Theme,
    width: u16,
    title: &str,
    body: &str,
    options: &[(&str, char)],
    selected: usize,
) -> Rows {
    let width = width as usize;
    if width < 6 {
        return vec![Line::from(text::clip(title, width))];
    }
    let inner = width - 4;
    let border = theme.fg(Tok::Warning);
    let pad_row = |spans: Vec<Span<'static>>| -> Line<'static> {
        let spans = truncate_spans(spans, inner, "");
        let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
        let mut row = vec![Span::styled(format!("{} ", theme.sym(Sym::BoxV)), border)];
        row.extend(spans);
        row.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
        row.push(Span::styled(format!(" {}", theme.sym(Sym::BoxV)), border));
        Line::from(row)
    };
    let mut rows = Vec::new();
    let title_text = text::ellipsize(
        &text::one_line(title),
        inner.saturating_sub(2),
        theme.ascii(),
    );
    let dashes = width.saturating_sub(text::width(&title_text) + 5);
    rows.push(Line::from(Span::styled(
        format!(
            "{}{} {} {}{}",
            theme.sym(Sym::BoxTl),
            theme.sym(Sym::BoxH),
            title_text,
            theme.sym(Sym::BoxH).repeat(dashes),
            theme.sym(Sym::BoxTr)
        ),
        border,
    )));
    for line in text::wrap_words(&text::sanitize(body), inner)
        .into_iter()
        .take(8)
    {
        rows.push(pad_row(vec![Span::styled(
            text::clip(&line, inner),
            theme.fg(Tok::ToolOutput),
        )]));
    }
    for (index, (label, hotkey)) in options.iter().enumerate() {
        let on = index == selected;
        let marker = if on { theme.sym(Sym::Pointer) } else { " " };
        let style = if on {
            theme
                .on(Tok::Text, Tok::SelectedBg)
                .patch(theme.fg(Tok::Accent))
        } else {
            theme.fg(Tok::Text)
        };
        let label = text::ellipsize(label, inner.saturating_sub(8), theme.ascii());
        rows.push(pad_row(vec![
            Span::styled(format!("{marker} {label}"), style),
            Span::styled(format!("  ({hotkey})"), theme.fg(Tok::Muted)),
        ]));
    }
    rows.push(pad_row(vec![Span::styled(
        "Up/Down choose · Enter confirm · Esc cancel",
        theme.fg(Tok::Dim),
    )]));
    rows.push(Line::from(Span::styled(
        format!(
            "{}{}{}",
            theme.sym(Sym::BoxBl),
            theme.sym(Sym::BoxH).repeat(width - 2),
            theme.sym(Sym::BoxBr)
        ),
        border,
    )));
    rows
}

/// Show an earlier session's conversation as transcript blocks (for `dial -c`).
fn replay(transcript: &mut Transcript, messages: &[Value]) {
    let mut open: HashMap<String, String> = HashMap::new();
    for message in messages {
        match message["role"].as_str() {
            Some("user") => {
                if let Some(text) = message["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    transcript.push_user(text);
                }
            }
            Some("assistant") => {
                if let Some(text) = message["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    transcript.assistant_delta(text);
                    transcript.finish_turn();
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let (id, name) = (
                        call["id"].as_str().unwrap_or("call"),
                        call["function"]["name"].as_str().unwrap_or("tool"),
                    );
                    let args: Value = call["function"]["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(Value::Null);
                    let summary = agent::summarize_call(name, &args);
                    transcript.tool_start(id, name, &summary, &summary);
                    open.insert(id.to_owned(), name.to_owned());
                }
            }
            Some("tool") => {
                let id = message["tool_call_id"].as_str().unwrap_or("");
                if open.remove(id).is_some() {
                    let output = message["content"].as_str().unwrap_or("");
                    let failed = output.starts_with("invalid tool call")
                        || output.starts_with("denied")
                        || output.starts_with("error");
                    transcript.tool_end(id, output, failed, 0.0);
                }
            }
            _ => {}
        }
    }
    transcript.finish_turn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_palette_uses_registered_commands_and_real_turn_policies() {
        use crate::ui::theme::Theme;
        use crossterm::event::{KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let custom = context::CommandDef {
            name: "review-project".into(),
            description: "Review project conventions".into(),
            template: "Review $ARGUMENTS".into(),
            source: "/fixture/review-project.md".into(),
        };
        let skill = skills::Skill {
            name: "web-review".into(),
            description: "Inspect a rendered page".into(),
            path: "/fixture/web-review/SKILL.md".into(),
            source: skills::Scope::Project,
        };
        let entries = palette_entries(&[custom], &[skill]);
        let find = |name: &str| entries.iter().find(|e| e.command.name == name).unwrap();
        assert!(find("undo").during_turn.starts_with("Stops"));
        assert!(find("model").during_turn.starts_with("Opens now"));
        for name in ["jobs", "message", "cancel", "search", "help", "context"] {
            assert!(find(name).during_turn.starts_with("Runs now"), "{name}");
        }
        for name in ["rename", "review-project", "skill:web-review"] {
            assert!(find(name).during_turn.starts_with("Runs after"), "{name}");
        }
        assert_eq!(find("rename").command.args_hint.as_deref(), Some("<title>"));
        let theme = Theme::dark();
        let keys = Keymap::defaults();
        let status = StatusData::default();
        let preview = |_: &UiConfig, _: u16| Line::default();
        let ctx = DrawCtx {
            theme: &theme,
            keys: &keys,
            status: &status,
            preview_status: &preview,
        };
        let mut palette = PaletteOverlay::new(entries, true);
        palette.handle(&Event::Paste("model".into()), &keys);
        let mut terminal = Terminal::new(TestBackend::new(110, 26)).unwrap();
        terminal.draw(|frame| palette.draw(frame, &ctx)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(screen.contains("turn running"));
        assert!(screen.contains("Opens now"));
        assert!(screen.contains("Alt+M"));
        // Optional visual artifact from the real renderer and command registry, for offscreen QA.
        if let Ok(path) = std::env::var("DIAL_TEST_PALETTE_CAPTURE") {
            let cells: Vec<_> = buffer
                .content
                .iter()
                .map(|cell| {
                    json!({
                        "symbol":cell.symbol(), "fg":format!("{:?}", cell.fg),
                        "bg":format!("{:?}", cell.bg), "modifiers":format!("{:?}",cell.modifier),
                    })
                })
                .collect();
            std::fs::write(
                path,
                json!({"width":110,"height":26,"cells":cells}).to_string(),
            )
            .unwrap();
        }
        assert!(
            matches!(palette.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &keys),
            OverlayEvent::Action(action) if action == "command:/model")
        );
        palette.handle(
            &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &keys,
        );
        palette.handle(&Event::Paste("review-project".into()), &keys);
        assert!(
            matches!(palette.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &keys),
            OverlayEvent::Action(action) if action == "command:/review-project")
        );
    }

    #[test]
    fn only_commands_meant_to_end_the_turn_stop_it() {
        for line in [
            "/exit",
            "/new",
            "/clear",
            "/resume abc",
            "/fork",
            "/undo",
            "/rollback 2",
            "/compact",
            "/plan on",
        ] {
            assert!(Shell::stops_turn(line), "{line}");
        }
        for line in [
            "/model",
            "/settings",
            "/approval auto",
            "/theme nord",
            "/cost",
            "/jobs",
            "/my-custom",
            "/skill:review x",
            "/export",
        ] {
            assert!(
                !Shell::stops_turn(line),
                "{line} must not interrupt a running turn"
            );
        }
    }

    #[test]
    fn context_model_picker_auto_masks_inherited_caps_and_preserves_family_defaults() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let set = |path: &str, value: toml::Value| Change::Set {
            path: path.into(),
            value,
        };
        let base = settings_store::save_config(
            &layout,
            Scope::Global,
            &[
                set("context_window", 272000.into()),
                set("cloud_context_window", 64000.into()),
                set("local_context_window", 8192.into()),
                set("model_roles.subagent.provider", "anthropic".into()),
                set("model_roles.subagent.model", "claude-opus-5-5".into()),
                set("model_roles.subagent.context_window", 200000.into()),
            ],
        )
        .unwrap();
        let global = std::fs::read(&layout.config_files()[0]).unwrap();
        let model = ModelSelection {
            provider: "anthropic".into(),
            model: "claude-opus-5-5".into(),
            base_url: Some(crate::llm::anthropic::BASE_URL.into()),
            context_window: Some(1000000),
            ..Default::default()
        };
        let selected = settings_store::save_config(
            &layout,
            Scope::Project,
            &selection_changes(&base, &model, "main").unwrap(),
        )
        .unwrap();
        assert_eq!(selected.context_window, 64000);
        assert_eq!(
            selected.context_limit,
            crate::config::types::ContextWindow::Auto
        );
        let selected = settings_store::save_config(
            &layout,
            Scope::Project,
            &selection_changes(&selected, &model, "subagent").unwrap(),
        )
        .unwrap();
        assert_eq!(selected.for_role("subagent").context_window, 64000);
        assert_eq!(std::fs::read(&layout.config_files()[0]).unwrap(), global);
        let reloaded = Config::load(&layout, None).unwrap();
        assert_eq!(reloaded.context_window, 64000);
        assert_eq!(reloaded.for_role("subagent").context_window, 64000);
    }

    #[test]
    fn switching_models_clears_an_inherited_profile_without_deleting_it() {
        use crate::llm::{chatgpt, profiles::ModelProfile};
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let profile = ModelProfile {
            provider: "chatgpt".into(),
            model: "old-model".into(),
            ..Default::default()
        };
        let config = settings_store::save_config(
            &layout,
            Scope::Global,
            &[
                Change::Set {
                    path: "provider".into(),
                    value: "chatgpt".into(),
                },
                Change::Set {
                    path: "model".into(),
                    value: "old-model".into(),
                },
                Change::Set {
                    path: "model_profiles.old".into(),
                    value: toml::Value::try_from(profile).unwrap(),
                },
                Change::Set {
                    path: "model_profile".into(),
                    value: "old".into(),
                },
            ],
        )
        .unwrap();
        let before = std::fs::read(layout.config_files()[0].clone()).unwrap();
        let selection = ModelSelection {
            provider: "chatgpt".into(),
            model: "new-model".into(),
            base_url: Some(chatgpt::BASE_URL.into()),
            ..Default::default()
        };
        let saved = settings_store::save_config(
            &layout,
            Scope::Project,
            &selection_changes(&config, &selection, "main").unwrap(),
        )
        .unwrap();
        assert_eq!(saved.model, "new-model");
        assert!(saved.model_profile.is_none());
        assert!(saved.model_profiles.contains_key("old"));
        assert_eq!(
            std::fs::read(layout.config_files()[0].clone()).unwrap(),
            before
        );
    }
    #[test]
    fn chatgpt_reasoning_controls_create_and_persist_an_exact_model_profile() {
        use crate::llm::{chatgpt, profiles::ReasoningEffort};
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        chatgpt::test_account(&layout.user_dir, false);
        chatgpt::test_catalog(
            &layout.user_dir,
            &serde_json::json!({"models":[{"slug":"account-model","visibility":"list","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"max"},{"effort":"ultra"}]}]}),
        );
        settings_store::save_config(
            &layout,
            Scope::Global,
            &[
                Change::Set {
                    path: "provider".into(),
                    value: "chatgpt".into(),
                },
                Change::Set {
                    path: "model".into(),
                    value: "account-model".into(),
                },
            ],
        )
        .unwrap();
        let config = Config::load(&layout, None).unwrap();
        assert!(config.model_profile.is_none());
        let (changes, label) = reasoning_changes(&config, "ultra").unwrap();
        assert_eq!(label, "ultra");
        let saved = settings_store::save_config(&layout, Scope::Project, &changes).unwrap();
        assert_eq!(
            saved.active_model_profile().unwrap().reasoning_effort,
            Some(ReasoningEffort::Ultra)
        );
        assert_eq!(
            Config::load(&layout, None)
                .unwrap()
                .active_model_profile()
                .unwrap()
                .reasoning_effort,
            Some(ReasoningEffort::Ultra)
        );
        assert_eq!(
            thinking_label(&reasoning_profile(&saved).unwrap().unwrap()),
            "ultra"
        );
        assert!(
            reasoning_changes(&saved, "off")
                .err()
                .unwrap()
                .to_string()
                .contains("supported reasoning levels")
        );
        assert!(reasoning_changes(&saved, "minimal").is_err());
        let (changes, _) = reasoning_changes(&saved, "high").unwrap();
        let saved = settings_store::save_config(&layout, Scope::Project, &changes).unwrap();
        assert_eq!(
            saved.active_model_profile().unwrap().reasoning_effort,
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            thinking_label(&reasoning_profile(&saved).unwrap().unwrap()),
            "high"
        );
        let switched = saved.with_selection(&ModelSelection {
            provider: "chatgpt".into(),
            model: "other-model".into(),
            base_url: Some(chatgpt::BASE_URL.into()),
            ..Default::default()
        });
        assert!(reasoning_profile(&switched).unwrap().is_none());
    }

    #[test]
    fn thinking_status_distinguishes_toggle_default_and_unsupported() {
        use crate::llm::profiles::{
            ModelCapabilities, ModelProfile, ReasoningApi, ReasoningEffort,
        };
        let mut profile = ModelProfile {
            capabilities: ModelCapabilities {
                reasoning: ReasoningApi::AnthropicAdaptive,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(thinking_label(&profile), "default");
        profile.thinking = Some(true);
        assert_eq!(thinking_label(&profile), "on");
        profile.reasoning_effort = Some(ReasoningEffort::High);
        assert_eq!(thinking_label(&profile), "high");
        profile.thinking = Some(false);
        assert_eq!(thinking_label(&profile), "off");
        profile.capabilities.reasoning = ReasoningApi::None;
        assert_eq!(thinking_label(&profile), "unsupported");
    }

    #[test]
    fn existing_budgeted_thinking_toggle_keeps_its_budget_without_effort_levels() {
        use crate::llm::profiles::{ModelCapabilities, ModelProfile, ReasoningApi};
        let mut config = Config {
            provider: Some("anthropic".into()),
            model: "exact-model".into(),
            model_profile: Some("custom".into()),
            ..Default::default()
        };
        config.model_profiles.insert(
            "custom".into(),
            ModelProfile {
                provider: "anthropic".into(),
                model: "exact-model".into(),
                capabilities: ModelCapabilities {
                    reasoning: ReasoningApi::AnthropicBudget,
                    ..Default::default()
                },
                thinking: Some(false),
                thinking_budget: Some(1024),
                ..Default::default()
            },
        );
        for arg in ["on", "off"] {
            let (changes, _) = reasoning_changes(&config, arg).unwrap();
            let Change::Set { value, .. } = &changes[0] else {
                panic!("profile must be saved")
            };
            let updated: ModelProfile = value.clone().try_into().unwrap();
            assert_eq!(updated.thinking, Some(arg == "on"));
            assert_eq!(updated.thinking_budget, Some(1024));
            updated.validate().unwrap();
        }
    }

    #[test]
    fn pending_status_queries_do_not_block_or_spawn_more_workers() {
        let (release, wait) = std::sync::mpsc::channel();
        let mut cache = StatusCache::<String>::default();
        let start = Instant::now();
        cache.refresh(Duration::from_secs(5), move || {
            wait.recv().unwrap();
            "ready".into()
        });
        for _ in 0..100 {
            cache.refresh(Duration::ZERO, || {
                panic!("a second query must not start while one is pending")
            });
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(cache.value.is_empty());
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cache.worker.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        cache.refresh(Duration::from_secs(5), || {
            panic!("fresh values must stay cached")
        });
        assert_eq!(cache.value, "ready");
        assert!(cache.worker.is_none());
    }
    use crate::ui::theme::Theme;

    fn config() -> (tempfile::TempDir, tempfile::TempDir, Layout, Config) {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
        let config = Config::load(&layout, None).unwrap();
        (home, project, layout, config)
    }

    #[test]
    fn browsing_model_catalog_reads_cached_lora_without_starting_python() {
        use std::os::unix::fs::PermissionsExt;
        let (_home, project, layout, mut config) = config();
        config.endpoint = None;
        config.provider = Some("mlx".into());
        let python = project.path().join("unexpected-python");
        std::fs::write(&python, "#!/bin/sh\nprintf ran > \"$0.called\"\nexit 1\n").unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
        config.python = python;
        let mut paths = Vec::new();
        for (name, supported) in [("main-model", true), ("role-model", false)] {
            let path = project.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            let selection = ModelSelection {
                model: path.to_string_lossy().into_owned(),
                ..Default::default()
            };
            let selected = config.with_selection(&selection);
            let store = Store::open(
                &selected.learning_dir(&layout),
                &selected.model,
                &selected.use_case,
                &selected.harness,
            )
            .unwrap();
            store
                .set_meta(
                    "serving_capabilities",
                    &json!({"inference":true, "lora":supported, "lora_probe_version":2})
                        .to_string(),
                )
                .unwrap();
            paths.push(selection);
        }
        config.model = paths[0].model.clone();
        config
            .model_roles
            .insert("reviewer".into(), paths[1].clone());
        let unverified = project.path().join("unverified-role-model");
        std::fs::create_dir_all(&unverified).unwrap();
        config.model_roles.insert(
            "writer".into(),
            ModelSelection {
                model: unverified.to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        let app = App {
            layout,
            config,
            overlay: toml::Table::new(),
            store: None,
            store_error: None,
            interrupt: Arc::new(AtomicBool::new(false)),
            compute: RefCell::new(None),
        };
        let mut catalog = Catalog::new();
        catalog.refresh(&app);
        assert_eq!(
            catalog.request, 0,
            "config refresh must preserve lazy discovery"
        );
        assert!(catalog.worker.is_none());
        assert!(catalog.get().is_none());
        catalog.start(&app);
        let deadline = Instant::now() + Duration::from_secs(5);
        while catalog.worker.is_some() {
            catalog.collect();
            assert!(Instant::now() < deadline, "catalog scan did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        for (selection, supported) in paths.iter().zip([true, false]) {
            let discovery = catalog.get().unwrap();
            let entry = discovery
                .models
                .iter()
                .find(|entry| entry.selection.model == selection.model)
                .unwrap();
            assert!(entry.metadata.lora_verified);
            assert_eq!(entry.metadata.lora, Some(supported));
        }
        let discovery = catalog.get().unwrap();
        let entry = discovery
            .models
            .iter()
            .find(|entry| entry.selection.model == unverified.to_string_lossy())
            .unwrap();
        assert!(!entry.metadata.lora_verified);
        // Repeated idle/catalog visits reuse the scan and never create model workers.
        for _ in 0..10 {
            catalog.start(&app);
            catalog.collect();
        }
        assert!(!project.path().join("unexpected-python.called").exists());
        assert!(!app.layout.user_dir.join("compute-processes").exists());
    }

    #[test]
    fn lora_acceptance_is_not_runtime_use_and_disabling_training_does_not_hide_it() {
        let (_h, _p, _layout, mut config) = config();
        config.endpoint = None;
        config.model = "test-model".into();
        config.use_case = "test".into();
        let store = Store::open(
            &config.state_dir,
            &config.model,
            &config.use_case,
            &config.harness,
        )
        .unwrap();
        store
            .set_meta(
                "serving_capabilities",
                "{\"lora\":true,\"lora_probe_version\":2}",
            )
            .unwrap();
        let mut active = store.active().unwrap();
        assert_eq!(
            lora_state(&config, Some(&store), Some(&active)),
            Lora::Ready
        );
        active.adapter = Some("/adapter".into());
        config.training.enabled = false;
        assert_eq!(
            lora_state(&config, Some(&store), Some(&active)),
            Lora::Active(active.id)
        );
        use crate::llm::provider::{AdapterPhase, AdapterRuntime};
        let accepted = Lora::Active(active.id);
        assert_eq!(accepted.with_runtime(&AdapterRuntime::default()), accepted);
        assert_eq!(
            accepted.with_runtime(&AdapterRuntime {
                adapter: Some("/adapter".into()),
                phase: AdapterPhase::Used
            }),
            Lora::Used
        );
        assert_eq!(
            Lora::Training.with_runtime(&AdapterRuntime {
                adapter: Some("/adapter".into()),
                phase: AdapterPhase::Used
            }),
            Lora::Training
        );
        assert_eq!(
            Lora::Ready.with_runtime(&AdapterRuntime {
                adapter: None,
                phase: AdapterPhase::Used
            }),
            Lora::Ready
        );
    }

    #[test]
    fn idle_handoff_releases_compute_without_closing_the_session() {
        let (_h, _p, layout, mut config) = config();
        let model = layout.user_dir.join("test-model");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("config.json"), "{}").unwrap();
        std::fs::write(model.join("model.safetensors"), "weights").unwrap();
        config.model = model.to_string_lossy().into_owned();
        config.use_case = "test".into();
        let lock_path = layout.user_dir.join("compute.lock");
        let lock = auto::lock_file(&lock_path).unwrap();
        lock.try_lock_exclusive().unwrap();
        let app = App {
            layout,
            config,
            overlay: toml::Table::new(),
            store: None,
            store_error: None,
            interrupt: Arc::new(AtomicBool::new(false)),
            compute: RefCell::new(Some(lock)),
        };
        let session = Session::create(
            &app.layout,
            SessionMeta {
                model: "test-model".into(),
            },
        )
        .unwrap();
        let mut agent = app.make_agent(session).unwrap();
        let id = agent.session().id().to_owned();
        let worker = auto::lock_file(&lock_path).unwrap();
        assert_eq!(
            worker.try_lock_exclusive().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let inherited = app.compute.borrow().as_ref().unwrap().try_clone().unwrap();
        assert!(app.release_idle_compute(&mut agent));
        worker.try_lock_exclusive().unwrap();
        drop(inherited);
        assert!(app.compute.borrow().is_none());
        assert_eq!(agent.session().id(), id);
        assert!(
            agent.adapter_monitor().is_some(),
            "backend remains available for lazy reload"
        );
    }

    #[test]
    fn idle_unload_stops_a_resident_server_without_a_background_request() {
        if crate::learn::processes::live_inspection_unavailable() {
            return;
        }
        let (_h, _p, layout, mut config) = config();
        std::fs::create_dir_all(&layout.user_dir).unwrap();
        let python = crate::llm::server::tests::fake_python(&layout.user_dir);
        config.python = python.clone();
        config.endpoint = None;
        config.provider = Some("mlx".into());
        config.model = layout
            .user_dir
            .join("fake-model-pid")
            .to_string_lossy()
            .into_owned();
        config.improve.enabled = false;
        let mut backend = crate::llm::provider::Backend::new(
            crate::llm::provider::ProviderKind::Local {
                model_dir: config.model.clone(),
            },
            python,
            5,
        )
        .with_steering(&config);
        let reply = backend
            .chat(
                None,
                &crate::llm::Chat {
                    messages: vec![json!({"role":"user","content":"hello"})],
                    tools: vec![],
                    temperature: 0.0,
                    max_tokens: 16,
                    seed: None,
                },
                5,
                None,
            )
            .unwrap();
        assert_eq!(reply.text, "pong");
        let marker: Value = serde_json::from_slice(&std::fs::read(&config.model).unwrap()).unwrap();
        let pid = marker["pid"].as_i64().unwrap() as i32;
        let lock = auto::lock_file(&layout.user_dir.join("compute.lock")).unwrap();
        lock.try_lock_exclusive().unwrap();
        let app = App {
            layout,
            config,
            overlay: toml::Table::new(),
            store: None,
            store_error: None,
            interrupt: Arc::new(AtomicBool::new(false)),
            compute: RefCell::new(Some(lock)),
        };
        let session = Session::create(
            &app.layout,
            SessionMeta {
                model: app.config.model.clone(),
            },
        )
        .unwrap();
        let id = session.id().to_owned();
        let mut agent = Agent::interactive(
            &app.config,
            backend,
            app.layout.clone(),
            session,
            None,
            None,
            app.interrupt.clone(),
        )
        .unwrap();
        assert!(!crate::learn::compute::background_pending(&app.layout.user_dir).unwrap());
        assert!(
            crate::learn::processes::snapshot(&app.layout.user_dir)
                .unwrap()
                .iter()
                .any(|row| row["pid"] == pid)
        );
        assert!(app.release_idle_compute(&mut agent));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert!(app.compute.borrow().is_none());
        assert_eq!(agent.session().id(), id);
        assert!(
            agent.adapter_monitor().is_some(),
            "backend remains available for the next turn"
        );
    }
    #[test]
    fn completed_probe_releases_compute_even_when_a_handle_was_inherited() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("compute.lock");
        let lock = auto::lock_file(&path).unwrap();
        lock.try_lock_exclusive().unwrap();
        let lease = ProbeLease(lock);
        let inherited = lease.0.try_clone().unwrap();
        drop(lease);
        let next = auto::lock_file(&path).unwrap();
        next.try_lock_exclusive().unwrap();
        drop(inherited);
    }

    #[test]
    fn learning_status_reads_the_selected_model_store_not_the_project_store() {
        let (_h, _p, layout, mut config) = config();
        config.model = "test-model".into();
        config.use_case = "test".into();
        config.endpoint = None;
        let project_store = Store::open(
            &layout.state_dir,
            &config.model,
            &config.use_case,
            &config.harness,
        )
        .unwrap();
        project_store
            .set_meta(
                "serving_capabilities",
                "{\"lora\":true,\"lora_probe_version\":2}",
            )
            .unwrap();
        config.state_dir = layout.state_dir.join("models/selected");
        let store = Store::open(
            &config.state_dir,
            &config.model,
            &config.use_case,
            &config.harness,
        )
        .unwrap();
        store
            .set_meta(
                "serving_capabilities",
                "{\"lora\":false,\"lora_probe_version\":2,\"reason\":\"fused QKV unsupported\"}",
            )
            .unwrap();
        store
            .set_meta("auto_status", "{\"state\":\"running\"}")
            .unwrap();
        store
            .set_meta("cycle_progress", "{\"stage\":\"self-play 5/24\"}")
            .unwrap();
        let app = App {
            layout,
            config,
            overlay: toml::Table::new(),
            store: Some(store),
            store_error: None,
            interrupt: Arc::new(AtomicBool::new(false)),
            compute: RefCell::new(None),
        };
        let mut learning = Learning::default();
        learning.read(&app);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !learning.cache.worker.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        learning.read(&app);
        assert_eq!(learning.cache.value.2, Lora::Unsupported);
        assert!(learning.cache.value.1.contains("self-play 5/24"));
        assert!(improve_summary(&app).contains("fused QKV unsupported"));
    }

    #[test]
    fn only_agent_relevant_changes_make_the_controller_rebuild_the_agent() {
        let (_h, _p, _l, base) = config();
        let mut ui = base.clone();
        ui.ui.theme = "light".into();
        ui.ui.status_separator = crate::llm::model_config::StatusSeparator::Slash;
        ui.approval = Approval::Auto;
        assert_eq!(
            agent_key(&base),
            agent_key(&ui),
            "theme, status line and approval apply in place"
        );
        for mutate in [
            (|c: &mut Config| c.model = "other".into()) as fn(&mut Config),
            |c| c.sandbox.allow_network = !c.sandbox.allow_network,
            |c| c.context_window += 1,
            |c| c.improve.enabled = !c.improve.enabled,
        ] {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert_ne!(agent_key(&base), agent_key(&changed));
        }
    }

    #[test]
    fn selecting_a_local_model_clears_the_remote_endpoint_and_a_new_remote_adds_its_provider() {
        let (_h, _p, _l, config) = config();
        let local = ModelSelection {
            provider: "mlx".into(),
            model: "mlx-community/Qwen".into(),
            ..ModelSelection::default()
        };
        let changes = selection_changes(&config, &local, "main").unwrap();
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, Change::Remove { path } if path == "endpoint")),
            "a local model must not keep an inherited endpoint"
        );
        assert!(changes.iter().any(|c| matches!(c, Change::Set { path, value } if path == "provider" && value.as_str() == Some("mlx"))));
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, Change::Set { path, value } if path == "context_window" && value.as_str() == Some("auto"))),
            "a model without a known window must not keep the previous model's"
        );
        let remote = ModelSelection {
            provider: "lm-studio".into(),
            model: "qwen".into(),
            base_url: Some("http://127.0.0.1:1234/v1".into()),
            context_window: Some(8192),
            ..ModelSelection::default()
        };
        let changes = selection_changes(&config, &remote, "main").unwrap();
        let sets: Vec<&str> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Set { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        for expected in [
            "model",
            "endpoint.base_url",
            "providers.lm-studio.base_url",
            "context_window",
        ] {
            assert!(sets.contains(&expected), "{expected} missing from {sets:?}");
        }
        let helper = selection_changes(&config, &remote, "subagent").unwrap();
        assert!(
            helper.iter().all(|c| match c {
                Change::Set { path, .. } | Change::Remove { path } =>
                    path.starts_with("model_roles.subagent."),
            }),
            "a helper role must not touch the main model"
        );
        assert!(selection_changes(&config, &remote, "bogus").is_err());
        assert!(
            selection_changes(&config, &ModelSelection::default(), "main").is_err(),
            "an empty model id is refused"
        );
    }

    #[test]
    fn clearing_a_role_in_the_project_layer_overrides_a_global_assignment() {
        assert!(
            matches!(&clear_role_changes("feedback", Scope::Global).unwrap()[..], [Change::Remove { path }] if path == "model_roles.feedback")
        );
        let project = clear_role_changes("feedback", Scope::Project).unwrap();
        assert!(project.iter().any(|c| matches!(c, Change::Set { path, value } if path == "model_roles.feedback.provider" && value.as_str() == Some("inherit"))));
        assert!(clear_role_changes("main", Scope::Global).is_err());
    }

    #[test]
    fn architecture_default_is_shown_and_clearing_it_follows_main() {
        let config = Config::default();
        let rows = role_rows(&config);
        let architecture = rows
            .iter()
            .find(|(role, _)| role == "architecture")
            .unwrap()
            .1
            .as_ref()
            .unwrap();
        assert_eq!(architecture.model, config.codebase.architecture_model);
        assert_eq!(architecture.provider, "mlx");
        for scope in [Scope::Global, Scope::Project] {
            assert!(clear_role_changes("architecture", scope).unwrap().iter().any(|change|
                matches!(change, Change::Set { path, value } if path == "codebase.architecture_model" && value.as_str() == Some(""))));
        }
    }

    #[test]
    fn a_global_role_with_a_project_inherit_marker_resolves_to_the_main_model() {
        let (_h, _p, layout, _) = config();
        settings_store::save_config(
            &layout,
            Scope::Global,
            &[
                Change::Set {
                    path: "model".into(),
                    value: "main-model".into(),
                },
                Change::Set {
                    path: "model_roles.feedback.provider".into(),
                    value: "mlx".into(),
                },
                Change::Set {
                    path: "model_roles.feedback.model".into(),
                    value: "helper".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            Config::load(&layout, None).unwrap().model_roles["feedback"].model,
            "helper"
        );
        settings_store::save_config(
            &layout,
            Scope::Project,
            &clear_role_changes("feedback", Scope::Project).unwrap(),
        )
        .unwrap();
        let after = Config::load(&layout, None).unwrap();
        assert!(!after.model_roles.contains_key("feedback"));
        assert_eq!(after.for_role("feedback").model, "main-model");
    }

    #[test]
    fn command_menu_lists_builtins_then_custom_then_skills_and_builtins_win() {
        let custom = vec![
            context::CommandDef {
                name: "review".into(),
                description: "review the diff".into(),
                template: String::new(),
                source: Default::default(),
            },
            context::CommandDef {
                name: "help".into(),
                description: "shadow".into(),
                template: String::new(),
                source: Default::default(),
            },
            context::CommandDef {
                name: "models".into(),
                description: "old alias".into(),
                template: String::new(),
                source: Default::default(),
            },
        ];
        let skill = skills::Skill {
            name: "lint".into(),
            description: "run linters".into(),
            path: Default::default(),
            source: skills::Scope::Project,
        };
        let entries = command_entries(&custom, &[skill]);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names.iter().filter(|n| **n == "help").count(),
            1,
            "a custom command cannot shadow a built-in"
        );
        assert!(
            !names.contains(&"models"),
            "retired aliases are not offered"
        );
        let at = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(at("exit") < at("review") && at("review") < at("skill:lint"));
        assert!(
            matches!(entries[at("review")].kind, CommandKind::Custom)
                && matches!(entries[at("skill:lint")].kind, CommandKind::Skill)
        );
        assert_eq!(entries[at("resume")].args_hint.as_deref(), Some("[id]"));
    }

    #[test]
    fn command_argument_choices_include_subcommands_flags_and_installed_skill_names() {
        let skill = skills::Skill {
            name: "review".into(),
            description: "Review".into(),
            path: Default::default(),
            source: skills::Scope::User,
        };
        let choices = command_argument_completions(&Config::default(), &[skill]);
        for expected in [
            "/understanding architecture",
            "/understanding build",
            "/skills sources",
            "/skills list",
            "/skills import --dry-run",
            "/skills enable review",
            "/skills disable review --user",
            "/approval readonly",
            "/plan off",
            "/improve status",
            "/improve jobs",
            "/improve stop",
            "/improve cycle",
            "/improve enable",
            "/improve disable",
            "/improve versions",
            "/improve tasks",
            "/experience ledger",
            "/theme auto",
        ] {
            assert!(
                choices.iter().any(|choice| choice == expected),
                "missing {expected}"
            );
        }
        assert!(
            choices.iter().all(|choice| !choice.contains('<')),
            "never insert usage placeholders"
        );
        assert!(choices.iter().all(|choice| {
            BUILTIN_COMMANDS
                .iter()
                .any(|(name, _)| choice.starts_with(&format!("/{name} ")))
        }));
    }

    #[test]
    fn numstat_parsing_keeps_binary_counts_and_renames_and_unusual_paths() {
        let files = parse_file_diffs(
            "12\t3\tsrc/with\ttab.rs\0-\t-\timage.png\02\t1\t\0old name.rs\0new name.rs\0",
        );
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "image.png");
        assert_eq!(files[0].added, None);
        assert_eq!(files[1].path, "new name.rs");
        assert_eq!((files[1].added, files[1].removed), (Some(2), Some(1)));
        assert_eq!(files[2].path, "src/with\ttab.rs");
    }

    #[test]
    fn git_status_counts_staged_modified_and_untracked() {
        assert_eq!(
            parse_status("M  a\n M b\nMM c\n?? d\n?? e\nA  f\n"),
            (3, 2, 2)
        );
        assert_eq!(parse_status(""), (0, 0, 0));
    }

    #[test]
    fn terminal_background_is_read_from_colorfgbg() {
        assert_eq!(terminal_dark(Some("15;0")), Some(true));
        assert_eq!(terminal_dark(Some("0;15")), Some(false));
        assert_eq!(terminal_dark(Some("0;default;15")), Some(false));
        assert_eq!(terminal_dark(Some("garbage")), None);
        assert_eq!(terminal_dark(None), None);
    }

    #[test]
    fn local_time_formats_like_strftime() {
        let clock = local_time("%H:%M");
        assert_eq!(clock.len(), 5);
        assert_eq!(clock.as_bytes()[2], b':');
        assert_eq!(local_time("%Y-%m-%d %H:%M:%S").len(), 19);
    }

    #[test]
    fn the_approval_panel_fits_any_width_and_shows_the_choice() {
        let theme = Theme::dark();
        let options = [
            ("Allow once", 'y'),
            ("Always allow this session", 'a'),
            ("Deny", 'n'),
        ];
        for width in [0u16, 1, 3, 4, 12, 40, 100] {
            let rows = panel_rows(
                &theme,
                width,
                "Allow bash?",
                "rm -rf build && cargo build 日本語",
                &options,
                1,
            );
            assert!(
                rows.iter().all(|r| text::line_width(r) <= width as usize),
                "width {width}"
            );
        }
        let rows = panel_rows(&theme, 60, "Allow bash?", "cargo test", &options, 1);
        assert_eq!(
            rows.len(),
            1 + 1 + 3 + 1 + 1,
            "title, body, options, hint, bottom"
        );
        assert!(rows.iter().all(|r| text::line_width(r) == 60));
        let selected: Vec<String> = rows
            .iter()
            .map(|r| r.to_string())
            .filter(|s| s.contains(theme.sym(Sym::Pointer)))
            .collect();
        assert_eq!(selected.len(), 1);
        assert!(
            selected[0].contains("Always allow this session"),
            "{selected:?}"
        );
    }

    #[test]
    fn effective_config_serializes_for_the_settings_overlay() {
        let (_h, _p, _l, config) = config();
        let table = toml::Table::try_from(&config).expect("Config must serialize to TOML");
        assert_eq!(
            settings_schema::get(&table, "ui.theme").and_then(toml::Value::as_str),
            Some("dark")
        );
        assert!(settings_schema::get(&table, "improve.interval_secs").is_some());
        assert!(settings_schema::get(&table, "sandbox.mode").is_some());
    }

    #[test]
    fn initial_discovery_publishes_and_a_stale_refresh_cannot_replace_the_newest() {
        fn result(id: &str) -> catalog::Discovery {
            catalog::Discovery {
                models: vec![],
                providers: vec![catalog::ProviderStatus {
                    id: id.into(),
                    label: id.into(),
                    base_url: None,
                    state: "reachable".into(),
                    detail: String::new(),
                    models: 0,
                }],
            }
        }
        let mut catalog = Catalog::new();
        catalog.launch(|| result("first"));
        catalog.worker.take().unwrap().join().unwrap();
        assert_eq!(catalog.get().unwrap().providers[0].id, "first");
        let (release, wait) = std::sync::mpsc::channel();
        catalog.launch(move || {
            wait.recv().unwrap();
            result("stale")
        });
        let stale = catalog.worker.take().unwrap();
        catalog.launch(|| result("newest"));
        catalog.worker.take().unwrap().join().unwrap();
        release.send(()).unwrap();
        stale.join().unwrap();
        assert_eq!(catalog.get().unwrap().providers[0].id, "newest");
    }
}
