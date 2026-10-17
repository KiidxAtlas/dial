//! Interactive controller: owns the terminal, the conversation view, the composer and the overlays, and
//! runs agent turns. Rendering is inline (scrollback-native, see `ui::term`): finished messages are
//! committed to the terminal's own scrollback, and only the live tail and the composer are repainted.
//!
//! Configuration is never edited here. Overlays emit semantic events (`OverlayEvent`); this module
//! persists them through `settings_store` (which validates the merged configuration before writing),
//! reloads the effective config, and applies the result at a safe boundary. External edits retain
//! last-good layers on failure; changes found during a turn are staged. Adoption rebuilds the
//! immutable agent configuration around the same session, task list, permissions and cost.
use crate::platform;

#[path = "commands/mod.rs"]
mod commands;

use crate::agent::todo::TodoList;
use crate::agent::{self, Agent, Decision, Ui, context, make_backend};
use crate::config::paths::Layout;
use crate::config::selection::{changes as selection_changes, protocol as selection_protocol};
use crate::config::session_models::SessionModels;
use crate::config::settings_store::{self, Change, Scope};
use crate::config::types::{Approval, Config, Outcome};
use crate::learn::auto;
use crate::llm::model_config::{ModelSelection, ProviderSpec, ROLES, UiConfig};
use crate::llm::{catalog, usage};
use crate::repl::{OutputFormat, PrintExit, PrintUi};
use crate::runtime::tool_output::ToolOutput;
use crate::storage::session::{Session, SessionMeta};
use crate::storage::skills;
use crate::storage::store::Store;
use crate::ui::composer::{CommandEntry, CommandKind, Composer, ComposerCtx, ComposerEvent};
use crate::ui::keys::{Action, Keymap};
use crate::ui::markdown::truncate_spans;
use crate::ui::overlay::compute::ComputeOverlay;
use crate::ui::overlay::models::ModelOverlay;
use crate::ui::overlay::palette::{ChoiceItem, ChoiceSpec, PaletteEntry, PaletteOverlay};
use crate::ui::overlay::providers::ProviderOverlay;
use crate::ui::overlay::settings::SettingsOverlay;
use crate::ui::overlay::shortcuts::ShortcutOverlay;
use crate::ui::overlay::{DrawCtx, OverlayEvent};
use crate::ui::status::{Lora, Phase, StatusData};
use crate::ui::term::{InlineTerminal, OverlayTerminal, Viewport};
use crate::ui::theme::{Sym, ThemeManager, Tok};
use crate::ui::transcript::{Transcript, TranscriptCtx, TurnStats};
use crate::ui::welcome::{self, WelcomeInfo};
use crate::ui::{Rows, statusline, tasks, text};
use anyhow::{Context, Result, ensure};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, poll, read};
use ratatui::text::{Line, Span};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    cell::RefCell,
    collections::hash_map::DefaultHasher,
    fs::{self, File},
    hash::{Hash, Hasher},
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
/// "Teach this model" watches this many recent turns for a commit of their changes.
const TEACH_PENDING_TURNS: usize = 8;
const TEACH_OFF: &str = "Turn on Teach this model (/settings → Model → Learning) while using a local model to rate turns and teach it from your work.";

#[derive(Clone, PartialEq, Eq)]
struct TeachState {
    model: String,
    /// The setting is on.
    wanted: bool,
    /// The learning store is open, so turns are recorded.
    active: bool,
}

impl TeachState {
    fn of(app: &App) -> Self {
        Self {
            model: app.config.model.clone(),
            wanted: app.config.improve.teach,
            active: app.store.is_some(),
        }
    }
}

/// Where a "Teach this model" turn started: HEAD (when in a repository) and the checkpoint counter.
struct TeachStart {
    base: Option<String>,
    checkpoint: Option<u64>,
}
/// Learning state is a store query, so it is refreshed on a timer, not per keystroke.
const STATUS_TTL: Duration = Duration::from_secs(2);
/// Streaming text repaints at most this often; tool events always repaint.
const PAINT_EVERY: Duration = Duration::from_millis(40);
/// Poll discovery metadata on the UI heartbeat, never read private TOML every frame.
const RESOURCE_TTL: Duration = Duration::from_secs(2);
const RESOURCE_SCAN_CAP: usize = 4096;
const RESOURCE_SCAN_TIME: Duration = Duration::from_millis(100);
/// Heartbeat fallback when `notify` covers every resource input; the watcher marks changes immediately.
const RESOURCE_WATCHED_FALLBACK: Duration = Duration::from_secs(30);
/// The picker loads only the newest sessions; parsing every journal would block the UI thread.
/// Older sessions remain reachable by id through the direct prefix lookup.
const PICKER_SESSION_LIMIT: usize = 200;

pub struct Options {
    pub session: Option<Session>,
    pub prompt: String,
    pub interactive: bool,
    /// Command-line flag overrides, layered on top of the config files every time the config is reloaded.
    pub overlay: toml::Table,
    /// Explicit launch notices for the transcript.
    pub notices: Vec<String>,
    /// `--print-thoughts`: print reasoning to stdout in print text mode.
    pub print_thoughts: bool,
    /// CLI launch overrides (`--tools`, `--skills`, system prompt, `--max-time`) applied to every agent in this process.
    pub launch: crate::agent::modes::Launch,
    /// `--add-dir`: extra workspace roots added to every agent in this process.
    pub add_dirs: Vec<std::path::PathBuf>,
    /// `--plan`, `--prewalk`, and the other mode flags; `drive` applies the startup arm to the first agent.
    pub mode_flags: crate::cli::mode_flags::ModeFlags,
}

/// Run the inline interface, or answer a single piped message.
pub fn run_interactive(layout: Layout, config: Config, options: Options) -> Result<()> {
    if !options.interactive {
        return run_plain(layout, config, options);
    }
    let mut app = App::new(
        layout,
        config,
        options.overlay,
        options.launch,
        options.add_dirs,
        options.mode_flags,
    )?;
    #[cfg(unix)]
    let startup_prompt = options.prompt.clone();
    let mut shell = Shell::new(&app, options.prompt)?;
    for notice in &options.notices {
        shell.transcript.notice("Learning", notice);
    }
    let outcome = drive(&mut app, &mut shell, options.session);
    commands::maintenance::stop_relay();
    let resume = shell.finish();
    #[cfg(unix)]
    if shell.restart_requested && outcome.is_ok() {
        let launch: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let launch = crate::agent::modes::relaunch::without_prompt(&launch, &startup_prompt);
        let args = crate::agent::modes::relaunch::relaunch_args(&launch, resume.as_deref());
        crate::telemetry::shutdown();
        return Err(crate::agent::modes::relaunch::exec(&args).into());
    }
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

/// How long `dial -p` waits, after its turn, for one selected-memory maintenance pass.
const PRINT_MEMORY_BUDGET: Duration = Duration::from_secs(20);

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
    ui.set_print_thoughts(options.print_thoughts);
    config.require_model()?;
    let _compute = if config.requires_compute_lock() {
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
    let app = App::new(
        layout,
        config,
        options.overlay,
        options.launch,
        options.add_dirs,
        options.mode_flags,
    )?;
    // Print runs schedule no detached improvement workers: a one-shot command must
    // not leave processes behind that compete for local compute after it exits.
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
    agent.begin_session_lifecycle();
    ui.set_interrupt(app.interrupt.clone());
    ui.system(
        &session_id,
        &app.config.model,
        &app.layout.project_root,
        &agent.tool_names(),
    );
    for notice in &options.notices {
        ui.notice(notice);
    }
    ui.check_output()?;
    let completed_before = agent.metrics().turns_completed;
    let result = agent.turn(options.prompt.trim(), &mut ui);
    let completed = agent.metrics().turns_completed > completed_before;
    ui.set_completed(completed);
    // A one-shot run has no idle loop: give the selected memory one bounded pass to retain this
    // turn before exit. Success prints nothing.
    if completed && result.is_ok() {
        agent.finish_memory_before_exit(&mut ui, PRINT_MEMORY_BUDGET);
    }
    agent.end_local_session();
    let (total, turn) = agent.usage();
    ui.set_usage_breakdown(total, turn);
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
    /// This session's own model choices: other processes' model writes never replace them.
    models: RefCell<Option<SessionModels>>,
    store: Option<Store>,
    store_error: Option<String>,
    interrupt: Arc<AtomicBool>,
    compute: RefCell<Option<File>>,
    /// CLI launch overrides applied to every agent built here.
    launch: crate::agent::modes::Launch,
    /// `--add-dir` roots added to every agent built here.
    add_dirs: Vec<std::path::PathBuf>,
    /// `--plan` / `--prewalk` startup state; the startup arm runs once for the first agent.
    mode_flags: crate::cli::mode_flags::ModeFlags,
}

impl App {
    fn new(
        layout: Layout,
        config: Config,
        overlay: toml::Table,
        launch: crate::agent::modes::Launch,
        add_dirs: Vec<std::path::PathBuf>,
        mode_flags: crate::cli::mode_flags::ModeFlags,
    ) -> Result<Self> {
        let models = Some(SessionModels::capture(
            &settings_store::load_merged(&layout, &overlay).context("reading session settings")?,
        ));
        let mut app = Self {
            layout,
            config: config.for_coding(),
            overlay,
            models: RefCell::new(models),
            store: None,
            store_error: None,
            interrupt: platform::interrupt::install_sigint(),
            compute: RefCell::new(None),
            launch,
            add_dirs,
            mode_flags,
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
        let resumed = session.additional_dirs().to_vec();
        let mut serving = self.config.clone();
        if let Some(store) = &self.store {
            crate::learn::improve::apply_pinned_model(&mut serving, store)?;
        }
        let backend = crate::agent::make_session_backend(&serving, &session)?;
        let mut agent = Agent::interactive(
            &self.config,
            backend,
            self.layout.clone(),
            session,
            None,
            self.store.as_ref(),
            self.interrupt.clone(),
        )?;
        agent.set_launch(self.launch.clone())?;
        agent.set_launch_modes(self.mode_flags.advisor, self.mode_flags.external_thinking);
        let mut dirs = self.add_dirs.clone();
        for dir in resumed {
            if !dir.is_dir() {
                eprintln!(
                    "dial: skipping missing workspace directory {}",
                    dir.display()
                );
            } else if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        if !dirs.is_empty() {
            agent.add_workspace_roots(&dirs)?;
            agent.persist_workspace_roots()?;
        }
        Ok(agent)
    }

    /// Adopt a freshly loaded, validated configuration and keep the per-model store in step.
    fn adopt(&mut self, config: Config) {
        self.config = config.for_coding();
        if !self.config.requires_compute_lock()
            && let Some(lock) = self.compute.get_mut().take()
        {
            crate::learn::compute::clear_owner(&self.layout.user_dir);
            let _ = fs2::FileExt::unlock(&lock);
        }
        self.reopen_store();
    }

    fn reopen_store(&mut self) {
        // Coding opens the learning store only for "Teach this model"; everything else enters
        // learning through explicit improvement operations.
        self.store = None;
        self.store_error = None;
        if self.config.teaching() {
            match Self::open_store(&self.config) {
                Ok(store) => self.store = Some(store),
                Err(error) => self.store_error = Some(format!("{error:#}")),
            }
        }
    }

    /// The effective configuration as the files and the command-line flags define it right now,
    /// keeping this session's own model choices.
    pub(crate) fn reload(&self) -> Result<Config> {
        settings_store::load_session_config(
            &self.layout,
            &self.overlay,
            self.models.borrow().as_ref(),
        )
    }

    /// Persist a settings write made in this session. Model keys it writes (or the model `role`
    /// it assigns) become this session's model choice; other sessions keep their own.
    pub(crate) fn save_settings(
        &self,
        scope: Scope,
        changes: &[Change],
        role: Option<&str>,
    ) -> Result<()> {
        match role {
            Some(role) => settings_store::save_model_config(&self.layout, scope, changes, role)?,
            None => settings_store::save_config(&self.layout, scope, changes)?,
        };
        if let Some(models) = self.models.borrow_mut().as_mut() {
            let merged = settings_store::load_merged(&self.layout, &self.overlay)
                .context("settings saved, but this session could not re-read them")?;
            models.adopt_paths(
                &merged,
                changes.iter().map(|change| match change {
                    Change::Set { path, .. } | Change::Remove { path } => path.as_str(),
                }),
            );
            if let Some(role) = role {
                models.adopt_role(&merged, role);
            }
        }
        Ok(())
    }

    /// The directory layout this session runs in.
    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
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

/// Where each provider's credential comes from, keyed by base URL: a saved key and/or a named
/// environment variable. Covers discovered built-in endpoints and configured ones; configured wins.
fn credential_sources(
    user_dir: &std::path::Path,
    config: &crate::config::types::Config,
    discovered: &[catalog::ProviderStatus],
) -> std::collections::BTreeMap<String, crate::ui::overlay::providers::CredentialSource> {
    let built_in = discovered
        .iter()
        .filter_map(|p| p.base_url.clone())
        .map(|url| (url, None));
    let configured = config
        .providers
        .values()
        .filter(|spec| !spec.base_url.trim().is_empty())
        .map(|spec| (spec.base_url.clone(), spec.api_key_env.clone()));
    let mut sources = std::collections::BTreeMap::new();
    for (url, env_name) in built_in.chain(configured) {
        let saved = catalog::credential_token(user_dir, &url)
            .ok()
            .flatten()
            .is_some();
        let env_name = env_name.or_else(|| catalog::known_key_env(&url).map(str::to_owned));
        let env = env_name.map(|name| crate::ui::overlay::providers::EnvCredential {
            set: std::env::var(&name).is_ok_and(|value| !value.is_empty()),
            name,
        });
        sources.insert(
            url,
            crate::ui::overlay::providers::CredentialSource { saved, env },
        );
    }
    sources
}

/// The `/memory` status in plain words: what Dial remembers (newest first), then the backend, whether
/// it is working, why not, notes from the backend, and what to do next.
fn memory_status_text(value: &Value) -> String {
    let backend = value.get("backend").and_then(Value::as_str).unwrap_or("");
    let active = value.get("active").and_then(Value::as_bool).unwrap_or(true);
    let (label, noun) = match backend {
        "off" => ("Off", ""),
        "provenance" => ("Remembered facts", "fact"),
        "local" => ("Rollout summaries (local)", "lesson"),
        "hindsight" => ("Hindsight service", ""),
        "sharpshooter" => ("Project decisions", ""),
        "semantic" => ("Semantic facts", "fact"),
        _ => ("unknown", ""),
    };
    let mut lines = Vec::new();
    if active && !noun.is_empty() {
        lines.extend(remembered_lines(value, noun));
        if let Some(user) = value.get("userFacts").and_then(Value::as_u64) {
            lines.push(format!("User-wide facts (all projects): {user}"));
        }
        lines.push(String::new());
    }
    lines.push(format!("Backend: {label}"));
    lines.push(format!("Active: {}", if active { "yes" } else { "no" }));
    let error = value.get("error").and_then(Value::as_str);
    // An inactive backend's generic message repeats the reason below with a fixed next step.
    if (noun.is_empty() || backend == "off")
        && (active || error.is_none())
        && let Some(message) = value.get("message").and_then(Value::as_str)
    {
        lines.push(message.to_owned());
    }
    if let Some(embedding) = value.get("embedding") {
        let state = embedding.get("state").and_then(Value::as_str).unwrap_or("");
        let line = match (state, embedding.get("model").and_then(Value::as_str)) {
            ("on", Some(model)) => match embedding.get("embedded").and_then(Value::as_u64) {
                Some(embedded) => format!("Embeddings: on ({model}, {embedded} facts embedded)"),
                None => format!("Embeddings: on ({model})"),
            },
            _ => {
                let reason = embedding
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("not configured");
                format!("Embeddings: off — {reason}")
            }
        };
        lines.push(line);
        if state != "on" && !reason_names_next_step(embedding) {
            lines.push(
                "  Recall by meaning: assign the embedding role (Alt+M) or run /setup embeddings."
                    .to_owned(),
            );
        }
    }
    if let Some(probe) = value.get("reachability").and_then(Value::as_str) {
        lines.push(format!("Service probe: {probe}"));
    }
    if let Some(error) = error {
        lines.push(format!("Why: {error}"));
    }
    for note in value
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        lines.push(format!("Note: {note}"));
    }
    if backend == "off" {
        lines.push("Next: choose a backend in /settings → Memory or with /setup memory".to_owned());
    } else if !active {
        lines.push(format!("Next: {}", memory_next_step(error.unwrap_or(""))));
    } else if !noun.is_empty() {
        let clear = if backend == "semantic" {
            "/memory clear forgets project facts (/memory clear user: user-wide)"
        } else {
            "/memory clear forgets project facts"
        };
        lines.push(format!("Next: {clear} · /memory help · /settings → Memory"));
    }
    lines.join("\n")
}

/// Whether a backend's embedding reason already says what to do (it mentions a role or a command).
fn reason_names_next_step(embedding: &Value) -> bool {
    embedding
        .get("reason")
        .and_then(Value::as_str)
        .is_some_and(|reason| reason.contains("Alt+M") || reason.contains("/setup"))
}

/// Where to fix an inactive memory backend: the Roles view when a model role is missing, settings
/// otherwise.
fn memory_next_step(reason: &str) -> &'static str {
    if reason.contains("/roles") || reason.contains(" role") {
        "assign the role in /roles (Alt+M)"
    } else {
        "/settings → Memory, or choose another backend with /setup memory"
    }
}

/// The "memory is paused" notice, due after a startup or rebuild only when the backend or its open
/// error differs from the last one reported (`last` remembers it).
fn memory_notice_due(
    last: &mut Option<(String, Option<String>)>,
    backend: &str,
    error: Option<String>,
) -> Option<String> {
    let state = (backend.to_owned(), error);
    if last.as_ref() == Some(&state) {
        return None;
    }
    let notice = state.1.as_deref().map(|reason| {
        if reason.contains("/roles") || reason.contains("/settings") {
            format!("Memory is paused: {reason}.")
        } else {
            format!(
                "Memory is paused: {reason}. Next: {}.",
                memory_next_step(reason)
            )
        }
    });
    *last = Some(state);
    notice
}

/// The remembered items for a backend that stores facts or lessons: a count heading, then one row per
/// item (newest first), or an empty-state line that says how items get recorded.
fn remembered_lines(value: &Value, noun: &str) -> Vec<String> {
    let count = value.get("count").and_then(Value::as_u64).unwrap_or(0);
    let truncated = value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if count == 0 {
        let how = if noun == "lesson" {
            "Dial learns from past sessions for this project."
        } else {
            "Dial records facts as sessions run."
        };
        return vec![format!("No remembered {noun}s yet."), how.to_owned()];
    }
    let shown = if truncated {
        format!("{count}+")
    } else {
        count.to_string()
    };
    let plural = if count == 1 && !truncated { "" } else { "s" };
    if value.get("recent").is_none() {
        // Backends that report only a count (semantic) list nothing, so make no "newest first" claim.
        return vec![format!(
            "{shown} remembered {noun}{plural} in this project."
        )];
    }
    let mut lines = vec![format!("{shown} remembered {noun}{plural}, newest first:")];
    for item in value
        .get("recent")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let content = item.get("content").and_then(Value::as_str).unwrap_or("");
        let content = content.split_whitespace().collect::<Vec<_>>().join(" ");
        let content: String = if content.chars().count() > 110 {
            format!("{}…", content.chars().take(109).collect::<String>())
        } else {
            content
        };
        match item.get("updated_at").and_then(Value::as_i64) {
            Some(then) => lines.push(format!("  {}  {content}", memory_age(then))),
            None => lines.push(format!("  {content}")),
        }
    }
    lines
}

/// Relative age of a unix timestamp in seconds: `just now`, `5m ago`, `2h ago`, `3d ago`.
fn memory_age(then: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(then, |d| d.as_secs() as i64);
    match (now - then).max(0) {
        0..=59 => "just now".into(),
        secs @ 60..=3599 => format!("{}m ago", secs / 60),
        secs @ 3600..=86399 => format!("{}h ago", secs / 3600),
        secs => format!("{}d ago", secs / 86400),
    }
}

fn session_age(started: &str) -> String {
    let Some(stamp) = started.get(..19).and_then(crate::platform::time::parse_utc) else {
        return started.to_owned();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    let seconds = now.saturating_sub(stamp).max(0) as u64;
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

/// An effective snapshot includes private policy and resolved limits omitted from persisted TOML.
fn config_key(config: &Config) -> String {
    let mut table = toml::Table::try_from(config).expect("validated config serializes");
    table.insert(
        "effective_context_window".into(),
        i64::from(config.context_window).into(),
    );
    table.insert(
        "effective_chatgpt_accounts".into(),
        format!("{:?}", config.chatgpt_allowed_accounts).into(),
    );
    table.insert(
        "pinned_chatgpt_accounts".into(),
        format!("{:?}", config.chatgpt_pinned_accounts).into(),
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
    if !onboard(app, shell, session.as_ref())? {
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
    let mut plan_mode = app.mode_flags.plan_mode;
    let mut startup_flags = Some(app.mode_flags.clone());
    let mut permission_rules = None;
    let mut session_cost = None;
    let mut runtime_accounting = None;
    let mut plan_carry: (Option<String>, Option<String>) = Default::default();
    shell.begin(app, &session, resumed);
    // The backend and its open error last reported, so the paused notice repeats only on change.
    let mut memory_reported: Option<(String, Option<String>)> = None;
    // `session_end` is handed from one agent to the next across same-session rebuilds.
    let mut lifecycle_open = false;
    // A local model server survives a same-session rebuild; the new backend reuses it when the
    // changed settings still match what it loaded.
    let mut resident = None;
    loop {
        let step = {
            let mut agent = app.make_agent(session)?;
            if let Some(server) = resident.take() {
                agent.adopt_resident_server(server);
            }
            if lifecycle_open {
                agent.continue_session_lifecycle();
            } else {
                agent.begin_session_lifecycle();
            }
            // Re-checked after every rebuild: a settings change can switch to a backend that
            // cannot open (a missing role, a half override), which must not go silent.
            if let Some(notice) = memory_notice_due(
                &mut memory_reported,
                app.config.memory.backend.as_str(),
                agent.memory_open_error(),
            ) {
                shell.transcript.notice("Memory", &notice);
            }
            if let Some(snapshot) = &todo {
                agent.restore_todo(snapshot)?;
            }
            agent.set_approval(approval);
            agent.set_session_cost(session_cost);
            if let Some(accounting) = runtime_accounting.take() {
                agent.restore_runtime_accounting(accounting)?;
            }
            agent.set_plan_mode(plan_mode);
            agent.restore_plan_carry(std::mem::take(&mut plan_carry));
            if let Some(flags) = startup_flags.take() {
                for notice in flags.apply_to_agent(&mut agent)? {
                    shell.transcript.notice("Mode", &notice);
                }
                // `--goal` excludes a positional prompt, so the objective is the first prompt.
                if let Some(objective) = &flags.goal {
                    shell.startup = Some(objective.clone());
                }
            }
            if let Some(rules) = permission_rules.take() {
                agent.set_permission_rules(rules);
            }
            let step = shell.run(app, &mut agent);
            if matches!(step, Ok(Step::Rebuild(_))) {
                agent.hand_off_session_lifecycle();
                lifecycle_open = true;
                resident = agent.take_resident_server();
            } else {
                agent.end_session_lifecycle();
            }
            agent.cancel_background_compaction();
            runtime_accounting = Some(agent.runtime_accounting());
            session = std::mem::replace(agent.session_mut(), Session::ephemeral());
            todo = Some(agent.todo().snapshot());
            approval = agent.approval();
            plan_mode = agent.plan_mode();
            plan_carry = agent.plan_carry();
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
                // Removing a model/provider layer can leave a valid but unconfigured setup.
                // Select a usable replacement without discarding the held session snapshots.
                if !onboard(app, shell, Some(&session))? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Esc while a sign-in still runs: the first Esc keeps setup open with a warning; the next gives up on the sign-in.
fn sign_in_still_pending(warned: &mut bool) -> bool {
    let first = !*warned;
    *warned = true;
    first
}

/// First run (or a provider that cannot be used): let the user pick a model before any agent exists.
fn onboard(app: &mut App, shell: &mut Shell, session: Option<&Session>) -> Result<bool> {
    // First run starts on provider setup (sign in or add a key for a hosted model); Esc there
    // moves to the discovered local model list once before quitting.
    let mut providers_first = app.config.model.trim().is_empty();
    let mut auth_warned = false;
    loop {
        let ready = !app.config.model.trim().is_empty()
            && match session {
                Some(session) => crate::agent::make_session_backend(&app.config, session).is_ok(),
                None => make_backend(&app.config).is_ok(),
            };
        if ready {
            return Ok(true);
        }
        shell.note("Welcome to dial", if app.config.model.trim().is_empty() {
            "Sign in to a provider or add an API key to start with a hosted model. Esc shows local models instead: running LM Studio or Ollama servers (and MLX models on Apple Silicon) are found automatically."
        } else {
            "The selected provider cannot be used yet (missing credentials?). Set it up in Providers."
        });
        let which = if providers_first {
            Which::Providers
        } else {
            Which::Models
        };
        let outcome = shell.run_overlay(app, None, which)?;
        match outcome.latest {
            Some(config) => {
                app.adopt(config);
                shell.after_adopt(app);
            }
            None => {
                if shell.auth_job.is_some() {
                    if sign_in_still_pending(&mut auth_warned) {
                        let label = shell.auth_job.as_ref().map_or("Sign-in", |job| job.label);
                        shell.note(label, format!("{label} is still waiting for your browser. Esc again quits and cancels it; select a model after sign-in completes."));
                        continue;
                    }
                    if let Some(job) = shell.auth_job.take() {
                        job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                if providers_first {
                    providers_first = false;
                    shell.note(
                        "No model yet",
                        "Pick a discovered local model here (LM Studio, Ollama, MLX). Esc again quits.",
                    );
                    continue;
                }
                // `dial setup -m` installs the MLX runtime, which exists only on Apple Silicon Macs.
                let local = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                    ", or download a local model with `dial setup -m <mlx-model>`"
                } else {
                    ""
                };
                shell.note(
                    "No model selected",
                    format!(
                        "Run dial again to pick a model or sign in to a provider{local}. Inside dial, {} opens the model list and /providers sets up sign-in and API keys.",
                        shell.keys.label(Action::ModelSelect)
                    ),
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
        let mut selections = vec![app.config.selection()];
        selections.extend(app.config.model_roles.values().cloned());
        self.launch(move || catalog::discover(&layout, &configured, &selections));
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
    root: Option<Arc<Path>>,
    cache: StatusCache<GitStatus>,
    files: StatusCache<Vec<crate::ui::rail::FileDiff>>,
}

impl Git {
    fn bind_root(&mut self, root: &Path) -> Option<Arc<Path>> {
        // The first paint may precede status initialization. An empty `git -C`
        // targets the launch directory, not the selected workspace.
        if root.as_os_str().is_empty() {
            return None;
        }
        if self.root.as_deref() != Some(root) {
            self.cache = StatusCache::default();
            self.files = StatusCache::default();
            self.root = Some(Arc::from(root));
        }
        self.root.clone()
    }
    fn files(&mut self, root: &Path) -> Vec<crate::ui::rail::FileDiff> {
        let Some(root) = self.bind_root(root) else {
            return Vec::new();
        };
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
        let Some(root) = self.bind_root(root) else {
            return GitStatus::default();
        };
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
    use std::io::ErrorKind;
    let mut command = Command::new("git");
    command
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
        .stderr(Stdio::null());
    let mut group = crate::tools::shell::ProcessGroup::new(
        crate::platform::process::spawn_group(&mut command).ok()?,
    );
    let mut stdout = group.child.stdout.take()?;
    crate::platform::io::set_nonblocking(&stdout).ok()?;
    // This already runs on a status worker. Drain here while checking the
    // deadline, rather than waiting for exit with a potentially full pipe.
    let deadline = Instant::now() + GIT_TIMEOUT;
    let mut cleanup = None;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut eof = false;
    loop {
        for _ in 0..16 {
            match crate::platform::io::read_nonblocking(&mut stdout, &mut buffer) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        let now = Instant::now();
        if group.exited().ok()? {
            group.terminate();
            let until = cleanup.get_or_insert(now + Duration::from_millis(300));
            if eof {
                break;
            }
            if now >= *until {
                return None;
            }
        } else if now >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = group.child.wait().ok()?;
    if status.success() {
        String::from_utf8(bytes).ok()
    } else {
        None
    }
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
        let config = app.reload().unwrap_or_else(|_| app.config.clone());
        let available = true;
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
    if !config.runs_dial_mlx() {
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

#[cfg(windows)]
fn local_time(format: &str) -> String {
    crate::platform::time::local_strftime(format)
}

#[cfg(unix)]
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

/// Name, arguments syntax and one-line description of every built-in slash command, in the order
/// the lists show them. The syntax lives in the usage column, never in the description. Hidden
/// aliases are in [`COMMAND_ALIASES`]; a usage of `""` means the command takes no arguments.
const BUILTIN_COMMANDS: &[(&str, &str, &str)] = &[
    (
        "workers",
        "[id]",
        "open the worker panel: results, cancel and message",
    ),
    (
        "processes",
        "",
        "open background processes and attach to a job's terminal",
    ),
    ("palette", "", "search commands and actions"),
    ("compute", "", "compute jobs and local runtimes"),
    (
        "changes",
        "",
        "list changed files and line counts for this workspace",
    ),
    (
        "help",
        "[all|text]",
        "search actions and commands; /help text lists them",
    ),
    (
        "queue",
        "[edit|remove] <number>",
        "review queued follow-ups; edit or remove one",
    ),
    (
        "review",
        "[uncommitted|base <ref>|ref <ref>|commit <ref>|pr://N|PR URL]",
        "read-only AI review of changes",
    ),
    (
        "review-file",
        "<turn> [path]",
        "review checkpoint changes; keep or revert them",
    ),
    (
        "model",
        "[settings|defaults|local]",
        "choose the main model and assign roles",
    ),
    ("providers", "", "set up providers, keys and endpoints"),
    ("settings", "", "appearance, tools and coding preferences"),
    (
        "reload",
        "",
        "reload settings, commands, skills and themes from disk",
    ),
    ("theme", "[name|auto]", "list or switch the color theme"),
    ("hotkeys", "", "view and edit keyboard shortcuts"),
    ("new", "", "start a fresh session"),
    ("sessions", "", "search and resume sessions"),
    (
        "resume",
        "[id]",
        "search sessions or resume a unique id prefix",
    ),
    ("fork", "", "copy this conversation into a new session"),
    ("rename", "<title>", "name this session"),
    (
        "export",
        "[path] [--html]",
        "export the conversation as Markdown or HTML",
    ),
    (
        "archive",
        "",
        "browse recent projects, sessions and prompts",
    ),
    (
        "autoresearch",
        "[status|init|run|log|notes|resume]",
        "manage branch-scoped benchmark experiments",
    ),
    ("tree", "", "browse session forks"),
    ("copy", "", "copy the last assistant answer"),
    ("plan", "[on|off]", "toggle read-only planning mode"),
    ("profile", "[name|off]", "choose a capability profile"),
    (
        "permissions",
        "[allow|deny] [--persist] <tool> [prefix] | remove <number>",
        "list, add or remove allow and deny rules",
    ),
    ("cost", "", "show observed runtime cost and usage coverage"),
    (
        "agents",
        "[definitions]",
        "open the worker panel; list agent definitions",
    ),
    ("message", "<id> <text>", "steer a live subagent"),
    ("cancel", "<id>", "cancel a live subagent"),
    (
        "compact",
        "[soft|remote|snapcompact|handoff|shake] [focus]",
        "compact the conversation context",
    ),
    (
        "undo",
        "",
        "drop the last turn and restore its file changes",
    ),
    (
        "rollback",
        "[n]",
        "restore files changed in the last n turns",
    ),
    (
        "clear",
        "",
        "clear the conversation and start a fresh session",
    ),
    ("todo", "", "show the task list"),
    (
        "skills",
        "[list|install|update|remove|enable|disable|import|sources]",
        "browse, install and manage skills",
    ),
    (
        "memory",
        "[clear [project|global]]",
        "what Dial remembers for this project",
    ),
    (
        "reliability",
        "",
        "live tool success, corrections and verification",
    ),
    (
        "policy",
        "",
        "live task phase, effective settings and request metrics",
    ),
    (
        "steering",
        "",
        "activation vector, evaluation and runtime use",
    ),
    (
        "experience",
        "[ledger]",
        "learned recipes, failures and investigation",
    ),
    (
        "understanding",
        "[architecture|build|clear|query]",
        "how the project fits together",
    ),
    (
        "code-history",
        "[words] [path:<p>] [branch:<b>] | show <id> | clear",
        "search diffs Dial's turns made, across sessions",
    ),
    (
        "laya",
        "[status|check|start|on|off]",
        "local decision advice",
    ),
    ("commands", "", "list custom commands"),
    ("search", "<query>", "search past sessions"),
    ("context", "", "show token usage"),
    (
        "approval",
        "[ask|write|auto|readonly]",
        "choose when tool calls need approval",
    ),
    (
        "outcome",
        "<0-100> [note]",
        "record your verdict on the last answer",
    ),
    (
        "improve",
        "[status|jobs|stop|cycle|train|session|enable|disable]",
        "self-improvement status and job controls",
    ),
    (
        "accounts",
        "pin",
        "pin which ChatGPT account this session uses",
    ),
    ("exit", "", "quit"),
];

/// Hidden aliases: resolved to the target before dispatch, and listed in no menu, palette or help.
const COMMAND_ALIASES: &[(&str, &str)] = &[
    ("quit", "exit"),
    ("q", "exit"),
    ("?", "help"),
    ("models", "model"),
    ("provider", "providers"),
    ("reasoning", "effort"),
    ("thinking", "effort"),
    ("jobs", "workers"),
    ("hub", "workers"),
    ("bg", "processes"),
    ("doctor", "setup"),
    ("plugin", "plugins"),
];

/// The dispatch name for `name`, following one hidden alias.
fn canonical_command(name: &str) -> &str {
    COMMAND_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, target)| target)
}

/// Commands that end or replace the running turn, so typing one stops the turn first. `stops_turn`,
/// `/help` and the palette all read this list.
const STOPS_TURN: &[&str] = &[
    "exit", "new", "clear", "resume", "sessions", "tree", "fork", "undo", "rollback", "compact",
    "plan",
];

/// Commands that run at once while a turn runs, without stopping it (see `live_command`). `/help`
/// and the palette read this list too.
const RUNS_NOW: &[&str] = &[
    "help",
    "hotkeys",
    "todo",
    "search",
    "skills",
    "commands",
    "context",
    "reliability",
    "policy",
    "workers",
    "message",
    "cancel",
    "queue",
];

/// Subcommands that run at once while a turn runs; the command itself is not in [`RUNS_NOW`].
const RUNS_NOW_ARGS: &[&str] = &["improve status", "improve jobs", "improve stop"];

/// Built-ins always win at dispatch, so a custom command with a built-in's name is never offered.
fn command_entries(custom: &[context::CommandDef], skills: &[skills::Skill]) -> Vec<CommandEntry> {
    let mut entries: Vec<CommandEntry> = BUILTIN_COMMANDS
        .iter()
        .map(|(name, usage, description)| CommandEntry {
            name: (*name).into(),
            description: (*description).into(),
            kind: CommandKind::Builtin,
            args_hint: (!usage.is_empty()).then(|| (*usage).to_owned()),
        })
        .collect();
    // Advanced sections (self-improvement, reliability, decision services, maintenance) sit after
    // the coding commands so the palette and completion lead with the work. The section is derived
    // from the category, so a label can never disagree with where the command is listed.
    entries.sort_by_key(|entry| crate::ui::command_surface::category(&entry.name).advanced());
    for spec in commands::specs() {
        if entries.iter().any(|e| e.name == spec.name) {
            continue;
        }
        entries.push(CommandEntry {
            name: spec.name.into(),
            description: spec.description.into(),
            kind: CommandKind::Builtin,
            args_hint: (!spec.args_hint.is_empty()).then(|| spec.args_hint.to_owned()),
        });
    }
    let reserved = |name: &str| {
        BUILTIN_COMMANDS
            .iter()
            .any(|(builtin, _, _)| *builtin == name)
            || COMMAND_ALIASES.iter().any(|(alias, _)| *alias == name)
            || matches!(name, "roles" | "model-settings" | "status")
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

/// What typing `name` during a running turn does. Derived from the same lists dispatch uses.
fn during_turn_note(name: &str) -> &'static str {
    let name = canonical_command(name);
    if STOPS_TURN.contains(&name) {
        "Stops the current turn before running."
    } else if name == "reload" {
        "Refreshes disk resources now; validated settings apply after this turn."
    } else if matches!(name, "model" | "providers" | "settings" | "compute") {
        "Opens now; changes to model/config apply after this turn."
    } else if matches!(name, "workers" | "agents") {
        "Opens now; m messages or resumes a worker, x x cancels a running one."
    } else if name == "processes" {
        "Opens now; keystrokes go to the attached job."
    } else if RUNS_NOW.contains(&name) {
        "Runs now without stopping the current turn."
    } else {
        "Runs after the current turn finishes."
    }
}

fn palette_entries(custom: &[context::CommandDef], skills: &[skills::Skill]) -> Vec<PaletteEntry> {
    let mut entries = command_entries(custom, skills)
        .into_iter()
        .map(|command| PaletteEntry {
            during_turn: during_turn_note(&command.name),
            command,
        })
        .collect::<Vec<_>>();
    // Getting-started commands lead the palette; the rest keep their registry order (stable sort).
    entries.sort_by_key(|entry| {
        crate::ui::command_surface::STARTERS
            .iter()
            .position(|starter| *starter == entry.command.name)
            .unwrap_or(usize::MAX)
    });
    entries
}

/// `/memory` argument choices for the selected backend: each backend accepts different
/// subcommands, and an unknown word is a search, so only real subcommands are offered.
fn memory_argument_completions(config: &Config) -> Vec<String> {
    use crate::config::memory_backend::MemoryBackendId;
    let backend = if config.memory.enabled {
        config.memory.backend
    } else {
        MemoryBackendId::Off
    };
    let (words, nested): (&[&str], &[(&str, &[&str])]) = match backend {
        MemoryBackendId::Off => (&["status", "help"], &[]),
        MemoryBackendId::Provenance => (
            &["status", "search", "clear", "help"],
            &[("clear", &["project", "global"])],
        ),
        MemoryBackendId::Semantic => (
            &[
                "status", "search", "save", "forget", "update", "history", "rebuild", "clear",
                "help",
            ],
            &[("clear", &["user"])],
        ),
        MemoryBackendId::Hindsight => (
            &[
                "status", "search", "save", "enqueue", "queue", "mm", "clear", "help",
            ],
            &[(
                "mm",
                &["list", "seed", "show", "history", "refresh", "delete"],
            )],
        ),
        MemoryBackendId::Local => (&["status", "save", "enqueue", "clear", "help"], &[]),
        MemoryBackendId::Sharpshooter => (
            &[
                "status", "search", "stats", "diagnose", "queue", "enqueue", "clear", "help",
            ],
            &[],
        ),
    };
    let mut out: Vec<String> = words.iter().map(|word| format!("/memory {word}")).collect();
    for (word, args) in nested {
        out.extend(args.iter().map(|arg| format!("/memory {word} {arg}")));
    }
    out
}

/// Concrete argument choices, not usage placeholders. Keep these next to the command registry;
/// the composer only matches/renders continuations and never knows command semantics.
fn command_argument_completions(config: &Config, skills: &[skills::Skill]) -> Vec<String> {
    let mut candidates = Vec::new();
    for (name, args) in [
        (
            "model",
            &[
                "settings", "defaults", "local", "next", "prev", "cancel", "restart",
            ][..],
        ),
        ("understanding", &["architecture", "build", "clear"][..]),
        ("code-history", &["show", "clear"]),
        ("checks", &["list", "add", "remove", "help"][..]),
        ("experience", &["ledger"]),
        ("laya", &["status", "check", "start", "on", "off"]),
        (
            "skills",
            &[
                "list",
                "sources",
                "import",
                "enable",
                "disable",
                "install",
                "update",
                "remove",
                "installed",
                "search",
            ],
        ),
        ("skills import", &["--dry-run", "--user", "--update"]),
        ("skills install", &["--user"]),
        ("approval", Approval::NAMES),
        ("plan", &["on", "off"]),
        ("permissions", &["allow", "deny", "remove"]),
        ("permissions allow", &["--persist"]),
        ("permissions deny", &["--persist"]),
        (
            "improve",
            &[
                "status", "jobs", "stop", "cycle", "train", "session", "enable", "disable",
                "versions", "tasks",
            ],
        ),
        ("theme", &["auto"]),
        ("export", &["--html"]),
        ("profile", &["off"]),
    ] {
        candidates.extend(args.iter().map(|arg| format!("/{name} {arg}")));
    }
    candidates.extend(memory_argument_completions(config));
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
        {
            let command = "effort";
            candidates.extend(
                profile
                    .capabilities
                    .reasoning_efforts
                    .iter()
                    .map(|effort| format!("/{command} {}", effort.as_str())),
            );
        }
    }
    candidates.extend(commands::completions(config));
    candidates
}

// ---------------------------------------------------------------------------
// Configuration changes as data (pure, so they can be tested without a terminal)
// ---------------------------------------------------------------------------

fn selection_headers<'a>(
    config: &'a Config,
    selection: &'a ModelSelection,
) -> &'a [crate::llm::model_config::RequestHeader] {
    selection
        .headers
        .as_deref()
        .or_else(|| {
            config
                .providers
                .get(&selection.provider)
                .filter(|provider| {
                    selection.base_url.as_ref().is_none_or(|url| {
                        url.trim_end_matches('/') == provider.base_url.trim_end_matches('/')
                    })
                })
                .and_then(|provider| provider.headers.as_deref())
        })
        .unwrap_or_default()
}

/// The `/model next` cycle set: the `--models` scope when installed, else saved favorites.
fn cycle_set(config: &Config) -> Vec<ModelSelection> {
    crate::cli::model_scope::cycle_set().unwrap_or_else(|| config.model_favorites.clone())
}

/// Next model in the `/model next` cycle: the active `--models` scope when one is installed,
/// otherwise the saved favorites (`cycle_set`), restricted to `available` models.
fn next_favorite(
    config: &Config,
    favorites: &[ModelSelection],
    available: &[ModelSelection],
) -> Option<ModelSelection> {
    let same_model = |a: &ModelSelection, b: &ModelSelection| {
        a.provider == b.provider
            && a.model == b.model
            && config
                .selection_base_url(a)
                .map(|url| url.trim_end_matches('/').to_owned())
                == config
                    .selection_base_url(b)
                    .map(|url| url.trim_end_matches('/').to_owned())
            && selection_protocol(config, a) == selection_protocol(config, b)
    };
    let same = |a: &ModelSelection, b: &ModelSelection| {
        same_model(a, b)
            && a.account_id == b.account_id
            && a.api_key_env == b.api_key_env
            && a.allow_insecure == b.allow_insecure
            && selection_headers(config, a) == selection_headers(config, b)
    };
    let mut candidates = Vec::new();
    for favorite in favorites {
        if let Some(selection) = available
            .iter()
            .find(|selection| same_model(favorite, selection))
        {
            let mut candidate = selection.clone();
            candidate.account_id = favorite.account_id.clone();
            candidate.headers = favorite.headers.clone();
            candidate.allow_insecure = favorite.allow_insecure
                || (favorite.base_url.is_none() && selection.allow_insecure);
            if let Some(variable) = &favorite.api_key_env {
                candidate.api_key_env = Some(variable.clone());
            }
            if !candidates.iter().any(|existing| same(existing, &candidate)) {
                candidates.push(candidate);
            }
        }
    }
    let current = config.selection();
    let index = candidates
        .iter()
        .position(|selection| same(selection, &current))
        .map_or(0, |index| (index + 1) % candidates.len());
    candidates.get(index).cloned()
}

/// The settings path a change writes.
fn change_path(change: &Change) -> &str {
    match change {
        Change::Set { path, .. } | Change::Remove { path } => path,
    }
}

/// Make `role` follow the main model again. In the project layer an explicit marker is needed so a
/// global assignment does not show through.
fn clear_role_changes(role: &str, scope: Scope) -> Result<Vec<Change>> {
    ensure!(
        ROLES.iter().any(|(name, _)| *name == role) && role != "main",
        "the main model cannot be cleared"
    );
    let changes = match scope {
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
    Ok(changes)
}

fn reasoning_profile(config: &Config) -> Result<Option<crate::llm::profiles::ModelProfile>> {
    if let Some(profile) = config.active_model_profile() {
        return Ok(Some(profile.clone()));
    }
    if config.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER)
        && let Some(profile) = crate::llm::chatgpt::model_profile(&config.user_dir, &config.model)?
    {
        return Ok(Some(profile));
    }
    let selection = config.selection();
    let identity = format!("{}/{}", selection.provider, selection.model);
    let Some(model) = crate::llm::catalog::model_capabilities(&identity) else {
        return Ok(None);
    };
    let Some(mode) = model.thinking else {
        return Ok(None);
    };
    use crate::llm::profiles::{ModelCapabilities, ModelProfile, ReasoningApi};
    let reasoning = match mode {
        crate::llm::catalog::ThinkingMode::Effort => {
            if selection.protocol == Some(crate::llm::model_config::ApiProtocol::Responses) {
                ReasoningApi::OpenaiResponses
            } else {
                ReasoningApi::OpenaiEffort
            }
        }
        crate::llm::catalog::ThinkingMode::Budget => match selection.provider.as_str() {
            "anthropic" => ReasoningApi::AnthropicBudget,
            "gemini" => ReasoningApi::GeminiBudget,
            _ => ReasoningApi::OpenaiEffort,
        },
        crate::llm::catalog::ThinkingMode::AnthropicAdaptive => ReasoningApi::AnthropicAdaptive,
        crate::llm::catalog::ThinkingMode::AnthropicBudgetEffort => ReasoningApi::AnthropicBudget,
        crate::llm::catalog::ThinkingMode::GoogleLevel => ReasoningApi::GeminiLevel,
    };
    Ok(Some(ModelProfile {
        provider: selection.provider,
        model: selection.model,
        capabilities: ModelCapabilities {
            reasoning,
            reasoning_efforts: model.thinking_efforts,
            ..Default::default()
        },
        reasoning_effort: model.default_effort,
        thinking: Some(true),
        ..Default::default()
    }))
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
    if args == "auto" {
        // `auto` stores a flag instead of a profile: each user turn's effort is judged from its prompt.
        return Ok((
            vec![Change::Set {
                path: "thinking_auto".into(),
                value: toml::Value::Boolean(true),
            }],
            "auto".into(),
        ));
    }
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
        ReasoningApi::AnthropicBudget
            | ReasoningApi::AnthropicAdaptive
            | ReasoningApi::GeminiBudget
            | ReasoningApi::OllamaThink
    ) && matches!(args, "on" | "off")
    {
        profile.thinking = Some(args == "on");
        if profile.capabilities.reasoning == ReasoningApi::GeminiBudget {
            if args == "off" {
                profile.thinking_budget = Some(0);
            } else if profile.thinking_budget == Some(0) {
                profile.thinking_budget = None;
            }
        } else if profile.capabilities.reasoning == ReasoningApi::OllamaThink {
            profile.reasoning_effort = None;
        }
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
            Change::Set {
                path: "thinking_auto".into(),
                value: toml::Value::Boolean(false),
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
    changes.push(set(
        "protocol",
        spec.protocol
            .unwrap_or_else(|| crate::llm::model_config::ApiProtocol::builtin(id))
            .as_str()
            .into(),
    ));
    changes.push(set(
        "headers",
        crate::llm::model_config::headers_setting(&spec.headers),
    ));
    changes.push(set(
        "api_key_env",
        spec.api_key_env.clone().unwrap_or_default().into(),
    ));
    changes
}

// ---------------------------------------------------------------------------
// The shell: everything visible and interactive
// ---------------------------------------------------------------------------

/// Includes inode and change time, so atomic replacement and same-sized edits are observable.
fn hash_resource(path: &Path, hash: &mut DefaultHasher) -> Result<bool> {
    path.hash(hash);
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            #[cfg(unix)]
            (
                meta.dev(),
                meta.ino(),
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec(),
                meta.ctime(),
                meta.ctime_nsec(),
                meta.mode(),
                meta.uid(),
                meta.nlink(),
            )
                .hash(hash);
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                (
                    meta.len(),
                    meta.last_write_time(),
                    meta.creation_time(),
                    meta.file_attributes(),
                )
                    .hash(hash);
                if !meta.file_type().is_symlink() {
                    crate::platform::fs::path_identity(path)?.hash(hash);
                }
            }
            if meta.file_type().is_symlink() {
                fs::read_link(path)?.hash(hash);
                if let Ok(target) = fs::metadata(path) {
                    #[cfg(unix)]
                    (
                        target.dev(),
                        target.ino(),
                        target.len(),
                        target.mtime(),
                        target.mtime_nsec(),
                        target.ctime(),
                        target.ctime_nsec(),
                    )
                        .hash(hash);
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::MetadataExt;
                        (
                            target.len(),
                            target.last_write_time(),
                            target.creation_time(),
                            target.file_attributes(),
                        )
                            .hash(hash);
                        let file = std::fs::File::open(path)?;
                        crate::platform::fs::identity(&file)?.hash(hash);
                    }
                }
            }
            Ok(meta.is_dir())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            0u8.hash(hash);
            Ok(false)
        }
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

fn settings_fingerprint(layout: &Layout) -> Result<u64> {
    let mut hash = DefaultHasher::new();
    for path in layout.config_files() {
        hash_resource(&path, &mut hash)?;
    }
    Ok(hash.finish())
}

/// Files hashed by `resources_fingerprint` and watched by `resource_events`.
fn resource_files(layout: &Layout) -> [std::path::PathBuf; 10] {
    [
        layout.user_dir.join("keybindings.toml"),
        layout.user_dir.join("skills.json"),
        layout.state_dir.join("skills.json"),
        layout.user_dir.join("SOUL.md"),
        layout.project_dir.join("SYSTEM.md"),
        layout.project_dir.join("APPEND_SYSTEM.md"),
        layout.project_dir.join("AGENTS.md"),
        layout.project_root.join("AGENTS.md"),
        layout.project_root.join("CLAUDE.md"),
        layout.project_root.join(".cursorrules"),
    ]
}

/// Directories hashed by `resources_fingerprint`; the flag marks skill roots (their `SKILL.md` counts).
fn resource_dirs(layout: &Layout) -> [(std::path::PathBuf, bool); 5] {
    [
        (layout.project_dir.join("commands"), false),
        (layout.user_dir.join("commands"), false),
        (layout.user_dir.join("themes"), false),
        (layout.project_dir.join("skills"), true),
        (layout.user_dir.join("skills"), true),
    ]
}

/// Poll discovery inputs, not the entire workspace or bundled skill trees.
fn resources_fingerprint(layout: &Layout) -> Result<u64> {
    let started = Instant::now();
    let mut hash = DefaultHasher::new();
    let mut count = 0;
    for path in resource_files(layout) {
        hash_resource(&path, &mut hash)?;
    }
    for (dir, skill_root) in resource_dirs(layout) {
        if !hash_resource(&dir, &mut hash)? {
            continue;
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("cannot scan {}", dir.display()))? {
            count += 1;
            ensure!(
                count <= RESOURCE_SCAN_CAP && started.elapsed() < RESOURCE_SCAN_TIME,
                "resource change scan exceeded its bounded budget; automatic refresh is incomplete"
            );
            paths.push(entry?.path());
        }
        paths.sort();
        for path in paths {
            ensure!(
                started.elapsed() < RESOURCE_SCAN_TIME,
                "resource change scan exceeded its bounded budget; automatic refresh is incomplete"
            );
            if hash_resource(&path, &mut hash)? && skill_root {
                hash_resource(&path.join("SKILL.md"), &mut hash)?;
            }
        }
    }
    Ok(hash.finish())
}

struct ResourceWatch {
    at: Instant,
    settings: Option<u64>,
    resources: Option<u64>,
    diagnostic: Option<String>,
    reload: settings_store::SettingsReload,
    events: Option<ResourceEvents>,
}

/// Disk-change signal from `notify`. `complete` means every resource input has a live watch, so the
/// heartbeat poll drops to `RESOURCE_WATCHED_FALLBACK`; otherwise it stays at `RESOURCE_TTL`.
struct ResourceEvents {
    dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    complete: bool,
    _watcher: notify::RecommendedWatcher,
}

/// Watch the same inputs that `settings_fingerprint` and `resources_fingerprint` hash. Returns `None`
/// only when the platform watcher cannot start; partial coverage still signals and keeps TTL polling.
fn resource_events(layout: &Layout) -> Option<ResourceEvents> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let dirty = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dirty);
    let mut watcher = notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
        flag.store(true, Ordering::Release);
    })
    .ok()?;
    let mut complete = true;
    let mut paths: Vec<(std::path::PathBuf, bool)> = layout
        .config_files()
        .into_iter()
        .chain(resource_files(layout))
        .map(|path| (path, false))
        .collect();
    paths.extend(
        resource_dirs(layout)
            .into_iter()
            .map(|(dir, _)| (dir, true)),
    );
    for (path, is_dir) in paths {
        let (target, mode) = if is_dir && path.is_dir() {
            (path, RecursiveMode::Recursive)
        } else {
            // Missing directories are seen through their parent; their contents stay on the TTL fallback.
            complete &= !is_dir;
            let Some(parent) = path.parent() else {
                complete = false;
                continue;
            };
            (parent.to_path_buf(), RecursiveMode::NonRecursive)
        };
        if !target.is_dir() || watcher.watch(&target, mode).is_err() {
            complete = false;
        }
    }
    Some(ResourceEvents {
        dirty,
        complete,
        _watcher: watcher,
    })
}

struct Shell {
    loop_mode: crate::agent::modes::loop_runner::LoopMode,
    runtime: crate::runtime::Tree,
    pending_result: Option<crate::runtime::Body>,
    shared_prose: bool,
    auth_job: Option<AuthJob>,
    /// `/extensions` intent waiting for the main loop to start it.
    pub(crate) extensions_pending: Option<commands::extensions_browser::Pending>,
    /// The running `/extensions` install, if any.
    pub(crate) extensions_job: Option<commands::extensions_browser::InstallJob>,
    /// Set by `/restart`; `run_interactive` relaunches the binary once the terminal is restored.
    restart_requested: bool,
    /// A provider retry was already explained in this turn; later attempts update nothing new.
    retry_noted: bool,
    smithery_resume: Option<commands::smithery::Resume>,
    commit_job: Option<commands::commit::CommitJob>,
    busy_overlay: Option<BusyOverlay>,
    term: InlineTerminal,
    themes: ThemeManager,
    keys: Keymap,
    composer: Composer,
    paste_guard: crate::ui::input::PasteGuard,
    transcript: Transcript,
    status: StatusData,
    usage_status: commands::UsageStatus,
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
    /// The live home screen, shown above the composer until the conversation starts.
    home: Option<welcome::Home>,
    started: Instant,
    request_phase: agent::RequestPhase,
    last_trajectory: Option<i64>,
    /// "Teach this model": recent turns whose changes may still be committed, oldest first.
    teach_pending: Vec<crate::learn::signals::TurnChange>,
    /// The rating-keys tip is shown once per shell.
    teach_hint_shown: bool,
    /// What the shell last saw of "Teach this model", to announce changes after a rebuild.
    teach_state: TeachState,
    /// Last "Teach this model" progress note: 0 none, 1 at three quarters of the threshold, 2 at it.
    teach_nudge_stage: u8,
    /// The background job this shell started, watched until it finishes.
    improve_watch: Option<ImproveWatch>,
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
    deferred_editor: bool,
    /// Shift+Tab / display reset pressed during a turn; applied at the next idle boundary.
    deferred_approval_cycle: bool,
    deferred_display_reset: bool,
    /// Handle to the agent's live jobs, usable while a turn holds the agent.
    live_tasks: Option<crate::agent::LiveTaskControl>,
    /// Session workers for the pinned block, the `workers` segment and finish notices.
    workers: crate::ui::worker_pins::WorkerBoard,
    /// Worker last opened by `Alt+Shift+A`, so repeated presses cycle through the running ones.
    recent_worker: Option<String>,
    background_control: Option<Arc<dyn crate::tools::ext::Tool>>,
    pty_approval: Approval,
    maintenance_control: Option<crate::agent::compaction_runtime::CompactionControl>,
    pending_config: Option<Config>,
    pending_verification: Option<ModelSelection>,
    /// A worker resume raised during a turn; launched when the turn settles.
    pending_resume: Option<String>,
    pending_favorite: Option<PendingFavorite>,
    resources: ResourceWatch,
    pending_resource_rebuild: bool,
    /// Push-to-talk capture in progress (`Alt+V`); its transcript goes into the draft.
    voice_input: Option<crate::voice::tui::PushToTalk>,
    /// Press/release state for the voice key; decides hold-to-talk or press-to-toggle.
    voice_gesture: crate::voice::gesture::Gesture,
    /// The read-out of the last answer (`voice.tts.auto_speak`) and the flag that stops it.
    speech: Option<(Arc<AtomicBool>, std::thread::JoinHandle<anyhow::Result<()>>)>,
}

struct PendingFavorite {
    main: ModelSelection,
    favorites: Vec<ModelSelection>,
    scope: Scope,
    catalog_request: u64,
}
impl PendingFavorite {
    fn current(&self, config: &Config, scope: Scope, request: u64) -> bool {
        let selection = config.selection();
        self.scope == scope
            && self.catalog_request == request
            && self.main.provider == selection.provider
            && self.main.model == selection.model
            && self.main.base_url == selection.base_url
            && selection_protocol(config, &self.main) == selection_protocol(config, &selection)
            && self.main.account_id == selection.account_id
            && self.main.headers == selection.headers
            && self.main.api_key_env == selection.api_key_env
            && self.main.allow_insecure == selection.allow_insecure
            && self.favorites == cycle_set(config)
    }
}

/// The configuration side of a local model's suggestion check; the overlay adds the profile's own
/// values (window, penalties, keep-alive, edit pin, draft model).
fn local_suggestion_base(config: &Config) -> crate::llm::local_evidence::Current {
    let name = |value: serde_json::Result<serde_json::Value>| {
        value
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    crate::llm::local_evidence::Current {
        edit_mode: name(serde_json::to_value(
            config.resolved_edit_mode(&config.harness),
        )),
        edit_mode_pinned: config.pinned_edit_mode(&config.harness).is_some(),
        context_window: config.context_window,
        draft_model: config.local_runtime.draft_model.clone(),
        side_models: name(serde_json::to_value(config.assistance.local.side_models)),
        ..Default::default()
    }
}

fn model_settings_profile(config: &Config) -> (String, crate::llm::profiles::ModelProfile) {
    use crate::llm::profiles::{ModelCapabilities, ModelProfile};
    let selection = config.selection();
    let profile = reasoning_profile(config).ok().flatten().unwrap_or_else(|| {
        config
            .model_profiles
            .values()
            .find(|p| p.provider == selection.provider && p.model == selection.model)
            .cloned()
            .unwrap_or_else(|| ModelProfile {
                provider: selection.provider.clone(),
                model: selection.model.clone(),
                capabilities: ModelCapabilities {
                    temperature: true,
                    top_p: true,
                    seed: matches!(selection.provider.as_str(), "mlx" | "lm-studio"),
                    ..Default::default()
                },
                ..Default::default()
            })
    });
    let existing = config
        .model_profile
        .clone()
        .filter(|_| config.active_model_profile().is_some())
        .or_else(|| {
            config
                .model_profiles
                .iter()
                .find(|(_, p)| p.provider == selection.provider && p.model == selection.model)
                .map(|(name, _)| name.clone())
        });
    let key = existing.unwrap_or_else(|| {
        use sha2::{Digest, Sha256};
        format!(
            "model_{:.16x}",
            Sha256::digest(format!("{}/{}", selection.provider, selection.model))
        )
    });
    (key, profile)
}

/// A background cycle or training run this shell started. Its finish is noted once.
struct ImproveWatch {
    state_dir: std::path::PathBuf,
    started: u64,
    checked: Instant,
}
enum Which {
    Palette,
    Choice(Box<ChoiceSpec>),
    Compute,
    Improve,
    Jobs,
    Subagents,
    ModelDownloads,
    SubagentsActivity,
    SubagentViewer {
        id: String,
        entry: Option<usize>,
        from_hub: bool,
    },
    Extensions,
    Settings,
    LearningSettings,
    LearningModels,
    ModelSettings,
    ContextDefaults,
    Models,
    ModelRoles,
    Providers,
    Shortcuts,
    Git {
        root: std::path::PathBuf,
        revision: String,
    },
}

/// `subagent-open:<id>[#<entry>]` from the hub: the worker's transcript, scrolled to the entry.
fn subagent_viewer_destination(action: &str) -> Option<Which> {
    let rest = action.strip_prefix("subagent-open:")?;
    let (id, entry) = match rest.split_once('#') {
        Some((id, entry)) => (id, entry.parse().ok()),
        None => (rest, None),
    };
    Some(Which::SubagentViewer {
        id: id.to_owned(),
        entry,
        from_hub: true,
    })
}

/// Overlay navigation uses the same destinations while idle and during work.
fn overlay_destination(action: &str) -> Option<Which> {
    match action {
        "open:models" => Some(Which::Models),
        "open:roles" => Some(Which::ModelRoles),
        "open:providers" => Some(Which::Providers),
        "open:settings" => Some(Which::Settings),
        "open:model-settings" => Some(Which::ModelSettings),
        "open:context-defaults" => Some(Which::ContextDefaults),
        "open:shortcuts" => Some(Which::Shortcuts),
        "open:compute" => Some(Which::Compute),
        "open:improve" => Some(Which::Improve),
        "open:improve-settings" => Some(Which::LearningSettings),
        "open:improve-models" => Some(Which::LearningModels),
        "open:extensions" => Some(Which::Extensions),
        "subagent-hub" => Some(Which::Subagents),
        "subagent-hub:activity" => Some(Which::SubagentsActivity),
        _ => subagent_viewer_destination(action),
    }
}

/// Work navigation is identical while idle and while a turn is running.
fn work_panel(name: &str, args: &str) -> Option<Which> {
    if !args.is_empty() {
        return None;
    }
    match name {
        "workers" | "agents" | "jobs" | "hub" => Some(Which::Subagents),
        "processes" | "bg" => Some(Which::Jobs),
        "compute" => Some(Which::Compute),
        _ => None,
    }
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
            loop_mode: Default::default(),
            runtime: Default::default(),
            pending_result: None,
            shared_prose: false,
            auth_job: None,
            extensions_pending: None,
            extensions_job: None,
            restart_requested: false,
            retry_noted: false,
            smithery_resume: None,
            commit_job: None,
            term,
            themes,
            keys,
            composer: {
                let mut composer = Composer::new(history, app.config.ui.history_limit);
                composer.set_history_provider(crate::storage::archive::history_provider(
                    &app.layout.user_dir,
                ));
                composer.set_word_completion(app.config.ui.word_completion);
                composer.set_project_root(app.layout.project_root.clone());
                composer.load_learned_words(app.layout.user_dir.join("learned-words.json"));
                composer
            },
            paste_guard: Default::default(),
            transcript: Transcript::new(),
            status: StatusData::default(),
            ui: app.config.ui.clone(),
            usage_status: commands::UsageStatus::new(app.layout.user_dir.clone()),
            catalog: Catalog::new(),
            git: Git::default(),
            learning: Learning::default(),
            yield_compute: false,
            adapter_monitor: None,
            request_policy: Value::Null,
            commands,
            scope: Scope::Global,
            tools_expanded: false,
            home: None,
            started: Instant::now(),
            request_phase: agent::RequestPhase::Waiting,
            last_trajectory: None,
            teach_pending: Vec::new(),
            teach_hint_shown: false,
            teach_state: TeachState::of(app),
            teach_nudge_stage: 0,
            improve_watch: None,
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
            deferred_editor: false,
            deferred_approval_cycle: false,
            deferred_display_reset: false,
            live_tasks: None,
            workers: Default::default(),
            recent_worker: None,
            background_control: None,
            pty_approval: app.config.approval,
            maintenance_control: None,
            skills_listing: String::new(),
            pending_config: None,
            busy_overlay: None,
            pending_verification: None,
            pending_resume: None,
            pending_favorite: None,
            resources: ResourceWatch {
                at: Instant::now() - RESOURCE_TTL,
                settings: None,
                events: resource_events(&app.layout),
                resources: None,
                diagnostic: None,
                reload: settings_store::SettingsReload::new(&app.layout, &app.overlay, &app.config),
            },
            pending_resource_rebuild: false,
            voice_input: None,
            voice_gesture: Default::default(),
            speech: None,
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

    /// Observe disk on a bounded timer. A running agent only receives a pending snapshot.
    fn refresh_resources(&mut self, app: &App, force: bool, busy: bool) -> bool {
        let signalled = self.resources.events.as_ref().is_some_and(|events| {
            events
                .dirty
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        });
        let interval = match &self.resources.events {
            Some(events) if events.complete => RESOURCE_WATCHED_FALLBACK,
            _ => RESOURCE_TTL,
        };
        if !force && !signalled && self.resources.at.elapsed() < interval {
            return false;
        }
        self.resources.at = Instant::now();
        let mut diagnostics = Vec::new();
        let mut changed = false;
        match settings_fingerprint(&app.layout) {
            Ok(stamp) if force || self.resources.settings != Some(stamp) => {
                self.resources.settings = Some(stamp);
                let (config, errors) = self.resources.reload.reload(
                    &app.layout,
                    &app.overlay,
                    app.models.borrow().as_ref(),
                );
                diagnostics.extend(errors);
                // Compare the same coding view used by App::new/adopt. Saved learning
                // preferences differ intentionally and are not an external settings edit.
                let config = config.for_coding();
                let key = config_key(&config);
                let different = key != config_key(&app.config);
                let pending_changed = self
                    .pending_config
                    .as_ref()
                    .is_none_or(|pending| config_key(pending) != key);
                self.pending_config = different.then_some(config);
                if different && pending_changed {
                    self.note(
                        "Settings",
                        if busy {
                            "External settings validated; they will apply after the current turn."
                        } else {
                            "External settings validated; applying them at the idle boundary."
                        },
                    );
                }
                changed = true;
            }
            Ok(_) => {}
            Err(error) => diagnostics.push(format!(
                "Settings change check: {error:#}; keeping last good settings"
            )),
        }
        let stamp = match resources_fingerprint(&app.layout) {
            Ok(stamp) => Some(stamp),
            Err(error) => {
                diagnostics.push(format!("Resource change check: {error:#}"));
                None
            }
        };
        if force || stamp.is_some_and(|stamp| self.resources.resources != Some(stamp)) {
            let had_resources = self.resources.resources.is_some();
            self.resources.resources = stamp;
            diagnostics.extend(self.reload_disk_resources(app));
            // Skills and prompt resources are captured when an agent is constructed.
            self.pending_resource_rebuild |= had_resources || force;
            changed = true;
        }
        let diagnostic = (!diagnostics.is_empty()).then(|| diagnostics.join("\n"));
        if diagnostic != self.resources.diagnostic {
            if let Some(error) = &diagnostic {
                self.note("Reload", error);
                changed = true;
            }
            self.resources.diagnostic = diagnostic;
        }
        if force {
            self.note("Reload", if busy {
                "Disk resources refreshed; validated settings and agent context will apply after the current turn. Invalid edits retain the last good values."
            } else {
                "Disk resources refreshed. Validated settings and agent context apply at this idle boundary; invalid edits retain the last good values."
            });
        }
        changed
    }

    fn reload_disk_resources(&mut self, app: &App) -> Vec<String> {
        let mut diagnostics = Vec::new();
        let key_path = app.layout.user_dir.join("keybindings.toml");
        match fs::read_to_string(&key_path) {
            Ok(text) => match text.parse::<toml::Table>() {
                Ok(table) => {
                    let (keys, warnings) = Keymap::from_table(&table);
                    self.keys = keys;
                    diagnostics.extend(warnings);
                }
                Err(error) => diagnostics.push(format!(
                    "{}: {error}; keeping last good key bindings",
                    key_path.display()
                )),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.keys = Keymap::defaults()
            }
            Err(error) => diagnostics.push(format!(
                "{}: {error}; keeping last good key bindings",
                key_path.display()
            )),
        }
        let themes = ThemeManager::new(&app.layout.user_dir, &self.ui, self.dark);
        if let Some(error) = themes.last_error() {
            diagnostics.push(format!("{error}; keeping last good theme"));
        } else {
            self.themes = themes;
        }
        self.refresh_command_resources(app);
        diagnostics
    }

    fn refresh_command_resources(&mut self, app: &App) {
        // Replace, rather than append, so deletion and same-name overrides agree across
        // completion, palette, help and dispatch.
        self.commands = context::discover_commands(&app.layout);
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
    }

    fn refresh_commands(&mut self, app: &App) {
        self.refresh_command_resources(app);
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

    /// Home screen and, for a resumed session, its earlier conversation.
    fn begin(&mut self, app: &App, session: &Session, resumed: bool) {
        self.session_id = session.id().to_owned();
        if self.ui.welcome {
            let (sessions, recent): (Vec<String>, Vec<(String, String)>) =
                Session::list_newest(&app.layout, 24)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|s| s.id != session.id() && s.messages > 0)
                    // A session that only ever ran a slash command is not a conversation to resume.
                    .filter(|s| !s.first_message.trim_start().starts_with('/'))
                    .map(|s| {
                        // Untitled sessions are named by their first message, else by size.
                        let title = [s.title.trim(), s.first_message.trim()]
                            .into_iter()
                            .find(|t| !t.is_empty())
                            .map_or_else(
                                || format!("untitled · {} messages", s.messages),
                                str::to_owned,
                            );
                        (s.id.clone(), (title, session_age(&s.started)))
                    })
                    .take(3)
                    .unzip();
            // The composer's bottom border already offers commands, actions, model and settings;
            // the home adds only what it does not.
            let tips = if self.ui.tips {
                vec![
                    ("@".to_owned(), "attach files".to_owned()),
                    ("!".to_owned(), "run a shell command".to_owned()),
                    (
                        "/extensions".to_owned(),
                        "MCP servers and plugins".to_owned(),
                    ),
                ]
            } else {
                Vec::new()
            };
            let selection = app.config.selection();
            // The status line reads git in the background, so it is still empty when the home is
            // first drawn; ask git directly (one fast local call).
            let branch = if self.status.branch.is_empty() {
                git_output(
                    &app.layout.project_root,
                    &["symbolic-ref", "--quiet", "--short", "HEAD"],
                )
                .map(|b| b.trim().to_owned())
                .unwrap_or_default()
            } else {
                self.status.branch.clone()
            };
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
                resume_hint: format!(
                    "{} to pick one, Enter to resume · /resume for all",
                    if self.themes.current().ascii() {
                        "Down"
                    } else {
                        "\u{2193}"
                    }
                ),
                setup_hint: format!(
                    "{} or /providers to sign in to a hosted model; /setup installs the local model runtime",
                    self.keys.label(Action::ModelSelect)
                ),
                branch,
                approval: format!("{:?}", app.config.approval).to_lowercase(),
                sandbox: format!("{:?}", app.config.sandbox.mode).to_lowercase(),
            };
            let motion = crate::ui::fx::motion(self.ui.motion, self.themes.current());
            self.home = Some(welcome::Home::new(info, sessions, motion));
        }
        if resumed {
            replay_session(&mut self.transcript, session);
            self.note("Resume", Self::resume_note(session));
        }
    }

    fn after_adopt(&mut self, app: &App) {
        self.teach_changed(app);
        self.ui = app.config.ui.clone();
        self.cfg_approval = app.config.approval;
        self.catalog.refresh(app);
        let themes = ThemeManager::new(&app.layout.user_dir, &app.config.ui, self.dark);
        if let Some(error) = themes.last_error() {
            self.note("Theme", format!("{error}; keeping last good theme"));
        } else {
            self.themes = themes;
        }
        self.refresh_command_resources(app);
        self.pending_resource_rebuild = false;
    }

    fn finish(&mut self) -> Option<String> {
        self.auth_job.take();
        // A read-out or microphone capture must not outlive the shell.
        self.stop_speech();
        if let Some(capture) = self.voice_input.take() {
            capture.stop();
        }
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
        // Workers spawned by a `task` call live in that call's card; the rail keeps the rest
        // (background and scheduled workers that no card owns).
        let snapshots: Vec<crate::runtime::Node> = self
            .runtime
            .bodies("worker")
            .iter()
            .filter(|b| b.read(|n| n.parent.is_some()))
            .map(|b| b.snapshot())
            .collect();
        if !snapshots.is_empty() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            self.transcript.sync_agents(&snapshots, now_ms);
        }
        self.status.workers = Some(self.workers.tally());
        self.status.tick = (self.started.elapsed().as_millis() / 83) as u64;
        self.status.clock = local_time("%H:%M");
        self.status.mode = self.composer.mode();
        self.status.usage = self.usage_status.poll();
        let (cols, rows) = self.term.size();
        // The conversation has started (a message, a resumed or switched session): the home
        // steps aside for its one-line summary, which leads the conversation in scrollback.
        if self.home.is_some() && self.transcript.has_user() {
            let home = self.home.take().expect("checked");
            self.term.commit(home.summary(cols, self.themes.current()));
        }
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
            home,
            ..
        } = self;
        let theme = themes.current();
        let expand_key = keys.label(Action::ToolsExpand);
        let tctx = TranscriptCtx {
            theme,
            ui,
            tick: status.tick,
            tools_expanded: *tools_expanded,
            expand_key: &expand_key,
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
                let worker_cap = match ui.pinned_workers {
                    crate::llm::model_config::PinnedWorkers::Full => {
                        (rows as usize / 3).clamp(2, 12)
                    }
                    _ => (rows as usize / 5).clamp(2, 6),
                }
                .min(available.saturating_sub(bottom.len()));
                if rows as usize >= crate::ui::worker_pins::MIN_TERMINAL_ROWS {
                    bottom.extend(crate::ui::worker_pins::rows(
                        &self.workers,
                        cols as usize,
                        worker_cap,
                        ui.pinned_workers,
                        ui.worker_live_preview,
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
                if let Some(home) = home {
                    let budget = (rows as usize).saturating_sub(view.rows.len() + bottom.len() + 1);
                    if budget > 2 {
                        let mut above_rail = home.rows(cols, budget, theme);
                        above_rail.append(&mut bottom);
                        bottom = above_rail;
                    }
                }
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

    /// Re-reads the worker board (throttled to about once a second) and queues finish notices.
    /// Reads session files, so this runs from the event loop, never from draw. Notices raised while
    /// the subagent hub or viewer is open are dropped: those views already show the change.
    fn refresh_workers(&mut self, app: &App) {
        if self.session_id.is_empty() {
            return;
        }
        let refreshed = self.workers.refresh(
            Instant::now(),
            &app.layout,
            &self.session_id,
            &self.runtime,
            self.live_tasks.as_ref(),
        );
        if !refreshed {
            return;
        }
        let hub_key = self.keys.label(Action::SubagentsHub);
        let subagents_open = self.busy_overlay.as_ref().is_some_and(|busy| {
            matches!(
                busy.overlay,
                ActiveOverlay::Subagents(_) | ActiveOverlay::SubagentViewer(_)
            )
        });
        for line in self.workers.take_notices(&hub_key) {
            if !subagents_open {
                self.note("Workers", line);
            }
        }
    }

    fn refresh_status(&mut self, app: &App, agent: &Agent<'_>) {
        self.refresh_workers(app);
        self.adapter_monitor = agent.adapter_monitor();
        self.request_policy = agent.session().control_state();
        let selection = agent.effective_config().selection();
        let project_root = agent
            .layout()
            .map(|layout| layout.project_root.clone())
            .unwrap_or_else(|| app.layout.project_root.clone());
        let (branch, counts) = self.git.read(&project_root);
        // Learning status is loaded only by an explicit improvement view.
        let status = &mut self.status;
        status.provider = selection.provider.clone();
        status.model = selection
            .display_name()
            .trim_start_matches(&format!("{}/", selection.provider))
            .to_owned();
        status.thinking = reasoning_profile(agent.effective_config())
            .ok()
            .flatten()
            .as_ref()
            .map(thinking_label)
            .unwrap_or_default();
        status.project_path = project_root.display().to_string();
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
        self.pty_approval = if agent.plan_mode() {
            Approval::Readonly
        } else {
            agent.approval()
        };
        status.sandbox = format!("{:?}", app.config.sandbox.mode).to_lowercase();
        status.version = None;
        status.learning.clear();
        status.lora = Lora::Unknown;
        status.role.clear();
        if self.todo != *agent.todo() {
            self.todo = agent.todo().clone();
        }
    }

    /// Shift+Tab: cycles write → auto → readonly → ask → write and reports the new mode.
    fn cycle_approval(&mut self, agent: &mut Agent<'_>) {
        let next = match agent.approval() {
            Approval::Write => Approval::Auto,
            Approval::Auto => Approval::Readonly,
            Approval::Readonly => Approval::Ask,
            Approval::Ask => Approval::Write,
        };
        agent.set_approval(next);
        self.note("Approval", format!("{next:?}").to_lowercase());
    }

    // ----- idle loop ----------------------------------------------------------------------

    fn run(&mut self, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
        self.maintenance_control = Some(agent.background_compaction_control());
        self.background_control = agent.background_control();
        let mut dirty = true;
        let mut previous_status = None;
        loop {
            // Actions pressed during a turn run at this boundary, once the turn has unwound.
            if std::mem::take(&mut self.deferred_editor) {
                self.edit_externally();
            }
            if std::mem::take(&mut self.deferred_approval_cycle) {
                self.cycle_approval(agent);
            }
            if std::mem::take(&mut self.deferred_display_reset) {
                self.display_reset();
            }
            dirty |= self.refresh_resources(app, false, false);
            // Large @file rankings finish on a worker; land them without waiting for a keystroke.
            dirty |= self.composer.poll_autocomplete();
            if let Some(notice) = self.composer.take_word_notice() {
                self.note("Word completion", notice);
            }
            for notice in crate::collab::room::pump()
                .into_iter()
                .chain(crate::llm::oauth::take_notice())
            {
                self.note("Notice", notice);
                dirty = true;
            }
            if let Some(config) = self.pending_config.take() {
                if let step @ Step::Rebuild(_) = self.config_changed(app, agent, config) {
                    return Ok(step);
                }
                dirty = true;
            }
            if std::mem::take(&mut self.pending_resource_rebuild) {
                return Ok(Step::Rebuild(Box::new(app.config.clone())));
            }
            if let Some(selection) = self.pending_verification.take() {
                self.verify_lora(app, agent, &selection)?;
                dirty = true;
            }
            if let Some(payload) = self.pending_resume.take() {
                self.resume_worker(app, agent, &payload)?;
                dirty = true;
            }
            for steer in crate::collab::room::take_steering() {
                match steer {
                    crate::collab::host::Steer::Prompt(text) => {
                        let step = self.run_turn(app, agent, &text)?;
                        if !matches!(step, Step::Done) {
                            return Ok(step);
                        }
                        dirty = true;
                    }
                    // Aborts reach a running turn through `Bridge::pump`; while idle there is nothing to stop.
                    crate::collab::host::Steer::Abort => {}
                }
            }
            if let Some(prompt) = self.loop_due(app, agent, &mut dirty) {
                let step = self.run_turn(app, agent, &prompt)?;
                if !matches!(step, Step::Done) {
                    return Ok(step);
                }
                dirty = true;
            }
            commands::commit::poll(self);
            self.poll_improve_watch();
            if commands::extensions_browser::poll(self, app, agent) {
                dirty = true;
            }
            if self.poll_voice() {
                dirty = true;
            }
            for (label, line) in AuthJob::drain_lines(&mut self.auth_job) {
                self.transcript.notice_merge(label, &line);
                dirty = true;
            }
            // A job cancelled elsewhere (Esc, sign-out, overlay) must not leave its panel pending.
            if self.auth_job.is_none() {
                self.transcript.settle_job_notices();
            }
            if let Some((provider, result)) = AuthJob::collect(&mut self.auth_job, app) {
                if provider == "Model" && result.is_ok() {
                    self.pending_config = Some(app.reload()?);
                }
                let step = match result {
                    Err(error) if provider == "Smithery" => {
                        commands::smithery::browser_failed(self, app, agent, &error)?
                    }
                    // An embedding setup finished while memory is not semantic: report it, then
                    // offer the switch with a single key (the offer does nothing when semantic).
                    Ok(message)
                        if provider == "Setup"
                            && message.starts_with(crate::setup::embeddings::SUMMARY_PREFIX) =>
                    {
                        self.transcript.notice_merge(provider, &message);
                        self.transcript.settle_job_notices();
                        self.command(
                            app,
                            agent,
                            &format!(
                                "/setup memory {}",
                                crate::setup::embeddings::OFFER_SEMANTIC_ARG
                            ),
                        )?
                    }
                    Ok(message) if provider == "Smithery" => {
                        self.note(provider, message);
                        commands::smithery::resume(self, app, agent)?
                    }
                    Ok(message) if message.is_empty() => {
                        if self.transcript.job_waiting(provider) {
                            self.transcript.notice_merge(provider, "Done.");
                        }
                        Step::Done
                    }
                    // Streamed output already said everything; a bare "Finished." adds nothing.
                    Ok(message)
                        if message == "Finished."
                            && self.transcript.notice_has_output(provider) =>
                    {
                        Step::Done
                    }
                    result => {
                        match result {
                            Ok(message) => self.transcript.notice_merge(provider, &message),
                            Err(error) => self
                                .transcript
                                .notice_merge(provider, &format!("{error:#}")),
                        }
                        Step::Done
                    }
                };
                self.transcript.settle_job_notices();
                self.catalog.refresh(app);
                dirty = true;
                if !matches!(step, Step::Done) {
                    return Ok(step);
                }
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
            let maintenance_busy = self.startup.is_some()
                || !self.deferred.is_empty()
                || self.composer.has_queued_work();
            if agent.foreground_idle_pending(maintenance_busy)?
                && !self.wait_compute_required(app, true, true)?
            {
                agent.note_compaction_input();
                continue;
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
                match agent.maintenance_tick(&mut bridge, maintenance_busy) {
                    Ok(Some(_)) => dirty = true,
                    Ok(None) => {}
                    Err(error) => bridge
                        .background_notice(&format!("Idle maintenance did not commit: {error:#}")),
                }
                if let Some(notice) = agent.memory_reachability_notice() {
                    bridge.background_notice(&notice);
                }
                if bridge.failed {
                    anyhow::bail!("terminal draw failed");
                }
            }
            let handoff = self.yield_compute
                || ((crate::learn::compute::background_pending(&app.layout.user_dir)?
                    || crate::learn::compute::foreground_pending(&app.layout.user_dir)?)
                    && app.compute.borrow().is_some());
            // Keep resident models loaded between turns so the next turn starts without reloading.
            // Unload them only when queued training, verification or another session asks for
            // compute; running child agents keep their models and compute lease.
            if self.startup.is_none() && handoff && app.release_idle_compute(agent) {
                self.yield_compute = false;
                self.note("Compute", "Local models unloaded for queued work. This chat stays open; the next turn reloads the model.");
                dirty = true;
            }
            self.refresh_status(app, agent);
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
            if self.busy_overlay.is_some() || dirty || previous_status.as_ref() != Some(&status) {
                if self.busy_overlay.is_some() {
                    self.step_busy_overlay(app, None)?;
                } else {
                    self.paint(None)?;
                }
            }
            previous_status = Some(status);
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
                || self.commit_job.is_some()
                || self.catalog.worker.is_some()
                || self.pending_favorite.is_some()
                || self.busy_overlay.is_some()
                || self.composer.autocomplete_pending();
            let timeout = if status_pending {
                TICK
            } else if left.iter().chain(&right).any(|s| s == "time") {
                Duration::from_secs(1)
            } else {
                STATUS_TTL
            };
            let timeout = agent
                .compaction_wakeup()
                .map_or(timeout, |maintenance| timeout.min(maintenance));
            let timeout = self.loop_mode.next_due().map_or(timeout, |due| {
                timeout.min(due.saturating_duration_since(Instant::now()))
            });
            let timeout = if self.loop_mode.is_awaiting_condition() {
                timeout.min(crate::agent::modes::loop_runner::CONDITION_POLL)
            } else {
                timeout
            };
            let timeout = if self.voice_input.is_some() {
                timeout.min(Duration::from_millis(100))
            } else {
                timeout
            };
            let animating = self.home.as_ref().is_some_and(welcome::Home::animating);
            let timeout = if animating {
                timeout.min(crate::ui::fx::FRAME)
            } else {
                timeout
            };
            if !poll(timeout)? {
                dirty = animating;
                if self.pending_favorite.is_some() {
                    self.catalog.collect();
                    let effective = &app.config;
                    let pending = self.pending_favorite.as_ref().unwrap();
                    if !pending.current(effective, self.scope, self.catalog.request) {
                        self.pending_favorite = None;
                        self.note("Favorites", "Pending switch cancelled because the model, favorites, scope or discovery request changed.");
                        dirty = true;
                    } else if self.catalog.get().is_some() {
                        self.pending_favorite = None;
                        match self.command(app, agent, "/model next")? {
                            Step::Done => dirty = true,
                            other => return Ok(other),
                        }
                    }
                }
                continue;
            }
            dirty = true;
            let event = read()?;
            if let Event::Key(key) = &event
                && self.consume_voice_edge(key)
            {
                continue;
            }
            if self.busy_overlay.is_some() {
                self.step_busy_overlay(app, Some(event))?;
                continue;
            }
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if self.pending_favorite.is_some()
                        && (key.code == KeyCode::Esc
                            || (key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)))
                    {
                        agent.note_compaction_input();
                        self.pending_favorite = None;
                        self.note(
                            "Favorites",
                            "Pending switch cancelled; discovery may still populate the picker.",
                        );
                        continue;
                    }
                    #[cfg(unix)]
                    if key.code == KeyCode::Char('z')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if let Err(error) = self.term.suspend_process() {
                            self.note("Suspend", format!("cannot suspend the terminal: {error:#}"));
                        }
                        continue;
                    }
                    if let Some(picked) = self.home_key(&key) {
                        if let Some(command) = picked {
                            match self.submit(app, agent, command)? {
                                Step::Done => {}
                                other => return Ok(other),
                            }
                        }
                        continue;
                    }
                    let event = self.composer_key(key);
                    let step = match event {
                        ComposerEvent::None => Step::Done,
                        ComposerEvent::Interrupt => {
                            self.pause_loop();
                            self.stop_speech();
                            Step::Done
                        }
                        ComposerEvent::Exit => Step::Quit,
                        ComposerEvent::Submit(text) => self.submit(app, agent, text)?,
                        ComposerEvent::Action(action) => {
                            let result = self.on_action(action, app, agent);
                            agent.note_compaction_input();
                            result?
                        }
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

    /// Keys for the live home while the input is empty: Down enters its list, Up walks back to the
    /// input, Enter picks, Esc leaves. `None` passes the key on; `Some(Some(command))` asks to run it.
    fn home_key(&mut self, key: &crossterm::event::KeyEvent) -> Option<Option<String>> {
        let home = self.home.as_mut()?;
        if !key.modifiers.is_empty()
            || !self.composer.draft().is_empty()
            || self.composer.menu_open()
        {
            return None;
        }
        match key.code {
            KeyCode::Down => home.down().then_some(None),
            KeyCode::Up => home.up().then_some(None),
            KeyCode::Esc => home.deselect().then_some(None),
            KeyCode::Enter => match home.pick()? {
                welcome::Pick::Resume(id) => Some(Some(format!("/resume {id}"))),
                welcome::Pick::Draft(text) => {
                    self.composer.set_draft(text);
                    Some(None)
                }
            },
            _ => {
                // Typing goes to the input; the selection steps aside.
                home.deselect();
                None
            }
        }
    }

    fn composer_key(&mut self, key: crossterm::event::KeyEvent) -> ComposerEvent {
        if let Some(control) = &self.maintenance_control {
            control.note_input();
        }
        let buffered = poll(Duration::ZERO).unwrap_or(false);
        // A burst of keys typed as `/settings` + Enter must submit the command, not insert a line
        // break: the guard only covers Enter when the draft could be an unbracketed multi-line paste.
        let command_enter = key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && crate::ui::input::enter_submits_command(&self.composer.draft());
        let guarded = if command_enter {
            None
        } else {
            self.paste_guard.text_for_key(key, buffered, Instant::now())
        };
        if let Some(text) = guarded {
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
        if let Some(control) = &self.maintenance_control {
            control.note_input();
        }
        self.paste_guard.pasted(Instant::now());
        self.composer.handle_paste(text);
    }

    /// Voice key press (not an auto-repeat): start push-to-talk, or stop the capture that is
    /// running. `poll_voice` inserts the transcript into the draft once the capture finishes.
    fn voice_press(&mut self, app: &App) {
        let release_reporting = crate::ui::term::reports_key_releases();
        match self.voice_gesture.press(release_reporting) {
            crate::voice::gesture::Command::Stop => self.stop_voice(),
            crate::voice::gesture::Command::Start { hold } => {
                if !app.config.voice.stt.enabled {
                    self.voice_gesture.finished();
                    self.note(
                        "Voice",
                        "Voice input is off. Turn on speech recognition under Voice in /settings.",
                    );
                    return;
                }
                match crate::voice::tui::PushToTalk::start(
                    crate::voice::VoiceEnv::from_config(&app.config),
                    hold,
                ) {
                    Ok(capture) => {
                        self.voice_input = Some(capture);
                        self.composer.set_recording(true, hold);
                    }
                    Err(error) => {
                        self.voice_gesture.finished();
                        self.note("Voice", format!("{error:#}"));
                    }
                }
            }
        }
    }

    /// Voice key release: ends a hold-started recording. Releases of toggle recordings are ignored.
    fn voice_release(&mut self) {
        if self.voice_gesture.release().is_some() {
            self.stop_voice();
        }
    }

    fn stop_voice(&self) {
        if let Some(capture) = &self.voice_input {
            capture.stop();
        }
    }

    /// Releases and auto-repeats of the voice key never reach normal key handling: a release ends a
    /// held recording, and a repeat must not toggle it again. Returns true when the key was consumed.
    fn consume_voice_edge(&mut self, key: &crossterm::event::KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release && self.keys.release_matches(Action::VoiceInput, key) {
            self.voice_release();
            return true;
        }
        key.kind == KeyEventKind::Repeat && self.keys.matches(Action::VoiceInput, key)
    }

    /// Collects a finished capture or read-out. Returns true when the screen needs a repaint.
    fn poll_voice(&mut self) -> bool {
        let mut changed = false;
        if let Some(result) = self
            .voice_input
            .as_ref()
            .and_then(|capture| capture.try_result())
        {
            self.voice_input = None;
            self.voice_gesture.finished();
            self.composer.set_recording(false, false);
            changed = true;
            match result {
                Ok(text) if !text.trim().is_empty() => self.composer_paste(text.trim()),
                Ok(_) => self.note("Voice", "No speech was heard."),
                Err(error) => self.note("Voice", format!("{error:#}")),
            }
        }
        if self
            .speech
            .as_ref()
            .is_some_and(|(_, handle)| handle.is_finished())
        {
            changed = true;
            if let Some((_, handle)) = self.speech.take() {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => self.note("Read-out", format!("{error:#}")),
                    Err(_) => self.note("Read-out", "The read-out stopped unexpectedly."),
                }
            }
        }
        changed
    }

    /// Stops a read-out that is still playing. Its thread exits at the next cancel check.
    fn stop_speech(&mut self) {
        if let Some((cancel, _)) = self.speech.take() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Reads the last answer aloud when `voice.tts.auto_speak` is on.
    fn auto_speak(&mut self, app: &App, agent: &Agent<'_>) {
        self.stop_speech();
        if !app.config.voice.tts.auto_speak {
            return;
        }
        let messages = agent.session().messages();
        // Only this turn's messages: a tool-only turn must not re-read an earlier answer.
        let turn_start = messages
            .iter()
            .rposition(|m| m["role"] == "user")
            .map_or(0, |i| i + 1);
        let Some(answer) = messages
            .iter()
            .skip(turn_start)
            .rev()
            .filter(|m| m["role"] == "assistant")
            .find_map(|m| m["content"].as_str().filter(|s| !s.trim().is_empty()))
        else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = crate::voice::tui::speak_in_background(
            crate::voice::VoiceEnv::from_config(&app.config),
            answer.to_owned(),
            cancel.clone(),
        );
        self.speech = Some((cancel, handle));
    }

    fn save_history(&self, app: &App) {
        let _ = settings_store::save_history(
            &app.layout,
            &self.composer.history(),
            self.ui.history_limit,
        );
    }

    /// `Alt+Shift+A`: the transcript of the most recently active running worker. Repeated presses
    /// cycle through the running workers in roster order. With none running, it cycles through all
    /// workers, and with no workers it opens the hub.
    fn open_recent_worker(&mut self, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
        let running: Vec<&str> = self
            .workers
            .agents()
            .iter()
            .filter(|a| !a.status.is_finished())
            .map(|a| a.id.as_str())
            .collect();
        let all: Vec<&str> = self
            .workers
            .agents()
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        let pool = if running.is_empty() { all } else { running };
        let id = crate::ui::worker_pins::next_recent(&pool, self.recent_worker.as_deref());
        let Some(id) = id else {
            return self.open(app, agent, Which::Subagents);
        };
        self.recent_worker = Some(id.clone());
        self.open(
            app,
            agent,
            Which::SubagentViewer {
                id,
                entry: None,
                from_hub: false,
            },
        )
    }

    fn on_action(&mut self, action: Action, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
        match action {
            Action::CommandPalette => self.open(app, agent, Which::Palette),
            Action::SubagentsHub => self.open(app, agent, Which::Subagents),
            Action::SubagentsRecent => self.open_recent_worker(app, agent),
            Action::ModelSelect => self.open(app, agent, Which::Models),
            Action::ModelRoles => self.open(app, agent, Which::ModelRoles),
            Action::ModelFavoriteNext => self.command(app, agent, "/model next"),
            Action::RateGood | Action::RateBad => {
                self.rate_last_turn(app, action == Action::RateGood);
                Ok(Step::Done)
            }
            Action::ProviderSetup => self.open(app, agent, Which::Providers),
            Action::Settings => self.open(app, agent, Which::Settings),
            Action::ToolsExpand => {
                self.tools_expanded = !self.tools_expanded;
                self.display_reset();
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
                self.cycle_approval(agent);
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
            Action::PasteImage => {
                match self.composer.paste_clipboard_image() {
                    Ok(()) => self.note("Image", "clipboard image attached to the draft"),
                    Err(error) => {
                        self.note("Image", format!("clipboard image unavailable: {error:#}"))
                    }
                }
                Ok(Step::Done)
            }
            Action::VoiceInput => {
                self.voice_press(app);
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
            keys,
            ..
        } = self;
        let expand_key = keys.label(Action::ToolsExpand);
        let ctx = TranscriptCtx {
            theme: themes.current(),
            ui,
            tick: status.tick,
            tools_expanded: *tools_expanded,
            expand_key: &expand_key,
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
        let draft_dir = match tempfile::Builder::new().prefix("dial-draft-").tempdir() {
            Ok(dir) => dir,
            Err(error) => {
                self.note(
                    "Editor",
                    format!("cannot create private draft directory: {error}"),
                );
                return;
            }
        };
        if let Err(error) = crate::platform::fs::set_owner_only(draft_dir.path()) {
            self.note("Editor", format!("cannot protect draft directory: {error}"));
            return;
        }
        let path = draft_dir.path().join("draft.md");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let draft_file = crate::platform::fs::open_owner_only(&options, &path);
        let mut draft_file = match draft_file {
            Ok(file) => file,
            Err(error) => {
                self.note(
                    "Editor",
                    format!("cannot create private draft file: {error}"),
                );
                return;
            }
        };
        if let Err(error) =
            std::io::Write::write_all(&mut draft_file, self.composer.expanded_draft().as_bytes())
        {
            let _ = std::fs::remove_file(&path);
            self.note(
                "Editor",
                format!("cannot write the draft for the editor: {error}"),
            );
            return;
        }
        drop(draft_file);
        let ran = self.term.suspend(|| {
            // The editor setting may carry arguments (`code --wait`), so it goes through the shell.
            #[cfg(unix)]
            let mut command = {
                let mut command = Command::new("/bin/sh");
                command
                    .arg("-c")
                    .arg(format!("{editor} \"$1\""))
                    .arg("dial-editor")
                    .arg(&path);
                command
            };
            #[cfg(windows)]
            let mut command = {
                let mut command = Command::new("pwsh");
                command
                    .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
                    .arg(format!("& {editor} $env:DIAL_EDITOR_FILE"))
                    .env("DIAL_EDITOR_FILE", &path);
                command
            };
            command.status()
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
        if let Err(error) = crate::storage::archive::record_prompt(
            &app.layout.user_dir,
            &agent.layout().unwrap_or(&app.layout).project_root,
            Some(agent.session().id()),
            &trimmed,
        ) {
            self.note(
                "History",
                format!("could not record prompt history: {error:#}"),
            );
        }
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
        let layout = agent
            .layout()
            .context("agent has no project layout")?
            .clone();
        layout.ensure_project_outside_storage()?;
        let ws = Workspace::new(&layout.project_root)
            .and_then(|ws| {
                tools::apply_sandbox_config(
                    ws,
                    &app.config.sandbox,
                    Some((layout.state_dir.as_path(), layout.user_dir.as_path())),
                )
                .map(|ws| ws.with_shell_backend(app.config.shell.backend))
            })
            .and_then(|ws| {
                Ok(
                    ws.with_artifacts(crate::storage::artifact::ArtifactStore::new(
                        &layout.state_dir.join("artifacts"),
                    )?),
                )
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
                                execution: Default::default(),
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
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                // Quiet output still owns input: keys, paste and resize go through
                                // the same dispatcher a noisy command uses in `repaint`, so the
                                // draft, queue and configured interrupt behave identically.
                                bridge.pump();
                                bridge.heartbeat()
                            }
                            Err(_) => {
                                break ToolOutput {
                                    execution: Default::default(),
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
                execution: Default::default(),
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

    /// Returns the loop prompt to re-submit once the armed iteration is due. A `--while` / `--until`
    /// condition runs on a worker and its verdict is picked up on a later tick. Reports a stop or a
    /// pause, and clears an interrupt flag that a cancelled condition may have set.
    fn loop_due(&mut self, app: &App, agent: &Agent<'_>, dirty: &mut bool) -> Option<String> {
        use crate::agent::modes::loop_runner::{self, Due, Gate};
        // Esc cancels only a condition that is already running; otherwise the flag is stale.
        if !self.loop_mode.is_awaiting_condition() {
            app.interrupt.store(false, Ordering::SeqCst);
        }
        let cwd = agent.workspace().root().to_path_buf();
        let interrupt = app.interrupt.clone();
        let due = self.loop_mode.take_due(Instant::now(), |condition| {
            Gate::start(condition, &cwd, interrupt, loop_runner::CONDITION_TIMEOUT)
        });
        match due {
            Due::Idle => None,
            Due::Submit(prompt) => Some(prompt),
            Due::Stop(message) | Due::Paused(message) => {
                self.note("Loop", message);
                *dirty = true;
                None
            }
        }
    }

    /// Esc while idle suspends an enabled loop.
    fn pause_loop(&mut self) {
        if self.loop_mode.is_enabled() {
            self.loop_mode.pause();
            self.note("Loop", "Loop paused. Send a prompt to resume.");
        }
    }

    /// A session change ends the loop: its prompt and budget belong to the old session.
    /// `disable` (not `pause`) so the next prompt typed in the new session is not captured as the loop prompt.
    pub(crate) fn stop_loop_for_session_change(&mut self) {
        // /undo and /outcome score the last turn of the session that ran it, never a session we left.
        self.last_trajectory = None;
        if self.loop_mode.disable() {
            self.note(
                "Loop",
                "Loop stopped: the session changed. Start it again with /loop.",
            );
        }
    }

    /// "Teach this model": settle earlier turns whose changes were committed since, then note HEAD
    /// and the checkpoint counter so the end of this turn can tell what it changed.
    fn teach_turn_start(&mut self, app: &App, agent: &Agent<'_>) -> Option<TeachStart> {
        app.store.as_ref()?;
        self.settle_commits(app, agent);
        let root = agent.workspace().root().to_path_buf();
        Some(TeachStart {
            base: crate::learn::signals::head(&|args| git_output(&root, args)),
            checkpoint: agent
                .checkpoints()
                .map(|checkpoints| checkpoints.next_turn()),
        })
    }

    fn teach_turn_end(&mut self, app: &App, agent: &Agent<'_>, trajectory: i64, start: TeachStart) {
        if let (Some(base), Some(first), Some(checkpoints)) =
            (start.base, start.checkpoint, agent.checkpoints())
        {
            // Every checkpointed turn since the start belongs to this one (workers included).
            let mut changed: Vec<String> = (first..checkpoints.next_turn())
                .flat_map(|turn| checkpoints.changed_files(turn))
                .collect();
            changed.sort();
            changed.dedup();
            let root = agent.workspace().root().to_path_buf();
            let git = |args: &[&str]| git_output(&root, args);
            if let Some(change) =
                crate::learn::signals::capture(&git, &root, trajectory, base, &changed)
            {
                self.teach_pending.push(change);
                let excess = self.teach_pending.len().saturating_sub(TEACH_PENDING_TURNS);
                self.teach_pending.drain(..excess);
                // The turn may have committed its own work.
                self.settle_commits(app, agent);
            }
        }
        if !self.teach_hint_shown {
            self.teach_hint_shown = true;
            self.note(
                "Teaching",
                format!(
                    "{} marks a turn good and {} bad. Committing a turn's changes also counts as good; /undo counts as bad.",
                    self.keys.label(Action::RateGood),
                    self.keys.label(Action::RateBad)
                ),
            );
        }
        self.teach_nudge(app);
    }

    /// Announce what changed for "Teach this model". Each model has its own learning database, so
    /// turn references do not survive a model change.
    fn teach_changed(&mut self, app: &App) {
        let now = TeachState::of(app);
        let before = std::mem::replace(&mut self.teach_state, now.clone());
        if before.model != now.model {
            self.last_trajectory = None;
            self.teach_pending.clear();
        }
        if before == now {
            return;
        }
        let message = if !now.wanted {
            if !before.active {
                return;
            }
            "Teaching off: sessions are no longer recorded.".to_owned()
        } else if !app.config.teaching() {
            format!(
                "Teach this model works with local models; {} is hosted, so this session is not recorded.",
                now.model
            )
        } else if let Some(error) = &app.store_error {
            format!("Teaching is on, but the learning store could not open: {error}")
        } else if before.active && before.model == now.model {
            return;
        } else {
            format!(
                "Teaching {}: turns you approve become training examples. /improve train trains and tests a new version; it serves only if it scores better.",
                now.model
            )
        };
        self.note("Teaching", message);
    }

    /// Credit pending turns whose exact changes have since been committed.
    fn settle_commits(&mut self, app: &App, agent: &Agent<'_>) {
        let Some(store) = app.store.as_ref() else {
            return;
        };
        if self.teach_pending.is_empty() {
            return;
        }
        let root = agent.workspace().root().to_path_buf();
        let git = |args: &[&str]| git_output(&root, args);
        let (mut kept, mut errors) = (0, Vec::new());
        self.teach_pending.retain(|change| {
            if crate::learn::signals::committed(&git, change) != Some(true) {
                return true;
            }
            let outcome = Outcome {
                trajectory: change.trajectory,
                score: 100.0,
                source: "git:commit".into(),
                reference: None,
            };
            match store.implicit_outcome(&outcome) {
                Ok(true) => kept += 1,
                Ok(false) => {}
                Err(error) => errors.push(format!("{error:#}")),
            }
            false
        });
        match kept {
            0 => {}
            1 => self.note(
                "Teaching",
                "Committed a turn's changes: saved as a good example.",
            ),
            n => self.note(
                "Teaching",
                format!("Committed {n} turns' changes: saved as good examples."),
            ),
        }
        for error in errors {
            self.note("Teaching", format!("could not record the commit: {error}"));
        }
    }

    /// Notes the approved-turn count after a turn, once at three quarters of the training threshold
    /// and again on reaching it, so a session hears about it at most twice.
    fn teach_nudge(&mut self, app: &App) {
        let Some(store) = app.store.as_ref() else {
            return;
        };
        let Ok(approved) = store.verified_live_turns() else {
            return;
        };
        let needed = crate::learn::improve::min_examples(&app.config);
        let stage = if approved >= needed {
            2
        } else if approved * 4 >= needed * 3 {
            1
        } else {
            0
        };
        if stage == 0 || stage <= self.teach_nudge_stage {
            return;
        }
        self.teach_nudge_stage = stage;
        let next = if stage == 2 {
            "enough to train: /improve session starts training on this session's approved turns"
        } else {
            "/improve session approves this session's turns"
        };
        self.note("Teaching", format!("{approved}/{needed} approved · {next}"));
    }

    /// Polls the job started by `/improve cycle` or `train`; notes its outcome once. The scheduler
    /// state is read at most every two seconds, and a queued or running job is not finished yet.
    fn poll_improve_watch(&mut self) {
        let Some(watch) = self.improve_watch.as_mut() else {
            return;
        };
        if watch.checked.elapsed() < Duration::from_secs(2) {
            return;
        }
        watch.checked = Instant::now();
        let Ok(store) = Store::open_read_only(&watch.state_dir) else {
            return;
        };
        let status = store
            .get_meta("auto_status")
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or(Value::Null);
        let state = status["state"].as_str().unwrap_or("");
        if matches!(state, "" | "queued" | "running")
            || status["at"].as_u64().unwrap_or(0) < watch.started
        {
            return;
        }
        let error = agent::truncate(status["error"].as_str().unwrap_or("no details"), 300);
        let live = status["active"].as_i64().map_or_else(
            || "the live version is unchanged".to_owned(),
            |id| format!("v{id} is live"),
        );
        let message = match state {
            "idle" => format!("Improvement cycle finished; {live}. /improve status shows the cycle report."),
            "paused" => format!("Improvement paused: {error}. It resumes when the budget allows; /improve enable resumes it now."),
            "stopped" => "Improvement stopped at your request.".to_owned(),
            "disabled" => "Improvement did not run: automatic improvement is paused. /improve enable resumes it.".to_owned(),
            "failed" => format!("Improvement failed: {error}. /improve status shows the last error."),
            other => format!("Improvement ended ({other})."),
        };
        self.improve_watch = None;
        self.note("Improvement", message);
    }

    /// `/improve status` text. Callers settle pending commits first when they have an agent.
    fn improve_status_text(&mut self, app: &App) -> String {
        self.learning.read(app);
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
        summary
    }

    /// `/improve` and its subcommands. Bare `/improve` opens the panel. Status, jobs and stop also run
    /// during a turn (see `live_command`).
    fn improve_command(&mut self, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
        let (sub, rest) = args
            .split_once(char::is_whitespace)
            .map_or((args, ""), |(sub, rest)| (sub, rest.trim()));
        match sub {
            "" => return self.open(app, agent, Which::Improve),
            "settings" => return self.open(app, agent, Which::LearningSettings),
            "models" => return self.open(app, agent, Which::LearningModels),
            "status" => {
                self.settle_commits(app, agent);
                let text = self.improve_status_text(app);
                self.note("Improvement", text);
            }
            "jobs" => {
                let text = improve_jobs_text(app);
                self.note("Improvement", text);
            }
            "stop" => {
                let text = improve_stop_text(app, rest);
                self.note("Improvement", text);
            }
            "cycle" | "train" | "session" | "enable" | "disable" | "versions" | "tasks"
            | "rollback" => {
                let text = self
                    .improve_action(app, sub, rest)
                    .unwrap_or_else(|error| format!("{error:#}"));
                self.note("Improvement", text);
            }
            other => self.note(
                "Improvement",
                format!(
                    "Unknown /improve action \"{other}\". Use status, jobs, stop <id>, cycle, train, session, enable, disable, versions, tasks, rollback [version], settings or models. Bare /improve opens the panel."
                ),
            ),
        }
        Ok(Step::Done)
    }

    fn start_improve_watch(&mut self, learning: &crate::config::types::Config) {
        self.improve_watch = Some(ImproveWatch {
            state_dir: learning.state_dir.clone(),
            started: crate::learn::improve::now(),
            checked: Instant::now() - Duration::from_secs(2),
        });
    }

    /// The `/improve` actions that start work, change the switch or read the learning store. Returns
    /// the note text. Starting work needs Enabled on; the Enabled switch is never changed here.
    fn improve_action(&mut self, app: &App, sub: &str, rest: &str) -> Result<String> {
        let learning = app.reload()?;
        let store = App::open_store(&learning)?;
        let off = format!(
            "Automatic improvement is off for this model, so nothing starts. Turn on Enabled in {} first.",
            crate::learn::improve::SETTINGS_PATH
        );
        match sub {
            "session" => {
                let approval = store.approve_session(&self.session_id)?;
                let mut text = format!(
                    "This session: {} turn(s) approved for training, {} already verified, {} kept with an existing lower score, {} not usable (failed or no saved request).",
                    approval.approved, approval.already, approval.judged, approval.unusable
                );
                let verified = store.verified_live_turns()?;
                let needed = crate::learn::improve::min_examples(&learning);
                if verified < needed {
                    text.push_str(&format!(
                        "\n{verified}/{needed} verified turns recorded for this model; training starts once there are {needed}. Run /improve session again after more turns."
                    ));
                    return Ok(text);
                }
                if !learning.improve.enabled {
                    text.push_str(&format!("\nTraining not started. {off}"));
                    return Ok(text);
                }
                ensure!(
                    !crate::learn::compute::paused(&app.layout.user_dir),
                    "{text}\nResume background work first"
                );
                let spawn = auto::start_training(&app.layout, &learning, &store)
                    .map_err(|error| anyhow::anyhow!("{text}\nTraining not started: {error:#}"))?;
                match spawn {
                    auto::Spawn::Started => {
                        self.yield_compute = app.config.uses_local_compute();
                        self.start_improve_watch(&learning);
                        text.push_str("\nTraining and evaluation queued in the background; coding keeps priority. Follow it with /improve jobs; cancel with /improve stop.");
                    }
                    auto::Spawn::Refused(reason) => text.push_str(&format!(
                        "\nTraining not started: {}",
                        reason.message(&learning.state_dir)
                    )),
                }
                Ok(text)
            }
            "cycle" | "train" => {
                ensure!(learning.improve.enabled, "{}", off);
                ensure!(
                    !crate::learn::compute::paused(&app.layout.user_dir),
                    "Resume background work first"
                );
                let spawn = if sub == "train" {
                    auto::start_training(&app.layout, &learning, &store)?
                } else {
                    auto::start_cycle(&app.layout, &learning, &store)?
                };
                spawn.ensure_started(&learning.state_dir)?;
                self.yield_compute = app.config.uses_local_compute();
                self.start_improve_watch(&learning);
                let job = if sub == "train" {
                    "Training and evaluation"
                } else {
                    "Improvement cycle"
                };
                Ok(format!(
                    "{job} queued in the background; coding keeps priority. Follow it with /improve jobs; cancel with /improve stop. You will get a note here when it finishes."
                ))
            }
            "enable" => {
                auto::resume(&learning.state_dir)?;
                store.set_meta("auto_enabled", "true")?;
                auto::maybe_spawn(&app.layout, &learning, &store)?;
                Ok(if learning.improve.enabled {
                    "Automatic improvement resumed for this project.".to_owned()
                } else {
                    format!(
                        "Automatic improvement resumed, but Enabled is still off. Turn it on in {} to run cycles.",
                        crate::learn::improve::SETTINGS_PATH
                    )
                })
            }
            "disable" => {
                store.set_meta("auto_enabled", "false")?;
                Ok("Automatic improvement paused for this project; a running cycle is not interrupted.".to_owned())
            }
            "versions" => {
                let live = store.active().ok().map(|version| version.id);
                let versions = store.versions()?;
                if versions.is_empty() {
                    return Ok("No versions recorded yet.".to_owned());
                }
                Ok(versions
                    .iter()
                    .map(|version| version_line(version, live))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "tasks" => {
                let tasks = crate::learn::repository::load(&learning)?;
                if tasks.is_empty() {
                    return Ok("No repository exercises registered.".to_owned());
                }
                Ok(tasks
                    .iter()
                    .map(|task| {
                        format!(
                            "{} · {} · {} files · {}",
                            task.spec.id,
                            task.spec.split,
                            task.files.len(),
                            task.digest.chars().take(12).collect::<String>()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "rollback" => {
                let confirmed = rest.split_whitespace().any(|word| word == "--yes");
                let wanted = rest
                    .split_whitespace()
                    .find(|word| *word != "--yes")
                    .map(|word| word.trim_start_matches('v').parse::<i64>())
                    .transpose()
                    .context("usage: /improve rollback [version]")?;
                let active = store.active()?;
                let parent = active
                    .parent
                    .context("the live version has no previous version to restore")?;
                ensure!(
                    wanted.is_none_or(|id| id == parent),
                    "only v{parent} can be restored: it is the previous version of the live v{}. /improve versions lists them.",
                    active.id
                );
                if !confirmed
                    && self.ask(
                        "Roll back learning",
                        &format!(
                            "Make v{parent} live again. v{} stays recorded; the change applies from the next turn.",
                            active.id
                        ),
                        &[("Roll back", 'r'), ("Cancel", 'c')],
                    ) != Some(0)
                {
                    return Ok(format!("Rollback cancelled. v{} stays live.", active.id));
                }
                // The same checks as `dial improve rollback`: no cycle may hold the version store.
                let control = auto::lock_file(&learning.state_dir.join("control.lock"))?;
                ensure!(
                    fs2::FileExt::try_lock_exclusive(&control).is_ok(),
                    "an improvement job is running; stop it with /improve stop first"
                );
                crate::learn::improve::verify_adapter(&store, &store.version(parent)?)?;
                let restored = store.rollback()?;
                Ok(format!(
                    "Rolled back: v{} is live again; v{} stays recorded. The change applies from the next turn.",
                    restored.id, active.id
                ))
            }
            other => anyhow::bail!("unknown /improve action {other}"),
        }
    }
    /// Alt+Up / Alt+Down: rate the last finished turn for "Teach this model".
    fn rate_last_turn(&mut self, app: &App, good: bool) {
        let Some(store) = app.store.as_ref() else {
            let message = if !app.config.improve.teach {
                TEACH_OFF.to_owned()
            } else if !app.config.teaching() {
                format!(
                    "Teach this model works with local models; {} is hosted, so this turn was not recorded.",
                    app.config.model
                )
            } else if let Some(error) = &app.store_error {
                format!("Teaching is on, but the learning store could not open: {error}")
            } else {
                TEACH_OFF.to_owned()
            };
            self.note("Teaching", message);
            return;
        };
        let Some(trajectory) = self.last_trajectory else {
            self.note("Teaching", "No finished turn to rate yet.");
            return;
        };
        let outcome = Outcome {
            trajectory,
            score: if good { 100.0 } else { 0.0 },
            source: "user:rating".into(),
            reference: None,
        };
        match store.outcome(&outcome) {
            Ok(()) => {
                self.teach_pending
                    .retain(|change| change.trajectory != trajectory);
                self.note(
                    "Teaching",
                    if good {
                        "Marked the last turn good: saved as a training example."
                    } else {
                        "Marked the last turn bad: it will not be used as an example."
                    },
                );
                self.start_live_monitor("Teaching", app.config.clone());
            }
            Err(error) => self.transcript.error(&format!("{error:#}")),
        }
    }

    fn run_turn(&mut self, app: &App, agent: &mut Agent<'_>, text: &str) -> Result<Step> {
        // A joined guest's prompts belong to the host agent: send them there or refuse. The local
        // agent never runs them.
        if let Some(routed) = crate::collab::room::route_guest_prompt(text) {
            let message = match routed {
                Ok(()) => "Sent to the host agent; its replies arrive as collab entries.",
                Err(message) => message,
            };
            self.note("Collab", message);
            return Ok(Step::Done);
        }
        self.loop_mode.turn_started(text);
        self.retry_noted = false;
        platform::interrupt::clear_interrupt();
        app.interrupt.store(false, Ordering::SeqCst);
        self.stop_speech();
        self.live_tasks = Some(agent.live_task_control());
        self.background_control = agent.background_control();
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
                self.loop_mode.pause();
                return Ok(Step::Done);
            }
            Err(error) => {
                self.transcript.error(&format!("{error:#}"));
                self.composer.set_busy(false);
                self.loop_mode.pause();
                return Ok(Step::Done);
            }
        }
        self.status.phase = Phase::Working;
        self.turn_output_tokens = 0;
        self.status.output_tokens = 0;
        self.status.tokens_per_second = None;
        self.status.activity = "thinking".into();
        let teach = self.teach_turn_start(app, agent);
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
        crate::collab::room::sync_host(agent.session());
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
                if let (Some(trajectory), Some(start)) = (report.trajectory, teach) {
                    self.teach_turn_end(app, agent, trajectory, start);
                }
            }
            Err(error) => self.transcript.error(&format!("{error:#}")),
        }
        // Notify from the agent's outcome, not from transport success: failed, blocked and
        // incomplete turns are errors, and an interrupted turn is silent (see `turn_end_notice`).
        let outcome = match &result {
            Ok(report) if report.interrupted => crate::agent::TurnOutcome::Interrupted,
            Ok(report) => report.outcome,
            Err(_) => crate::agent::TurnOutcome::Failed,
        };
        if let Some(notice) =
            crate::ui::termcaps::turn_end_notice(outcome, self.ui.notify, self.ui.notify_errors)
        {
            crate::ui::termcaps::notify(notice.kind, "Dial", notice.body, &self.session_id);
        }
        self.transcript.finish_turn();
        if matches!(&result, Ok(report) if !report.interrupted) {
            self.auto_speak(app, agent);
        }
        if result.is_ok() {
            self.transcript.turn_stats(TurnStats {
                clock: local_time("%Y-%m-%d %H:%M:%S"),
                elapsed_secs: elapsed,
                prompt_tokens: self.status.prompt_tokens,
                output_tokens: (streamed_tokens > 0).then_some(streamed_tokens),
                tokens_per_second: self.status.tokens_per_second,
                outcome: None,
            });
        }
        let halted = result.as_ref().map_or(true, |report| report.interrupted);
        self.loop_mode.turn_settled(Instant::now(), halted);
        if failed {
            anyhow::bail!("terminal draw failed; the screen was restored");
        }
        Ok(Step::Done)
    }

    // ----- asking the user ------------------------------------------------------------------

    /// A modal choice drawn in place of the composer. `None` means the user cancelled.
    fn ask(&mut self, title: &str, body: &str, options: &[(&str, char)]) -> Option<usize> {
        self.ask_with_cancel(title, body, options, None)
    }

    fn ask_with_cancel(
        &mut self,
        title: &str,
        body: &str,
        options: &[(&str, char)],
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<usize> {
        if options.is_empty() {
            return None;
        }
        let mut body_offset = 0usize;
        let mut selected = 0usize;
        let mut show_raw = false;
        loop {
            if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
                return None;
            }
            let shown = if title == RECOVERY_PANEL {
                format!(
                    "{}\n\n{} {}",
                    crate::ui::recovery::panel_text(body, show_raw),
                    self.keys.label(Action::ToolsExpand),
                    if show_raw {
                        "to hide details"
                    } else {
                        "for details"
                    }
                )
            } else {
                body.to_owned()
            };
            let (cols, height) = self.term.size();
            let rows = panel_rows_window(
                self.themes.current(),
                cols,
                title,
                &shown,
                options,
                selected,
                body_offset,
                height.saturating_sub(6) as usize,
            );
            if self.paint(Some(rows)).is_err() {
                return None;
            }
            if !poll(TICK).ok()? {
                continue;
            }
            match read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    // Page by the body rows actually visible, keeping one row of overlap, so a short
                    // terminal never skips details the user has not seen.
                    let down = self.keys.matches(Action::SelectPageDown, &key);
                    if down || self.keys.matches(Action::SelectPageUp, &key) {
                        let (cols, height) = self.term.size();
                        let lines =
                            text::wrap_words(&text::sanitize(&shown), panel_inner(cols)).len();
                        let visible =
                            panel_body_limit(options.len(), height.saturating_sub(6) as usize);
                        body_offset = panel_scroll(body_offset, lines, visible, down);
                        continue;
                    }
                    if title == RECOVERY_PANEL && self.keys.matches(Action::ToolsExpand, &key) {
                        show_raw = !show_raw;
                        body_offset = 0;
                        continue;
                    }
                    if self.keys.matches(Action::Dismiss, &key) {
                        return None;
                    }
                    if self.keys.matches(Action::SelectUp, &key) {
                        selected = (selected + options.len() - 1) % options.len();
                        continue;
                    }
                    if self.keys.matches(Action::SelectDown, &key) {
                        selected = (selected + 1) % options.len();
                        continue;
                    }
                    match key.code {
                        KeyCode::Up => selected = (selected + options.len() - 1) % options.len(),
                        KeyCode::Down | KeyCode::Tab => selected = (selected + 1) % options.len(),
                        KeyCode::Enter => return Some(selected),
                        KeyCode::Esc => return None,
                        KeyCode::Char('c') if ctrl => return None,
                        KeyCode::Char(c) if !ctrl => {
                            if let Some(index) = options.iter().position(|(_, hotkey)| {
                                *hotkey != ' ' && hotkey.eq_ignore_ascii_case(&c)
                            }) {
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
        self.wait_compute_required(app, app.config.uses_local_compute(), false)
    }

    fn wait_compute_required(
        &mut self,
        app: &App,
        required: bool,
        maintenance: bool,
    ) -> Result<bool> {
        if !app.config.compute_locks || !required || app.compute.borrow().is_some() {
            return Ok(true);
        }
        let _request = crate::learn::compute::Request::foreground(&app.layout.user_dir)?;
        let lock = auto::lock_file(&app.layout.user_dir.join("compute.lock"))?;
        let waiting_since = Instant::now();
        let elapsed_before_wait = self.status.elapsed_secs;
        let idle_input = self
            .maintenance_control
            .as_ref()
            .and_then(|control| control.input_time());
        loop {
            if maintenance
                && (self.startup.is_some()
                    || !self.deferred.is_empty()
                    || self.composer.has_queued_work()
                    || self
                        .maintenance_control
                        .as_ref()
                        .and_then(|control| control.input_time())
                        != idle_input)
            {
                return Ok(false);
            }
            match fs2::FileExt::try_lock_shared(&lock) {
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
            self.paint_waiting(app)?;
            if poll(TICK)? {
                let event = read()?;
                match self.wait_event(app, event)? {
                    WaitFlow::Continue => {}
                    WaitFlow::Cancel => {
                        self.note(
                            "Cancelled",
                            "An improvement or training job holds local compute exclusively.",
                        );
                        return Ok(false);
                    }
                    WaitFlow::Stop => return Ok(false),
                }
            }
        }
    }

    /// Keep painting and accepting input while local preparation owns a subprocess. Typed text
    /// queues in the composer (the busy composer), and the configured interrupt or Escape cancels.
    fn wait_local_preparation(
        &mut self,
        app: &App,
        work: impl FnOnce(Arc<AtomicBool>, &mut dyn FnMut(&str)) -> Result<String> + Send + 'static,
    ) -> Result<bool> {
        let mut slot = None;
        start_streaming_job(&mut slot, "Local models", work);
        self.status.phase = Phase::Setup;
        self.composer.set_busy(true);
        let outcome = self.preparation_events(app, &mut slot);
        self.composer.set_busy(false);
        outcome
    }

    fn preparation_events(&mut self, app: &App, slot: &mut Option<AuthJob>) -> Result<bool> {
        loop {
            for (label, line) in AuthJob::drain_lines(slot) {
                self.transcript.notice_merge(label, &line);
            }
            if let Some(job) = slot.as_ref() {
                match job.receiver.try_recv() {
                    Ok(result) => {
                        self.transcript.settle_job_notices();
                        result?;
                        return Ok(true);
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        anyhow::bail!("local preparation worker stopped unexpectedly")
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
            self.status.activity = "preparing local model · Esc cancels".into();
            self.paint_waiting(app)?;
            if poll(TICK)? {
                let event = read()?;
                // Escape and the configured interrupt cancel preparation, unless a panel is open
                // (then Escape closes the panel first, as everywhere else).
                let cancel_key = self.busy_overlay.is_none()
                    && matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release
                        && (self.keys.matches(Action::Interrupt, key)
                            || self.keys.matches(Action::SelectCancel, key)));
                let flow = if cancel_key {
                    WaitFlow::Cancel
                } else {
                    self.wait_event(app, event)?
                };
                match flow {
                    WaitFlow::Continue => {}
                    WaitFlow::Cancel => {
                        *slot = None;
                        self.note("Local models", "Preparation cancelled; settings unchanged. Cached downloads can be reused.");
                        return Ok(false);
                    }
                    // A message sent now is queued in `startup` and runs once preparation stops.
                    WaitFlow::Stop => {
                        *slot = None;
                        return Ok(false);
                    }
                }
            }
        }
    }

    /// One terminal event during a blocking wait. An open panel gets it first, as in the idle
    /// loop: Escape closes the panel, while the configured interrupt cancels the wait. Otherwise
    /// keys, paste and resize use the composer and command routing that turns use, so typed and
    /// queued work is kept rather than discarded.
    fn wait_event(&mut self, app: &App, event: Event) -> Result<WaitFlow> {
        if self.busy_overlay.is_some() {
            let interrupt = matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release
                && self.keys.matches(Action::Interrupt, key)
                && !self.keys.matches(Action::SelectCancel, key));
            if interrupt {
                if let Some(state) = &mut self.busy_overlay {
                    state.next = None;
                }
                self.finish_busy_overlay(app)?;
                return Ok(WaitFlow::Cancel);
            }
            return Ok(if self.step_busy_overlay(app, Some(event))? {
                WaitFlow::Stop
            } else {
                WaitFlow::Continue
            });
        }
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.wait_key(app, key),
            Event::Paste(text) => {
                self.composer_paste(&text);
                Ok(WaitFlow::Continue)
            }
            Event::Resize(..) => {
                self.term.on_resize();
                Ok(WaitFlow::Continue)
            }
            _ => Ok(WaitFlow::Continue),
        }
    }

    fn wait_key(&mut self, app: &App, key: crossterm::event::KeyEvent) -> Result<WaitFlow> {
        if self.keys.matches(Action::Interrupt, &key) {
            return Ok(WaitFlow::Cancel);
        }
        Ok(match self.composer_key(key) {
            ComposerEvent::Interrupt | ComposerEvent::Exit => WaitFlow::Cancel,
            ComposerEvent::Submit(text) => {
                if self.route_busy_command(app, text) {
                    WaitFlow::Stop
                } else {
                    WaitFlow::Continue
                }
            }
            ComposerEvent::Action(Action::CommandPalette) => self.wait_command(app, "/palette"),
            ComposerEvent::Action(Action::ModelSelect) => self.wait_command(app, "/model"),
            ComposerEvent::Action(Action::ModelRoles) => self.wait_command(app, "/roles"),
            ComposerEvent::Action(Action::ModelFavoriteNext) => {
                self.wait_command(app, "/model next")
            }
            ComposerEvent::Action(Action::Settings) => self.wait_command(app, "/settings"),
            ComposerEvent::Action(Action::SubagentsHub) => self.wait_command(app, "/agents"),
            ComposerEvent::Action(Action::EditExternal) => {
                self.edit_externally();
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::PasteImage) => {
                if let Err(error) = self.composer.paste_clipboard_image() {
                    self.note("Image", format!("clipboard image unavailable: {error:#}"));
                }
                WaitFlow::Continue
            }
            _ => WaitFlow::Continue,
        })
    }

    fn wait_command(&mut self, app: &App, command: &str) -> WaitFlow {
        if self.route_busy_command(app, command.to_owned()) {
            WaitFlow::Stop
        } else {
            WaitFlow::Continue
        }
    }

    /// Repaint during a blocking wait: an open panel draws itself, otherwise the transcript.
    fn paint_waiting(&mut self, app: &App) -> Result<()> {
        if self.busy_overlay.is_some() {
            self.step_busy_overlay(app, None)?;
            Ok(())
        } else {
            self.paint(None)
        }
    }

    fn prepare_local(&mut self, app: &App) -> Result<bool> {
        if !app.config.runs_dial_mlx() {
            return Ok(true);
        }
        if !app.config.python.is_file() || !app.config.worker.is_file() {
            let choice = self.ask("Set up local models",
                "Installs a private Python environment with uv under your Dial home directory. Downloads need network access.",
                &[("Install runtime", 'y'), ("Cancel", 'n')]);
            if choice != Some(0) {
                return Ok(false);
            }
            self.note("Local models", "installing the local runtime…");
            let layout = app.layout.clone();
            if !self.wait_local_preparation(app, move |cancel, progress| {
                let progress = RefCell::new(progress);
                crate::setup::ensure_runtime_with(&layout, &cancel, &|line| {
                    progress.borrow_mut()(line)
                })?;
                Ok(String::new())
            })? {
                return Ok(false);
            }
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
                self.note("Local models", "resolving and checking the local model…");
                let config = app.config.clone();
                let capabilities = Arc::new(Mutex::new(None));
                let prepared = capabilities.clone();
                if !self.wait_local_preparation(app, move |cancel, progress| {
                    let progress = RefCell::new(progress);
                    crate::llm::download::ensure_local_model_with(
                        &config.python,
                        &config.model,
                        &crate::llm::download::Choice::Auto,
                        &cancel,
                        &|line| progress.borrow_mut()(line),
                    )?;
                    let caps = crate::learn::worker::capabilities_controlled(&config, &cancel)?;
                    ensure!(
                        caps["inference"] == true,
                        "local model is not supported by this runtime: {}",
                        caps["reason"]
                    );
                    *prepared.lock().unwrap_or_else(|e| e.into_inner()) = Some(caps);
                    Ok(String::new())
                })? {
                    return Ok(false);
                }
                if let Some(store) = &app.store
                    && let Some(caps) = capabilities
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                {
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
                "Select a downloaded local MLX checkpoint: LoRA verification needs MLX weights. Remote endpoints and GGUF files cannot be probed here.",
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
        let _lease = if app.config.compute_locks {
            let lock = auto::lock_file(&app.layout.user_dir.join("compute.lock"))?;
            if let Err(error) = fs2::FileExt::try_lock_shared(&lock) {
                self.note("LoRA verification", format!("An improvement or training job holds local compute; retry F6 when it finishes. {error}"));
                return Ok(());
            }
            Some(ProbeLease(lock))
        } else {
            None
        };
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
        let mut which = which;
        // A settings change that needs a rebuild is carried while the overlay chain keeps
        // navigating, so the requested panel still opens and the rebuild still happens.
        let mut carried = Step::Done;
        loop {
            let outcome = self.run_overlay(app, Some(&mut *agent), which)?;
            if let Some(config) = outcome.latest {
                carried = self.config_changed(app, agent, config);
            }
            if let Some(selection) = outcome.verify_lora {
                self.verify_lora(app, agent, &selection)?;
                return Ok(carried);
            }
            if let Some(command) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("command:"))
            {
                return Ok(match self.command(app, agent, command)? {
                    Step::Done => carried,
                    step => step,
                });
            }
            if let Some(prompt) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("turn:"))
            {
                return Ok(match self.submit(app, agent, prompt.to_owned())? {
                    Step::Done => carried,
                    step => step,
                });
            }
            if let Some(payload) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("subagent-resume:"))
            {
                let id = payload.split('\n').next().unwrap_or_default().to_owned();
                self.resume_worker(app, agent, payload)?;
                // The viewer stays open and tails the resumed run.
                which = Which::SubagentViewer {
                    id,
                    entry: None,
                    from_hub: true,
                };
                continue;
            }
            if let Some(id) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("test:"))
            {
                // The catalog scan probes each provider's endpoint; the reopened page shows the result.
                self.catalog.refresh(app);
                self.note(
                    "Connection test",
                    format!("Checking {id}. Its status appears in the provider list when the check finishes."),
                );
                which = Which::Providers;
                continue;
            }
            match outcome.next.as_deref().and_then(overlay_destination) {
                Some(destination) => which = destination,
                None => return Ok(carried),
            }
        }
    }

    /// `/git [revision]`: the panel on the working tree, or read-only on a pinned revision.
    pub(crate) fn open_git(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        revision: &str,
    ) -> Result<Step> {
        let which = Which::Git {
            root: agent.workspace().root().to_path_buf(),
            revision: revision.to_owned(),
        };
        self.open(app, agent, which)
    }

    /// Apply UI/approval now, then adopt the authoritative config at this safe boundary.
    fn config_changed(&mut self, app: &App, agent: &mut Agent<'_>, fresh: Config) -> Step {
        let fresh = fresh.for_coding();
        self.ui = fresh.ui.clone();
        self.composer.set_history_limit(fresh.ui.history_limit);
        self.composer.set_word_completion(fresh.ui.word_completion);
        let themes = ThemeManager::new(&app.layout.user_dir, &fresh.ui, self.dark);
        if let Some(error) = themes.last_error() {
            self.note("Theme", format!("{error}; keeping last good theme"));
        } else {
            self.themes = themes;
        }
        if fresh.approval != self.cfg_approval {
            self.cfg_approval = fresh.approval;
            agent.set_approval(fresh.approval);
        }
        if config_key(&fresh) != config_key(&app.config) {
            Step::Rebuild(Box::new(fresh))
        } else {
            Step::Done
        }
    }

    fn make_overlay(&mut self, app: &App, which: Which) -> Result<ActiveOverlay> {
        self.refresh_resources(app, false, self.status.phase != Phase::Idle);
        if matches!(
            which,
            Which::Models
                | Which::ModelRoles
                | Which::LearningModels
                | Which::ModelDownloads
                | Which::Providers
        ) {
            self.catalog.start(app);
        }
        let effective = match app.reload() {
            Ok(config) => config,
            Err(error) => {
                self.note("Settings", format!("{error:#}; using last good settings"));
                self.resources.reload.config().clone()
            }
        };
        let discovery = self.catalog.get();
        let learning_settings = matches!(which, Which::LearningSettings);
        let context_defaults = matches!(which, Which::ContextDefaults);
        let learning_models = matches!(which, Which::LearningModels);
        let overlay = match which {
            Which::Choice(spec) => ActiveOverlay::Palette(PaletteOverlay::choice(*spec)),
            Which::Palette => ActiveOverlay::Palette(PaletteOverlay::new(
                palette_entries(&self.commands, &self.skills),
                self.status.phase != Phase::Idle,
            )),
            Which::Compute => ActiveOverlay::Compute(ComputeOverlay::new()),
            Which::Improve => ActiveOverlay::Improve(Box::new(
                crate::ui::overlay::improve::ImproveOverlay::new(improve_view(app)),
            )),
            Which::Subagents => ActiveOverlay::Subagents(Box::new(
                crate::ui::subagent_hub::SubagentHub::new(self.session_id.clone()),
            )),
            Which::SubagentsActivity => ActiveOverlay::Subagents(Box::new(
                crate::ui::subagent_hub::SubagentHub::open_activity(self.session_id.clone()),
            )),
            Which::SubagentViewer {
                id,
                entry,
                from_hub,
            } => ActiveOverlay::SubagentViewer(Box::new(
                crate::ui::overlay::subagent_viewer::SubagentViewer::new(
                    id,
                    self.session_id.clone(),
                    entry,
                    from_hub,
                    self.ui.clone(),
                    std::time::Instant::now(),
                ),
            )),
            Which::Extensions => {
                ActiveOverlay::Extensions(Box::new(commands::extensions_browser::overlay(
                    app.layout(),
                    &effective,
                    self.extensions_job.as_ref(),
                )))
            }
            Which::Shortcuts => ActiveOverlay::Shortcuts(ShortcutOverlay::new()),
            Which::Jobs => {
                ActiveOverlay::Jobs(Box::new(crate::ui::overlay::jobs::JobsOverlay::new()))
            }
            Which::Git { root, revision } => ActiveOverlay::Git(Box::new(
                crate::ui::overlay::git::GitOverlay::open(
                    crate::git_panel::Repo::new(&root, &effective),
                    (!revision.is_empty()).then_some(revision.as_str()),
                )
                .map_err(anyhow::Error::msg)?,
            )),
            Which::ModelSettings => {
                let (name, profile) = model_settings_profile(&effective);
                let mut overlay =
                    crate::ui::overlay::model_settings::ModelSettingsOverlay::new(name, profile);
                if effective.assistance.local.suggestions {
                    overlay = overlay
                        .with_suggestions(&effective.user_dir, local_suggestion_base(&effective));
                }
                ActiveOverlay::ModelSettings(Box::new(overlay))
            }
            Which::Settings | Which::LearningSettings | Which::ContextDefaults => {
                let (values, global, project) = settings_tables(app, &effective)?;
                let themes = self
                    .themes
                    .names()
                    .into_iter()
                    .filter(|e| e.error.is_none())
                    .map(|e| e.name)
                    .collect();
                let overlay = SettingsOverlay::new(values, project, global, self.scope, themes);
                let mut overlay = if learning_settings {
                    overlay.learning_only()
                } else {
                    overlay.coding_only()
                };
                if context_defaults {
                    overlay = overlay.context_defaults();
                }
                apply_settings_hints(&mut overlay, &effective);
                overlay.set_setup_offers(setting_setup_offers(&app.layout.user_dir, &effective));
                // The Embedding model row lists discovered embedding models and curated setups.
                if effective.memory.backend
                    == crate::config::memory_backend::MemoryBackendId::Semantic
                {
                    self.catalog.start(app);
                }
                overlay.set_embedding_models(settings_embedding_models(discovery.as_ref()));
                overlay.set_embedding_setups(embedding_setup_offers(&app.layout.user_dir));
                ActiveOverlay::Settings(Box::new(overlay))
            }
            Which::Models | Which::ModelRoles | Which::LearningModels | Which::ModelDownloads => {
                let mut models =
                    ModelOverlay::new(discovery, effective.role_statuses(), self.scope)
                        .with_favorites(favorite_routes(&effective))
                        .with_memory_required(crate::config::memory_backend::requires_memory_role(
                            &effective.memory,
                        ))
                        .with_main_lora(self.status.lora)
                        .with_learning(learning_models)
                        .with_downloads(
                            app.layout(),
                            crate::stats::local_models::Settings::from_config(&effective),
                            effective.python.clone(),
                        );
                models.set_memory_optional(
                    crate::config::memory_backend::auto_retain_uses_memory_role(&effective.memory),
                );
                if matches!(which, Which::ModelRoles) {
                    models.show_roles();
                }
                if matches!(which, Which::ModelDownloads) {
                    models.show_downloads();
                }
                ActiveOverlay::Models(Box::new(models))
            }
            Which::Providers => {
                let user_dir = app.layout.user_dir.clone();
                let has_key = Box::new(move |url: &str| {
                    catalog::credential_token(&user_dir, url)
                        .ok()
                        .flatten()
                        .is_some()
                });
                let credential_sources_for_overlay = credential_sources(
                    &app.layout.user_dir,
                    &effective,
                    discovery.as_ref().map_or(&[], |d| d.providers.as_slice()),
                );
                let mut providers =
                    ProviderOverlay::new(discovery, effective.providers, has_key, self.scope);
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
                providers.set_credential_sources(credential_sources_for_overlay);
                ActiveOverlay::Providers(providers)
            }
        };
        Ok(overlay)
    }

    fn run_overlay(
        &mut self,
        app: &App,
        agent: Option<&mut Agent<'_>>,
        which: Which,
    ) -> Result<OverlayOutcome> {
        let compute = matches!(which, Which::Compute);
        self.term.set_overlay_mouse(self.ui.mouse);
        let mut overlay = self.make_overlay(app, which)?;
        let Shell {
            term,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark,
            auth_job,
            background_control,
            pty_approval,
            extensions_pending,
            extensions_job,
            live_tasks,
            runtime,
            ..
        } = self;
        // Browsing panels keep resident models loaded; only an explicit compute action releases them.
        let mut release = overlay_compute_release(app, agent);
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
            release_compute: release.as_mut().map(|release| release as &mut dyn FnMut()),
            seen_generation: 0,
            auth_job,
            background_control: background_control.as_ref(),
            pty_approval: *pty_approval,
            live_tasks: live_tasks.as_ref(),
            runtime,
            extensions_pending,
            extensions_job,
            compute_monitor: if compute {
                Some(ComputeMonitor::new(app))
            } else {
                None
            },
        };
        env.seen_generation = env.catalog.generation;
        let result = term.with_overlay(|ot| run_overlay(ot, &mut env, &mut overlay));
        env.themes.restore();
        result?;
        Ok(OverlayOutcome {
            latest: env.latest,
            next: env.next,
            verify_lora: env.verify_lora,
        })
    }

    fn start_busy_overlay(&mut self, app: &App, which: Which) -> Result<()> {
        if self.busy_overlay.is_some() {
            return Ok(());
        }
        let compute = matches!(which, Which::Compute);
        let overlay = self.make_overlay(app, which)?;
        self.term.set_overlay_mouse(self.ui.mouse);
        self.term.open_overlay()?;
        self.busy_overlay = Some(BusyOverlay {
            overlay,
            paste_guard: Default::default(),
            compute_monitor: compute.then(|| ComputeMonitor::new(app)),
            verify_lora: None,
            latest: None,
            next: None,
            seen_generation: self.catalog.generation,
        });
        Ok(())
    }

    /// A single bounded panel step; never waits for terminal input or agent completion.
    fn step_busy_overlay(&mut self, app: &App, event: Option<Event>) -> Result<bool> {
        let Some(mut state) = self.busy_overlay.take() else {
            return Ok(false);
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
            background_control,
            pty_approval,
            extensions_pending,
            extensions_job,
            live_tasks,
            runtime,
            ..
        } = self;
        let mut env = OverlayEnv {
            compute_monitor: state.compute_monitor.take(),
            verify_lora: state.verify_lora.take(),
            latest: state.latest.take(),
            next: state.next.take(),
            seen_generation: state.seen_generation,
            auth_job,
            background_control: background_control.as_ref(),
            pty_approval: *pty_approval,
            live_tasks: live_tasks.as_ref(),
            runtime,
            extensions_pending,
            extensions_job,
            app,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark: *dark,
            release_compute: None,
        };
        refresh_overlay(&mut env, &mut state.overlay);
        let redraw = event.is_none();
        let closed = event.is_some_and(|event| {
            overlay_input(&mut env, &mut state.overlay, &mut state.paste_guard, event)
        });
        let draw_result = if redraw && !closed {
            term.draw_overlay_placed(|frame| draw_overlay_frame(frame, &env, &mut state.overlay))
        } else {
            Ok(())
        };
        state.compute_monitor = env.compute_monitor.take();
        state.verify_lora = env.verify_lora.take();
        state.latest = env.latest.take();
        state.next = env.next.take();
        state.seen_generation = env.seen_generation;
        self.busy_overlay = Some(state);
        draw_result?;
        if closed {
            self.finish_busy_overlay(app)
        } else {
            Ok(false)
        }
    }

    fn finish_busy_overlay(&mut self, app: &App) -> Result<bool> {
        let Some(state) = self.busy_overlay.take() else {
            return Ok(false);
        };
        self.themes.restore();
        self.term.close_overlay()?;
        if let Some(config) = state.latest {
            self.pending_config = Some(config);
            self.note("Settings", "Saved; changes apply after the current turn.");
        }
        if let Some(selection) = state.verify_lora {
            self.pending_verification = Some(selection);
            self.note(
                "LoRA verification",
                "Verification runs after the current turn releases compute.",
            );
        }
        let next_panel = state.next.as_deref().and_then(overlay_destination);
        if let Some(which) = next_panel {
            self.start_busy_overlay(app, which)?;
        } else if let Some(command) = state
            .next
            .as_deref()
            .and_then(|next| next.strip_prefix("command:"))
        {
            return Ok(self.route_busy_command(app, command.to_owned()));
        } else if let Some(payload) = state
            .next
            .as_deref()
            .and_then(|next| next.strip_prefix("subagent-resume:"))
        {
            let id = payload.split('\n').next().unwrap_or_default().to_owned();
            self.pending_resume = Some(payload.to_owned());
            self.note(
                "Subagents",
                "The resume starts after the current turn finishes.",
            );
            self.start_busy_overlay(
                app,
                Which::SubagentViewer {
                    id,
                    entry: None,
                    from_hub: true,
                },
            )?;
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------

impl Shell {
    fn note(&mut self, title: &str, text: impl AsRef<str>) {
        self.transcript.notice(title, text.as_ref());
    }

    /// Opens a background job's panel; its progress and result fill it in place.
    pub(crate) fn note_pending(&mut self, title: &str) {
        self.transcript.notice_pending(title);
    }

    /// Runs the live rollback monitor on a background job. Its adapter digest hashes whole weight
    /// files, which must not block the UI thread. The job opens its own store connection; a quiet
    /// monitor produces an empty result, which is not noted.
    fn start_live_monitor(&mut self, label: &'static str, config: Config) {
        let started = start_streaming_job(&mut self.auth_job, label, move |_cancel, _progress| {
            let store = Store::open(
                &config.state_dir,
                &config.model,
                &config.use_case,
                &config.harness,
            )?;
            Ok(match crate::learn::improve::monitor_now(&store, &config)? {
                Some(rollback) => format!("{rollback}; it applies from the next turn"),
                None => String::new(),
            })
        });
        if !started {
            self.note(
                label,
                "live monitor skipped: another background operation is still running",
            );
        }
    }

    /// Queue an `extensions:` action from the browser. The main loop starts it, because starting
    /// needs the app's config and layout.
    fn queue_extensions(
        pending: &mut Option<commands::extensions_browser::Pending>,
        scope: Scope,
        payload: &str,
        overlay: &mut ActiveOverlay,
    ) {
        let intent =
            match serde_json::from_str::<crate::ui::overlay::extensions::model::Intent>(payload) {
                Ok(intent) => intent,
                Err(error) => {
                    overlay.banner(format!("could not read that action: {error}"), true);
                    return;
                }
            };
        let progress = commands::extensions_browser::queue(pending, intent, scope);
        if let ActiveOverlay::Extensions(panel) = overlay {
            panel.attach(progress);
        }
    }

    /// Persist settings writes, reload the effective config and apply it live.
    pub(crate) fn persist(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        changes: &[Change],
        scope: Scope,
    ) -> Result<Step> {
        app.save_settings(scope, changes, None)?;
        let fresh = app.reload()?;
        Ok(self.config_changed(app, agent, fresh))
    }

    const SEARCH_USAGE: &str = "Search past sessions in this project.\nUsage: /search <words>\nExample: /search parser error";
    const OUTCOME_USAGE: &str = "Rate the last completed turn so improvement runs learn from it.\nUsage: /outcome <0-100> [note]\nExample: /outcome 90 fixed the bug on the first try";

    /// `/help`: how to start, then keys, then turn behaviour, then every command. Rows are
    /// `name  description` pairs so the transcript lays them out as aligned columns.
    fn help_text(&self, all: bool) -> String {
        const LEFT: usize = 24;
        let row = |left: &str, right: &str| format!("  {left:<LEFT$}  {right}");
        let palette = self.keys.label(Action::CommandPalette);
        let mut lines = vec![
            "Getting started".to_owned(),
            row(
                "Type and press Enter",
                "ask for anything: explain, fix, build, review",
            ),
            row("/", "open the command menu; keep typing to filter"),
            row(&palette, "search slash commands"),
            row("@path", "attach a file to the message"),
            row(
                "!command",
                "run a shell command yourself; output joins the conversation",
            ),
            row("!!command", "run a shell command without adding its output"),
            String::new(),
            "Keys".to_owned(),
        ];
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
            lines.push(row(&self.keys.labels(action), action.describe()));
        }
        lines.push(row("/hotkeys", "view and rebind every shortcut"));
        lines.push(String::new());
        lines.push("While a turn is running".to_owned());
        lines.push(row("Enter", "steer the current turn with your message"));
        lines.push(row(
            &self.keys.labels(Action::FollowUp),
            "queue a follow-up for when the turn ends",
        ));
        lines.push(row(
            "Up",
            "edit the last queued message (Esc cancels the edit)",
        ));
        lines.push(row(
            "Stops the turn",
            &STOPS_TURN
                .iter()
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(" "),
        ));
        lines.push(row(
            "Runs at once",
            &RUNS_NOW
                .iter()
                .chain(RUNS_NOW_ARGS)
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(" "),
        ));
        lines.push(row(
            &format!("/model or {}", self.keys.label(Action::ModelSelect)),
            "opens now; the choice applies after the turn",
        ));
        lines.push(row("Anything else", "runs when the turn finishes"));
        // Commands, grouped the same way as the action palette. Advanced sections stay folded
        // unless asked for, so the first screen of help is about everyday work.
        let entries = command_entries(&self.commands, &self.skills);
        let command_row = |entry: &CommandEntry| -> String {
            let name = format!("/{}", entry.name);
            match &entry.args_hint {
                Some(hint) if name.len() + 1 + hint.len() <= LEFT => {
                    row(&format!("{name} {hint}"), &entry.description)
                }
                Some(hint) => format!(
                    "{}\n{}",
                    row(&name, &entry.description),
                    row("", &format!("usage: {name} {hint}"))
                ),
                None => row(&name, &entry.description),
            }
        };
        let mut folded = Vec::new();
        for category in crate::ui::command_surface::Category::ALL {
            let members: Vec<&CommandEntry> = entries
                .iter()
                .filter(|entry| {
                    entry.kind == CommandKind::Builtin
                        && crate::ui::command_surface::category(&entry.name) == category
                })
                .collect();
            if members.is_empty() {
                continue;
            }
            if category.advanced() && !all {
                folded.push(format!("{} ({})", category.title(), members.len()));
                continue;
            }
            lines.push(String::new());
            lines.push(category.title().to_owned());
            lines.extend(members.into_iter().map(command_row));
        }
        let custom: Vec<&CommandEntry> = entries
            .iter()
            .filter(|entry| entry.kind != CommandKind::Builtin)
            .collect();
        if !custom.is_empty() {
            lines.push(String::new());
            lines.push("Your commands and skills".to_owned());
            lines.extend(custom.into_iter().map(command_row));
        }
        if !folded.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "More commands in {}. /help all lists them; Ctrl+P searches everything.",
                folded.join(" and ")
            ));
        }
        lines.push(String::new());
        lines.push(
            "/reload re-reads settings, commands, skills and themes from disk (files are also checked every two seconds; invalid edits keep the last good settings).".into(),
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
        STOPS_TURN.contains(&canonical_command(name))
    }

    fn queue_command(&mut self, line: &str) {
        let items = self.composer.queued_messages();
        if items.is_empty() {
            self.note("Queue", "No queued messages. While busy, Enter steers the current task and the follow-up shortcut queues the next task.");
            return;
        }
        let words: Vec<_> = line.split_whitespace().collect();
        let requested = words
            .get(2)
            .and_then(|number| number.parse::<usize>().ok())
            .and_then(|number| number.checked_sub(1));
        let selected = if words.len() == 1 {
            let labels: Vec<_> = items
                .iter()
                .enumerate()
                .map(|(index, (message, follow_up))| {
                    format!(
                        "{} {}: {}",
                        index + 1,
                        if *follow_up { "follow-up" } else { "steering" },
                        crate::ui::text::ellipsize(&crate::ui::text::one_line(message), 72, true)
                    )
                })
                .collect();
            let options: Vec<_> = labels
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    (
                        label.as_str(),
                        char::from_digit((index + 1) as u32, 36).unwrap_or('?'),
                    )
                })
                .collect();
            self.ask(
                "Queued messages",
                "Choose a message to edit or remove. Esc keeps the queue unchanged.",
                &options,
            )
        } else {
            requested.filter(|index| *index < items.len())
        };
        let Some(index) = selected else {
            if words.len() > 1 {
                self.note(
                    "Queue",
                    "Use /queue, /queue edit <number> or /queue remove <number>. Queue unchanged.",
                );
            }
            return;
        };
        let action = if words.get(1) == Some(&"edit") {
            Some(0)
        } else if words.get(1) == Some(&"remove") {
            Some(1)
        } else if words.len() == 1 {
            self.ask(
                "Queued message",
                &items[index].0,
                &[("Edit", 'e'), ("Remove", 'r'), ("Keep", 'k')],
            )
        } else {
            None
        };
        match action {
            Some(0) if self.composer.edit_queued(index) => self.note("Queue", "Message moved to the composer for editing. Your other queued messages are unchanged."),
            Some(0) => self.note("Queue", "Draft kept. Clear or send it before editing a queued message."),
            Some(1) => { self.composer.remove_queued(index); self.note("Queue", "Removed only the selected queued message."); }
            _ => {}
        }
    }

    /// Shared by inference and local-compute waiting. Only explicit session/work changes, or an
    /// explicit send-now of a queued message, cancel the running turn.
    fn route_busy_command(&mut self, app: &App, text: String) -> bool {
        // Plain Submit while busy is the explicit empty-Enter queue flush.
        // Stop at the next safe point and dispatch it in this same chat, ahead
        // of deferred commands and the remaining queued messages.
        if !text.trim_start().starts_with('/') {
            self.startup = Some(text);
            return true;
        }
        if text.split_whitespace().next() == Some("/reload") {
            if text.trim() == "/reload" {
                self.refresh_resources(app, true, true);
            } else {
                self.note("Reload", "Usage: /reload");
            }
            return false;
        }
        if text.split_whitespace().next().is_some_and(|word| {
            RUNS_NOW.contains(&word.trim_start_matches('/')) && word == "/queue"
        }) {
            self.queue_command(&text);
            return false;
        }
        if text.trim() == "/model restart" {
            self.note("Model", "Finish or cancel the current turn before restarting its managed model. No server was stopped.");
            return false;
        }
        let overlay = match text.trim() {
            "/palette" => Some(Which::Palette),
            "/model" | "/models" => Some(Which::Models),
            "/roles" => Some(Which::ModelRoles),
            "/model settings" => Some(Which::ModelSettings),
            "/model defaults" => Some(Which::ContextDefaults),
            "/providers" | "/provider" => Some(Which::Providers),
            "/settings" => Some(Which::Settings),
            "/model-settings" => Some(Which::ModelSettings),
            "/extensions" => Some(Which::Extensions),
            _ => text.trim().strip_prefix('/').and_then(|rest| {
                let (name, args) = rest
                    .split_once(char::is_whitespace)
                    .map_or((rest, ""), |(name, args)| (name, args.trim()));
                work_panel(name, args)
            }),
        };
        if let Some(which) = overlay {
            if let Err(error) = self.start_busy_overlay(app, which) {
                self.note("Panel", format!("{error:#}"));
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
            ("help" | "?", "" | "all") => {
                let text = self.help_text(args.trim() == "all");
                self.note("Help", text);
            }
            ("hotkeys", "") => {
                let text = self.hotkeys_text();
                self.note("Hotkeys", text);
            }
            ("todo", "") => {
                let text = if self.todo.is_empty() {
                    "No tasks yet. The agent adds a checklist here when work has several steps."
                        .to_owned()
                } else {
                    self.todo.render()
                };
                self.note("Todo", text);
            }
            ("search", "") => self.note("Search", Self::SEARCH_USAGE),
            ("search", query) if !query.is_empty() => {
                match Session::search(&app.layout, query, 8) {
                    Ok(hits) if hits.is_empty() => {
                        self.note("Search", format!("No past messages match \"{query}\"."))
                    }
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
            ("improve", "status") => {
                let text = self.improve_status_text(app);
                self.note("Improvement", text);
            }
            ("improve", "jobs") => {
                let text = improve_jobs_text(app);
                self.note("Improvement", text);
            }
            ("improve", rest) if rest == "stop" || rest.starts_with("stop ") => {
                let text = improve_stop_text(app, rest.trim_start_matches("stop"));
                self.note("Improvement", text);
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
            ("jobs" | "workers", id) => {
                let result = match &self.live_tasks {
                    Some(control) => control.status(Some(id)),
                    None => Err("no live jobs".to_owned()),
                };
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", error),
                }
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
        replay_session(&mut self.transcript, session);
        self.session_id = session.id().to_owned();
    }

    fn pick_session(
        &mut self,
        agent: &mut Agent<'_>,
        query: &str,
        tree: bool,
        direct: bool,
    ) -> Result<Option<Session>> {
        use crate::ui::session_picker::Action as Pick;
        let layout = agent
            .layout()
            .context("agent has no project layout")?
            .clone();
        let current = agent.session().id().to_owned();
        if direct && !query.is_empty() {
            let matches = Session::find_prefix(&layout, query)?;
            match matches.as_slice() {
                [only] if only.id == current => {
                    let name = Self::session_name(&only.title, &only.id);
                    self.note("Session", format!("Already in session {name}"));
                    return Ok(None);
                }
                [only] => return Session::open(&only.path).map(Some),
                // No match, or an ambiguous prefix: the picker opens with the prefix as its search.
                _ => {}
            }
        }
        let mut scope = (false, PICKER_SESSION_LIMIT);
        let show_empty = direct && !query.is_empty();
        let mut picker =
            Self::load_session_picker(&layout, &current, tree, scope, query, show_empty)?;
        if picker.sessions.is_empty() {
            self.note("Sessions", "no sessions yet");
            return Ok(None);
        }
        loop {
            let (cols, height) = self.term.size();
            if let Some((id, path)) = picker.wanted_preview(cols as usize, height as usize) {
                picker.set_preview(id, Session::preview_turns(&path).unwrap_or_default());
            }
            let rows = picker.rows(cols as usize, height as usize, self.themes.current());
            self.paint(Some(rows))?;
            let event = read()?;
            if matches!(event, Event::Resize(..)) {
                self.term.on_resize();
            }
            match picker.handle(&event, height as usize) {
                Pick::Cancel => return Ok(None),
                Pick::Resume(index) => {
                    let session = &picker.sessions[index];
                    let (id, path) = (session.id.clone(), session.path.clone());
                    let name = Self::session_name(&session.title, &id);
                    if id == current {
                        self.note("Session", format!("Already in session {name}"));
                        return Ok(None);
                    }
                    if path.parent() != Some(layout.sessions_dir().as_path()) {
                        let project = session.project.clone();
                        picker.set_notice(format!(
                            "{name} belongs to {project}. Run dial there to resume it, or /move it here."
                        ));
                        continue;
                    }
                    match Session::open(&path) {
                        Ok(session) => return Ok(Some(session)),
                        // The picker stays open so another session can be chosen.
                        Err(error) => picker.set_notice(format!("{error:#}")),
                    }
                }
                Pick::Pin(index) => {
                    let id = picker.sessions[index].id.clone();
                    match crate::storage::session_pins::toggle(&layout.user_dir, &id) {
                        Ok(pinned) => picker.set_pinned(index, pinned),
                        Err(error) => picker.set_notice(format!("{error:#}")),
                    }
                }
                Pick::Rename(index, title) => {
                    let (id, path) = {
                        let session = &picker.sessions[index];
                        (session.id.clone(), session.path.clone())
                    };
                    let result = if id == current {
                        agent.session_mut().rename(&title)
                    } else {
                        Session::open(&path).and_then(|mut session| session.rename(&title))
                    };
                    let notice = match result {
                        Ok(()) => format!("Renamed to {title}."),
                        Err(error) => format!("Rename failed: {error:#}"),
                    };
                    Self::reload_session_picker(
                        &mut picker,
                        &layout,
                        &current,
                        tree,
                        scope,
                        Some(notice),
                    )?;
                }
                Pick::Fork(index) => {
                    let (id, path, name) = {
                        let session = &picker.sessions[index];
                        (
                            session.id.clone(),
                            session.path.clone(),
                            Self::session_name(&session.title, &session.id),
                        )
                    };
                    let forked = if id == current {
                        agent.session().fork(&layout)
                    } else {
                        Session::read(&path).and_then(|source| source.fork(&layout))
                    };
                    let notice = match forked {
                        Ok(fork) => format!("Forked {name} as {}.", short(fork.id())),
                        Err(error) => format!("Fork failed: {error:#}"),
                    };
                    Self::reload_session_picker(
                        &mut picker,
                        &layout,
                        &current,
                        tree,
                        scope,
                        Some(notice),
                    )?;
                }
                Pick::Delete(index) => {
                    let (id, path, name) = {
                        let session = &picker.sessions[index];
                        (
                            session.id.clone(),
                            session.path.clone(),
                            Self::session_name(&session.title, &session.id),
                        )
                    };
                    if id == current {
                        picker.set_notice("Use /delete to delete the current session.");
                        continue;
                    }
                    let body = format!("Delete {name}? Its file is removed and cannot be resumed.");
                    if self.ask(
                        "Delete session?",
                        &body,
                        &[("Cancel", 'n'), ("Delete", 'd')],
                    ) != Some(1)
                    {
                        continue;
                    }
                    // The pin is dropped only after the journal is gone.
                    let removed = Session::open(&path)
                        .and_then(|session| session.remove_file())
                        .and_then(|()| crate::storage::session_pins::unpin(&layout.user_dir, &id));
                    let notice = match removed {
                        Ok(()) => format!("Deleted {name}."),
                        Err(error) => format!("Delete failed: {error:#}"),
                    };
                    Self::reload_session_picker(
                        &mut picker,
                        &layout,
                        &current,
                        tree,
                        scope,
                        Some(notice),
                    )?;
                }
                Pick::Scope => {
                    scope = (!scope.0, PICKER_SESSION_LIMIT);
                    Self::reload_session_picker(&mut picker, &layout, &current, tree, scope, None)?;
                }
                Pick::LoadAll => {
                    scope.1 = usize::MAX;
                    Self::reload_session_picker(&mut picker, &layout, &current, tree, scope, None)?;
                }
                Pick::None => {}
            }
        }
    }

    /// The resume picker for one scope `(all projects, per-kind limit)`. Pins, open-elsewhere marks
    /// and the journal count behind "showing N of M" are read here, never in the picker.
    fn load_session_picker(
        layout: &Layout,
        current: &str,
        tree: bool,
        scope: (bool, usize),
        query: &str,
        show_empty: bool,
    ) -> Result<crate::ui::session_picker::Picker> {
        let (all, limit) = scope;
        let sessions = if all {
            Session::list_for_picker_all(layout, limit)?
        } else {
            Session::list_for_picker(layout, limit)?
        };
        let total = Session::count_for_picker(layout, all)?;
        let open_elsewhere: std::collections::HashSet<String> = sessions
            .iter()
            .filter(|s| s.id != current && Session::open_elsewhere(&s.path))
            .map(|s| s.id.clone())
            .collect();
        let pinned = crate::storage::session_pins::load(&layout.user_dir)?;
        Ok(
            crate::ui::session_picker::Picker::new(sessions, pinned, current, query, tree)
                .scope(all, total)
                .with_open_elsewhere(open_elsewhere)
                .with_show_empty(show_empty),
        )
    }

    /// Rebuilds the picker for `scope`, keeping the highlighted session when it is still listed.
    fn reload_session_picker(
        picker: &mut crate::ui::session_picker::Picker,
        layout: &Layout,
        current: &str,
        tree: bool,
        scope: (bool, usize),
        notice: Option<String>,
    ) -> Result<()> {
        let keep = picker.selected_id().map(str::to_owned);
        let mut next = Self::load_session_picker(
            layout,
            current,
            tree,
            scope,
            picker.query(),
            picker.show_empty(),
        )?;
        if let Some(id) = keep {
            next.select_id(&id);
        }
        if let Some(notice) = notice {
            next.set_notice(notice);
        }
        *picker = next;
        Ok(())
    }

    /// A session's title with its short id, or the short id alone when it has no title.
    fn session_name(title: &str, id: &str) -> String {
        let title = text::one_line(title);
        if title.trim().is_empty() {
            short(id)
        } else {
            format!("\"{}\" ({})", title.trim(), short(id))
        }
    }

    /// What a resumed session reports, at startup and after `/resume`: its name, model, last
    /// activity, then the recorded state.
    fn resume_note(session: &Session) -> String {
        let model = match session.model_name().rsplit('/').next() {
            Some(name) if !name.is_empty() => name.to_owned(),
            _ => "unknown model".to_owned(),
        };
        format!(
            "{} · {model} · active {}\n{}",
            Self::session_name(&session.title(), session.id()),
            crate::ui::session_picker::age(&session.updated_at()),
            session.resume_summary()
        )
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

    /// Launches a finished worker's resume (`payload` is `id` then a newline and the message)
    /// through the `task` tool's resume path. It is a live job, not a model turn. Outcomes are
    /// reported as notes.
    fn resume_worker(&mut self, app: &App, agent: &mut Agent<'_>, payload: &str) -> Result<()> {
        let (id, message) = payload.split_once('\n').unwrap_or((payload, ""));
        let args = serde_json::json!({"action": "resume", "id": id, "message": message});
        let (result, failed) = {
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
            let result = agent.start_subagents(&args, &mut bridge);
            (result, bridge.failed)
        };
        if failed {
            anyhow::bail!("terminal draw failed");
        }
        match result {
            Ok(_) => self.note("Subagents", format!("Resumed {id}.")),
            Err(error) => self.note("Subagents", format!("Cannot resume {id}: {error:#}")),
        }
        Ok(())
    }

    /// The value picker a bare command opens in the TUI: `/approval`, `/effort`, `/profile`,
    /// `/theme`, `/queue` and `/permissions remove`. `None` keeps the command's text output.
    fn picker(&self, app: &App, agent: &Agent<'_>, name: &str, args: &str) -> Option<ChoiceSpec> {
        let pick = |value: &str, detail: &str, current: bool| ChoiceItem {
            value: value.to_owned(),
            label: value.to_owned(),
            detail: detail.to_owned(),
            current,
        };
        let picker = |title: &str,
                      command: &str,
                      verb: &'static str,
                      remove: Option<&'static str>,
                      items: Vec<ChoiceItem>| ChoiceSpec {
            title: title.to_owned(),
            command: command.to_owned(),
            verb,
            remove,
            items,
        };
        match (name, args) {
            ("approval", "") => {
                let current = format!("{:?}", agent.approval()).to_lowercase();
                let items = Approval::NAMES
                    .iter()
                    .map(|value| {
                        let mode: Approval = serde_json::from_value(serde_json::json!(value))
                            .expect("approval metadata");
                        pick(value, mode.description(), *value == current)
                    })
                    .collect();
                Some(picker("Approval", "approval", "", None, items))
            }
            ("effort", "") => {
                let profile = reasoning_profile(&app.config).ok().flatten()?;
                let level = thinking_label(&profile);
                let mut items: Vec<ChoiceItem> = profile
                    .capabilities
                    .reasoning_efforts
                    .iter()
                    .map(|effort| {
                        let current = !app.config.thinking_auto && effort.as_str() == level;
                        pick(effort.as_str(), "", current)
                    })
                    .collect();
                items.push(pick(
                    "auto",
                    "Dial judges each prompt's thinking level",
                    app.config.thinking_auto,
                ));
                Some(picker("Thinking effort", "effort", "", None, items))
            }
            ("profile", "") => {
                let active = app.config.model_profile.as_deref();
                let mut items: Vec<ChoiceItem> = app
                    .config
                    .model_profiles
                    .iter()
                    .map(|(key, profile)| {
                        let detail = format!("{} / {}", profile.provider, profile.model);
                        pick(key, &detail, active == Some(key.as_str()))
                    })
                    .collect();
                items.push(pick("off", "no profile", active.is_none()));
                Some(picker("Capability profile", "profile", "", None, items))
            }
            ("theme", "") => {
                let active = self.themes.current().name().to_owned();
                let items = self
                    .themes
                    .names()
                    .into_iter()
                    .filter(|entry| entry.error.is_none())
                    .map(|entry| pick(&entry.name, "", entry.name == active))
                    .collect();
                Some(picker("Theme", "theme", "", None, items))
            }
            ("queue", "") => {
                let items: Vec<ChoiceItem> = self
                    .composer
                    .queued_messages()
                    .into_iter()
                    .enumerate()
                    .map(|(index, (message, follow_up))| ChoiceItem {
                        value: (index + 1).to_string(),
                        label: format!("{}. {}", index + 1, crate::ui::text::one_line(&message)),
                        detail: if follow_up {
                            "follow-up for after the turn"
                        } else {
                            "steering the current turn"
                        }
                        .to_owned(),
                        current: false,
                    })
                    .collect();
                (!items.is_empty())
                    .then(|| picker("Queued messages", "queue", "edit", Some("remove"), items))
            }
            ("permissions", "remove") => {
                let items: Vec<ChoiceItem> = agent
                    .permission_rules()
                    .iter()
                    .enumerate()
                    .map(|(index, rule)| ChoiceItem {
                        value: (index + 1).to_string(),
                        label: format!(
                            "{}. {} {}",
                            index + 1,
                            rule.tool,
                            rule.command_prefix.join(" ")
                        )
                        .trim_end()
                        .to_owned(),
                        detail: format!("{:?}", rule.effect),
                        current: false,
                    })
                    .collect();
                (!items.is_empty()).then(|| {
                    picker(
                        "Remove a permission rule",
                        "permissions",
                        "remove",
                        None,
                        items,
                    )
                })
            }
            _ => None,
        }
    }

    fn command(&mut self, app: &App, agent: &mut Agent<'_>, line: &str) -> Result<Step> {
        let Some(rest) = line.strip_prefix('/') else {
            return self.run_turn(app, agent, line);
        };
        let (name, args) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, args)| (name, args.trim()));
        // `/accounts pin` is the clearer spelling of `/session pin`, which pins a ChatGPT account.
        let accounts_pin;
        let (name, args) = if name == "accounts" && (args == "pin" || args.starts_with("pin ")) {
            accounts_pin = format!("pin{}", &args[3..]);
            ("session", accounts_pin.as_str())
        } else {
            (name, args)
        };
        let name = canonical_command(name);
        if (args.is_empty() || (name == "permissions" && args == "remove"))
            && let Some(spec) = self.picker(app, agent, name, args)
        {
            return self.open(app, agent, Which::Choice(Box::new(spec)));
        }
        if let Some(panel) = work_panel(name, args) {
            return self.open(app, agent, panel);
        }
        if let Some(result) = commands::run(self, app, agent, name, args) {
            return result.or_else(|error| {
                self.note(&format!("/{name}"), format!("{error:#}"));
                Ok(Step::Done)
            });
        }
        match name {
            "reload" => {
                if !args.is_empty() {
                    self.note("Reload", "Usage: /reload");
                    return Ok(Step::Done);
                }
                self.refresh_resources(app, true, false);
                if let Some(config) = self.pending_config.take() {
                    return Ok(self.config_changed(app, agent, config));
                }
                if std::mem::take(&mut self.pending_resource_rebuild) {
                    return Ok(Step::Rebuild(Box::new(app.config.clone())));
                }
            }
            "compute" => return self.open(app, agent, Which::Compute),
            "processes" | "bg" => return self.open(app, agent, Which::Jobs),
            "exit" | "quit" | "q" => return Ok(Step::Quit),
            "help" | "?" => match args {
                "" => return self.open(app, agent, Which::Palette),
                "all" => {
                    let text = self.help_text(true);
                    self.note("Help", text);
                }
                _ => {
                    let text = self.help_text(false);
                    self.note("Help", text);
                }
            },
            "queue" => self.queue_command(line),
            "review" => {
                platform::interrupt::clear_interrupt();
                self.live_tasks = Some(agent.live_task_control());
                self.background_control = agent.background_control();
                self.composer.set_busy(true);
                self.status.phase = Phase::Working;
                match self.wait_compute(app).and_then(|ready| if ready { self.prepare_local(app) } else { Ok(false) }) {
                    Ok(true) => {}
                    Ok(false) => { self.composer.set_busy(false); return Ok(Step::Done); }
                    Err(error) => { self.composer.set_busy(false); self.note("Review", format!("{error:#}")); return Ok(Step::Done); }
                }
                let guidance = self.commands.iter().find(|def| def.name == "review")
                    .map(|def| context::expand_command(def, args)).unwrap_or_default();
                let started = Instant::now();
                let result = {
                    let mut bridge = Bridge { app, shell: self, interrupt: app.interrupt.clone(), started,
                        last_paint: Instant::now() - PAINT_EVERY, failed: false, pending: Vec::new(), next_id: 0 };
                    bridge.repaint(true);
                    agent.review_target(args, &guidance, &mut bridge)
                };
                app.interrupt.store(false, Ordering::SeqCst);
                self.composer.set_busy(false);
                self.status.activity.clear();
                self.refresh_status(app, agent);
                match result { Ok(report) => self.note("Review", report), Err(error) => self.note("Review", format!("{error:#}")) }
            }
            "review-file" => {
                let result = (|| -> Result<()> {
                    let checkpoints = agent.checkpoints().cloned().context("No checkpoints are available for this session")?;
                    let (turn_text, requested_path) = args.split_once(' ').unwrap_or((args, ""));
                    let turn = if args.is_empty() {
                        let turns = checkpoints.list();
                        if turns.is_empty() { self.note("Review", "No checkpoint changes to review. No files changed."); return Ok(()); }
                        let labels: Vec<_> = turns.iter().map(|entry| format!("{}: {} ({} files)", entry.turn, entry.label, entry.files)).collect();
                        let choices: Vec<_> = labels.iter().map(|label| (label.as_str(), ' ')).collect();
                        let Some(selected) = self.ask("Review checkpoints", "Choose a turn. Esc returns to your draft without changing files.", &choices) else { return Ok(()); };
                        turns[selected].turn
                    } else { turn_text.parse::<u64>().context("Expected a checkpoint turn number")? };
                    let browse = requested_path.trim().is_empty();
                    loop {
                        let path = if browse {
                            let paths = checkpoints.changed_files(turn);
                            if paths.is_empty() { self.note("Review", "No readable changed-file inventory. No files changed."); break; }
                            let choices: Vec<_> = paths.iter().map(|path| (path.as_str(), ' ')).collect();
                            let Some(selected) = self.ask("Changed files", "Choose a file. Reviewed, tested and accepted are separate states; file preview shows recorded checks, not new verification.", &choices) else { break; };
                            paths[selected].clone()
                        } else { requested_path.trim().to_owned() };
                        let (diff, token) = checkpoints.review_file(turn, &path)?;
                        let state = agent.session().control_state();
                        let recorded = state["reliability"]["verification_results"].as_array().map(|checks| checks.iter().map(|check| format!("{}: {}", check["name"].as_str().unwrap_or("check"), check["status"].as_str().unwrap_or("unverified"))).collect::<Vec<_>>().join("\n")).unwrap_or_else(|| "No checks recorded.".to_string());
                        let reviewed = state["reviewed_file"]["identity"].as_str() == Some(token.as_str());
                        let body = format!("{} · {}\n{}\nRecorded checks (not file-specific proof):\n{}\nRevert includes user edits shown here; comparison is not atomic against external writers. Page keys scroll the diff/details.", path, if reviewed { "reviewed version" } else { "not reviewed" }, diff, recorded);
                        match self.ask("Review file", &body, &[("Keep", 'k'), ("Revert file", 'r'), ("Cancel", 'n')]) {
                            Some(0) => {
                                anyhow::ensure!(checkpoints.review_file(turn, &path)?.1 == token, "File changed after preview. Review the new version before keeping it.");
                                let mut state = agent.session().control_state();
                                state["reviewed_file"] = serde_json::json!({"turn":turn,"path":path,"identity":token});
                                agent.session_mut().set_control_state(state)?;
                                self.note("Review", "Kept the reviewed version; review is not test verification.");
                            }
                            Some(1) => {
                                checkpoints.restore_file(turn, &path, &token)?;
                                self.note("Review", "Restored only the selected file. The revert has its own checkpoint for /undo.");
                            }
                            _ => {}
                        }
                        if !browse { break; }
                    }
                    Ok(())
                })();
                if let Err(error) = result { self.note("Review", error.to_string()); }
            }
            "model" if args == "restart" => {
                if self.ask("Restart managed model?", "Unload only this session's Dial-managed local model. The same model reloads on the next request; files and conversation stay intact. External servers are never restarted.", &[("Restart", 'r'), ("Cancel", 'n')]) == Some(0) {
                    match agent.restart_managed_model() {
                        Ok(()) => self.note("Model", "Managed model unloaded; it will reload on the next request."),
                        Err(error) => self.note("Model", error.to_string()),
                    }
                }
            }
            "model" if args == "cancel" => {
                self.pending_favorite = None;
                self.note("Favorites", "Pending switch cancelled.");
            }
            "model" if args == "next" => {
                self.pending_favorite = None;
                let effective = &app.config;
                let favorites = cycle_set(effective);
                if favorites.is_empty() {
                    self.note(
                        "Favorites",
                        "No favorites saved. Use Ctrl-F in /models to save one.",
                    );
                    return Ok(Step::Done);
                }
                let Some(discovery) = self.catalog.get() else {
                    self.catalog.start(app);
                    self.pending_favorite = Some(PendingFavorite {
                        main: effective.selection(),
                        favorites,
                        scope: self.scope,
                        catalog_request: self.catalog.request,
                    });
                    self.note(
                        "Favorites",
                        "Discovering available models; the favorite switch will apply when ready. Esc or /model cancel cancels it.",
                    );
                    return Ok(Step::Done);
                };
                let available = discovery
                    .models
                    .iter()
                    .filter(|entry| {
                        entry.available && entry.supports_chat()
                    })
                    .map(|entry| entry.selection.clone())
                    .collect::<Vec<_>>();
                match next_favorite(effective, &favorites, &available) {
                    Some(selection) => {
                        let changes = selection_changes(effective, &selection, "main")?;
                        match app.save_settings(self.scope, &changes, Some("main"))
                            .and_then(|_| app.reload())
                            .map(|fresh| self.config_changed(app, agent, fresh)) {
                            Ok(step) => {
                                self.note(
                                    "Favorites",
                                    format!("Switched to {}", selection.display_name()),
                                );
                                return Ok(step);
                            }
                            Err(error) => self.note("Favorites", format!("not changed: {error:#}")),
                        }
                    }
                    None => self.note(
                        "Favorites",
                        if crate::cli::model_scope::is_active() {
                            "No available models in the --models scope."
                        } else {
                            "No available chat favorites. Use Ctrl-F in /models to save one."
                        },
                    ),
                }
            }
            "model" if matches!(args, "settings" | "setting") => {
                return self.open(app, agent, Which::ModelSettings);
            }
            "model" if args == "defaults" => {
                return self.open(app, agent, Which::ContextDefaults);
            }
            "model" | "models" if args == "local" => {
                return self.open(app, agent, Which::ModelDownloads);
            }
            "model" if !args.is_empty() => {
                self.note(
                    "Model",
                    "Unknown option. Use /model to choose a model, /model settings to tune it, /model defaults for shared context defaults, or /model local for the Get models downloads in /models.",
                );
            }
            "model" | "models" => return self.open(app, agent, Which::Models),
            "roles" => return self.open(app, agent, Which::ModelRoles),
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
                } else if args != "off" && !app.config.model_profiles.contains_key(args) {
                    self.note("Profile", format!("Unknown profile `{args}`. Define it under [model_profiles] or run /profile to list them."));
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
            "providers" | "provider" => return self.open(app, agent, Which::Providers),
            "palette" => return self.open(app, agent, Which::Palette),
            "settings" => return self.open(app, agent, Which::Settings),
            "improve" if args == "settings" => return self.open(app, agent, Which::LearningSettings),
            "improve" if args == "models" => return self.open(app, agent, Which::LearningModels),
            "model-settings" => return self.open(app, agent, Which::ModelSettings),
            "hotkeys" => return self.open(app, agent, Which::Shortcuts),
            "accounts" => self.note(
                "Accounts",
                "Usage: /accounts pin (chooses which ChatGPT account this session uses)",
            ),
            "improve" => return self.improve_command(app, agent, args),
            "changes" => {
                let files = self.git.files(&agent.layout().unwrap_or(&app.layout).project_root);
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
            "new" | "clear" => match agent.layout().context("agent has no project layout").and_then(|layout| {
                Session::create(
                    layout,
                    SessionMeta {
                        model: agent.model().to_owned(),
                    },
                )
            }) {
                Ok(session) => {
                    let id = session.id().to_owned();
                    agent.swap_session(session);
                    agent.set_plan_mode(false);
                    self.stop_loop_for_session_change();
                    self.show_session(agent.session());
                    self.session_id = id.clone();
                    self.note("Session", format!("new session {}", short(&id)));
                }
                Err(error) => self.note("Session", format!("error: {error:#}")),
            },
            "autoresearch" => {
                let layout = agent.layout().ok_or_else(|| anyhow::anyhow!("agent has no project layout"))?.clone();
                let state = layout.state_dir.join("autoresearch");
                let mut words = args.splitn(5, char::is_whitespace).filter(|word| !word.is_empty());
                let action = words.next().unwrap_or("status");
                let result = match action {
                    "init" => match (words.next(), words.next(), words.next(), words.next()) {
                        (Some(name), Some(metric), Some(direction), Some(benchmark)) => match crate::tools::autoresearch::experiment::parse_direction(direction) {
                            Ok(maximize) => crate::tools::autoresearch::experiment::init(&layout.project_root, &state, name, benchmark, metric, maximize).map(|e| format!("Initialized {} on branch {}.{}", e.name, e.branch, if e.origin.is_empty() { String::new() } else { format!("\nLeave with /autoresearch exit; it returns to {}.", e.origin) })),
                            Err(_) => anyhow::bail!("direction must be max or min. Usage: /autoresearch init <name> <metric> <max|min> <benchmark command>"),
                        },
                        _ => anyhow::bail!("Usage: /autoresearch init <name> <metric> <max|min> <benchmark command>"),
                    },
                    "run" => crate::tools::autoresearch::experiment::run(&layout.project_root, &state).map(|run| format!("{} = {} · {}{}", run.metric, run.commit, if run.kept {"best"} else {"not best"}, if run.output.is_empty(){String::new()}else{format!("\n{}",run.output)})),
                    "log" => crate::tools::autoresearch::experiment::log(&state).map(|runs| runs.into_iter().map(|r| format!("#{} {} {} {}", r.id, r.metric, if r.kept {"best"} else {""}, r.commit)).collect::<Vec<_>>().join("\n")),
                    "notes" => { let text = args.trim().split_once(char::is_whitespace).map_or("", |(_, rest)| rest.trim()); if text.is_empty() { crate::tools::autoresearch::experiment::notes(&state) } else { crate::tools::autoresearch::experiment::update_notes(&state, text).map(|()| "Experiment notes updated.".to_owned()) } }
                    "resume" | "status" | "" => crate::tools::autoresearch::experiment::current(&state).map(|experiment| experiment.map(|e| format!("{} · {} · {} ({})\n{}", e.name, e.metric, e.branch, e.benchmark, crate::tools::autoresearch::experiment::notes(&state).unwrap_or_default())).unwrap_or_else(|| "No autoresearch experiment. Use /autoresearch init <name> <metric> <max|min> <benchmark command>.".to_owned())),
                    "exit" => crate::tools::autoresearch::experiment::exit(&layout.project_root, &state),
                    other => anyhow::bail!("Unknown autoresearch action: {other}. Use init, run, log, notes, resume, exit."),
                };
                match result { Ok(text) => self.note("Autoresearch", text), Err(error) => self.note("Autoresearch", format!("{error:#}")) }
            }
            "archive" => {
                let archive = crate::storage::archive::Archive::new(&app.layout.user_dir);
                let result = match args.trim() {
                    "sessions" => archive.sessions(None, 30).map(|view| view.text),
                    "prompts" => archive.prompts(None, None, 30).map(|view| view.text),
                    "projects" | "" => archive.projects(30).map(|view| view.text),
                    other => archive.prompts(Some(other), None, 30).map(|view| view.text),
                };
                match result {
                    Ok(text) if text.trim().is_empty() => self.note("Archive", "no archived history"),
                    Ok(text) => self.note("Archive", text),
                    Err(error) => self.note("Archive", format!("{error:#}")),
                }
            }
            "sessions" | "resume" | "tree" => {
                match self.pick_session(agent, args, name == "tree", name == "resume") {
                    Ok(Some(session)) => {
                        self.show_session(&session);
                        agent.swap_session(session);
                        agent.set_plan_mode(false);
                        self.stop_loop_for_session_change();
                        self.note("Resume", Self::resume_note(agent.session()));
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
                            paths.extend(words.next().map(|p| p.trim_matches(['\'', '"']).to_owned()));
                        } else {
                            command_prefix.push(word.to_owned());
                        }
                    }
                    let prefix = if tool == "bash" {
                        crate::agent::approval::bash_prefix(&command_prefix)
                    } else {
                        Ok(command_prefix)
                    };
                    match prefix {
                        Ok(command_prefix) => {
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
                        }
                        Err(error) => self.note("Permissions", error),
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
                            "No allow or deny rules yet. /permissions allow <tool> [prefix] adds one; \"Always allow\" in an approval prompt adds one too.".into()
                        } else {
                            rules.join("\n")
                        },
                    );
                } else {
                    self.note("Permissions", "usage: /permissions; /permissions allow|deny [--persist] <tool> [prefix words | --path <glob>…]; /permissions remove <number> [--persist]");
                }
            }
            "cost" => {
                let (total, turn) = agent.usage();
                if total.all.requests == 0 {
                    self.note(
                        "Cost and usage",
                        "No model requests yet in this session.\nCost and token counts appear here after the first reply.",
                    );
                    return Ok(Step::Done);
                }
                // Summary first; the accounting detail (attempts, reports, pricing coverage) follows
                // per role as `key: value` rows.
                let describe = |role: &str, c: &usage::Counts| format!(
                    "{role}\n  cost: {} · requests: {} · retries: {} · failed: {}\n  tokens in: {} · out: {} · cached reads: {} · cache writes: {} · reasoning: {}\n  priced: {} · unpriced: {} · local: {} · subscription: {}\n  attempts: {} · accepted: {} · discarded: {} · without complete usage: {} · usage reports: {} · cache reports: {} · reasoning reports: {}",
                    c.cost_label(), c.requests, c.retries, c.failed_requests,
                    c.input, c.output, c.cache_read, c.cache_write, c.reasoning,
                    c.priced_requests, c.unpriced_requests, c.local_requests, c.subscription_requests,
                    c.transport_attempts, c.successful_requests, c.discarded_attempts,
                    c.unreported_attempts, c.reported_responses,
                    c.cache_reported_responses, c.reasoning_reported_responses,
                );
                let turn_label = turn
                    .coverage
                    .turn_id
                    .map_or_else(|| "this turn".to_owned(), |id| format!("turn {id}"));
                let mut text = format!(
                    "Session: {} · requests: {} · tokens in: {} · out: {}\nCurrent {turn_label}: {}\nCoverage: {}\n\n{}\n\n{}",
                    total.all.cost_label(), total.all.requests, total.all.input, total.all.output,
                    turn.all.cost_label(), total.coverage.history,
                    describe("Main agent", &total.main), describe("Workers", &total.workers),
                );
                for (model, counts) in &total.by_model {
                    text.push_str(&format!("\n\nModel {model}\n{}\n{}",
                        describe("  Main agent", &counts.main).replace("\n  ", "\n    "),
                        describe("  Workers", &counts.workers).replace("\n  ", "\n    ")));
                }
                text.push_str("\n\nMissing usage or prices are unknown, not free; local and subscription usage is not an invoice estimate. Caps stop on observed cost before the next step; in-flight workers and discarded attempts can add unobserved charges. Accounting for earlier (resumed) sessions is unavailable.");
                self.note("Cost and usage", text);
            }
            "workers" => {
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
            "fork" => match agent.layout().context("agent has no project layout").and_then(|layout| agent.session().fork(layout)) {
                Ok(session) => {
                    let id = session.id().to_owned();
                    agent.swap_session(session);
                    agent.set_plan_mode(false);
                    self.stop_loop_for_session_change();
                    self.session_id = id.clone();
                    self.note("Session", format!("forked into {}", short(&id)));
                }
                Err(error) => self.note("Session", format!("error: {error:#}")),
            },
            "compact" => {
                let mut options = match crate::config::compaction::CompactionOptions::from_args(args) {
                    Ok(options) => options,
                    Err(error) => { self.note("Context", format!("{error:#}")); return Ok(Step::Done); }
                };
                options.suppress_continuation = true;
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
                    agent.compact_with_options(&mut bridge, options)
                };
                self.composer.set_busy(false);
                if let Err(error) = result {
                    self.transcript.error(&format!("{error:#}"));
                }
            }
            "undo" => {
                let mut lines = Vec::new();
                if let (Some(trajectory), Some(store)) = (self.last_trajectory, app.store.as_ref())
                {
                    let outcome = Outcome {
                        trajectory,
                        score: 0.0,
                        source: "user:undo".into(),
                        reference: None,
                    };
                    match store.outcome(&outcome) {
                        Ok(()) => self.start_live_monitor("Undo", app.config.clone()),
                        Err(error) => lines.push(format!("could not record undo feedback: {error:#}")),
                    }
                }
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
                agent.cancel_background_compaction();
                match commands::branch::rewind(app, agent, 1) {
                    Ok(rewound) => {
                        lines.push(format!("dropped {} message(s)", rewound.dropped));
                        lines.extend(rewound.note);
                    }
                    Err(error) => lines.push(format!("rewind: {error:#}")),
                }
                self.note("Undo", lines.join("\n"));
            }
            "rollback" => {
                let turns = if args.is_empty() { Ok(1) } else { args.parse::<usize>() };
                let Ok(turns) = turns else {
                    self.note("Rollback", "Usage: /rollback [n] with a whole number of turns.");
                    return Ok(Step::Done);
                };
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
            "todo" => {
                let text = agent.todo().render();
                let text = if text.trim().is_empty() {
                    "No tasks yet. The agent adds a checklist here when work has several steps.".to_owned()
                } else {
                    text
                };
                self.note("Todo", text)
            }
            "skills" => {
                let words = crate::runtime::arguments::split_words(args);
                if matches!(words.first().map(String::as_str), Some("install" | "update")) {
                    // Network-bound: run in the job slot so the UI keeps responding; progress streams in.
                    let layout = app.layout.clone();
                    let registries = app.config.skills.registries.clone();
                    let started = start_streaming_job(&mut self.auth_job, "Skills", move |cancel, progress| {
                        skills::management::command(&layout, &registries, words, &cancel, progress)
                    });
                    if !started {
                        self.note("Skills", "Another background job is running; try again when it finishes.");
                    }
                } else {
                    let result = skills::management::command(
                        &app.layout,
                        &app.config.skills.registries,
                        words,
                        &std::sync::atomic::AtomicBool::new(false),
                        &mut |_: &str| {},
                    );
                    match result {
                        Ok(text) => self.note("Skills", text),
                        Err(error) => self.note("Skills", format!("{error:#}")),
                    }
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
            "code-history" => {
                use crate::tools::history::{self, ChangeHistory, Request};
                if !(app.config.codebase.enabled && app.config.codebase.history) {
                    self.note("Code history", "Code history is off. Turn it on in /settings → Project understanding → Code change history (codebase.history).");
                    return Ok(Step::Done);
                }
                let request = match history::parse_request(args) {
                    Ok(request) => request,
                    Err(usage) => {
                        self.note("Code history", usage);
                        return Ok(Step::Done);
                    }
                };
                if request == Request::Clear
                    && self.ask("Clear code history", "Delete every recorded diff for this project? Files are not changed.", &[("Clear code history", 'y'), ("Keep it", 'n')]) != Some(0)
                {
                    return Ok(Step::Done);
                }
                let text = ChangeHistory::open(&app.layout).and_then(|store| match request {
                    Request::Search { query, path, branch } => {
                        let found = store.search(&query, path.as_deref(), branch.as_deref(), 30)?;
                        Ok(format!("{}\n/code-history show <id> prints a diff · filter with path:<file|dir> branch:<name>", history::render_list(&found)))
                    }
                    Request::Show(id) => Ok(match store.show(id)? {
                        Some((version, diff)) => history::render_version(&version, &diff),
                        None => format!("No recorded version #{id}."),
                    }),
                    Request::Clear => Ok(format!("Cleared {} recorded versions.", store.clear()?)),
                });
                self.note("Code history", text.unwrap_or_else(|error| format!("{error:#}")));
            }
            "understanding" if args == "clear" => {
                let result = crate::tools::codebase::Codebase::open(&app.layout)
                    .and_then(|cache| cache.clear_architecture());
                match result {
                    Ok(removed) => self.note(
                        "Understanding",
                        format!("Cleared saved architecture synthesis ({removed} records)."),
                    ),
                    Err(error) => self.note("Understanding", format!("error: {error:#}")),
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
                match result {
                    Ok(()) => {
                        let cancel = app.interrupt.clone();
                        let text = crate::tools::codebase::Codebase::open(&app.layout)
                            .and_then(|cache| cache.understanding("architecture", &cancel))
                            .map(|report| crate::tools::codebase::render_understanding(&report, 40))
                            .unwrap_or_else(|error| format!("{error:#}"));
                        bridge.shell.note("Understanding", text);
                    }
                    Err(error) => bridge.shell.note("Architecture", format!("{error:#}")),
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
                    let on = |flag: bool| if flag { "on" } else { "off" };
                    text.push_str(&format!("\nAssistance {} · syntax guard {} · related tests {} · compact tools {}\nTeacher {} · needs a hosted experience role · {}s budget\nEdit controls in /settings; model role in {} → Roles → experience.",on(app.config.assistance.enabled),on(app.config.assistance.syntax_guard),on(app.config.assistance.selective_verification),on(app.config.assistance.compact_tools),on(app.config.assistance.teacher_enabled),app.config.assistance.teacher_timeout_secs,self.keys.label(Action::ModelSelect)));
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
                            crate::tools::codebase::Codebase::open(layout)
                                .and_then(|cache| cache.understanding(args, &cancel))
                                .map(|report| {
                                    crate::tools::codebase::render_understanding(&report, 40)
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
                app.interrupt.store(false, Ordering::SeqCst);
                let needs_compute = app.config.memory_uses_local_compute()
                    && !matches!(args.split_whitespace().next(), None | Some("status" | "queue"));
                if needs_compute && !self.wait_compute_required(app, true, false)? {
                    return Ok(Step::Done);
                }
                self.composer.set_busy(true);
                let result = {
                    let mut bridge = Bridge {
                        app, shell: self, interrupt: app.interrupt.clone(),
                        started: Instant::now(), last_paint: Instant::now() - PAINT_EVERY,
                        failed: false, pending: Vec::new(), next_id: 0,
                    };
                    agent.selected_memory_command(args, &mut bridge)
                        .and_then(|value| {
                            // `/memory help` answers with text (a `help` field or a bare string).
                            if let Some(help) = value
                                .get("help")
                                .and_then(Value::as_str)
                                .or_else(|| value.as_str())
                            {
                                Ok(help.to_owned())
                            } else if matches!(args.trim(), "" | "status") && value.get("backend").is_some() {
                                Ok(memory_status_text(&value))
                            } else {
                                serde_json::to_string_pretty(&value).map_err(Into::into)
                            }
                        })
                };
                self.composer.set_busy(false);
                self.note("Memory", result.unwrap_or_else(|error| format!("error: {error:#}")));
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
            "search" if args.trim().is_empty() => self.note("Search", Self::SEARCH_USAGE),
            "search" => match agent.layout().ok_or_else(|| anyhow::anyhow!("agent has no project layout")).and_then(|layout| Session::search(layout, args, 8)) {
                Ok(hits) if hits.is_empty() => self.note("Search", format!("No past messages match \"{}\".", args.trim())),
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
            "context" => {
                let used = agent.last_prompt_tokens;
                let window = u64::from(agent.context_window());
                let percent = (used * 100).checked_div(window).unwrap_or(0);
                self.note(
                    "Context",
                    format!(
                        "used: {} of {} tokens ({percent}%) · messages: {}\n/compact summarizes older turns to free room; /new starts fresh.",
                        group_digits(used),
                        group_digits(window),
                        agent.session().messages().len()
                    ),
                )
            }
            "approval" => {
                match args {
                    "ask" => agent.set_approval(Approval::Ask),
                    "write" => agent.set_approval(Approval::Write),
                    "auto" => agent.set_approval(Approval::Auto),
                    "readonly" => agent.set_approval(Approval::Readonly),
                    "" => {}
                    _ => {
                        self.note("Approval", "usage: /approval ask|write|auto|readonly");
                        return Ok(Step::Done);
                    }
                }
                let mode = format!("{:?}", agent.approval()).to_lowercase();
                let meaning = agent.approval().description();
                self.note(
                    "Approval",
                    format!("mode: {mode} · {meaning}\nChange it with /approval ask|write|auto|readonly or Shift+Tab."),
                );
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
                            Ok(()) => {
                                self.note("Outcome", format!("recorded {score} for trajectory {trajectory}"));
                                self.start_live_monitor("Outcome", app.config.clone());
                            }
                            Err(error) => self.transcript.error(&format!("{error:#}")),
                        }
                    }
                    (None, _, _) => self.note("Outcome", Self::OUTCOME_USAGE),
                    (_, None, _) => self.note("Outcome", "No completed turn to score yet. Finish a turn, then rate it with /outcome <0-100>."),
                    (_, _, None) => self.note("Outcome", TEACH_OFF),
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
            // Grouped one line per kind (wrapped by the transcript) instead of one name per row.
            let (mut dark, mut light, mut custom) = (Vec::new(), Vec::new(), Vec::new());
            for entry in self.themes.names() {
                let name = if entry.name == active {
                    format!("● {}", entry.name)
                } else {
                    entry.name.clone()
                };
                if let Some(error) = entry.error {
                    custom.push(format!("{name} (cannot load: {error})"));
                } else if !entry.builtin {
                    custom.push(name);
                } else if crate::ui::theme::Theme::builtin(&entry.name)
                    .is_some_and(|theme| !theme.is_dark())
                {
                    light.push(name);
                } else {
                    dark.push(name);
                }
            }
            let mut lines = vec![format!("Current: {active}")];
            for (kind, names) in [("Dark", dark), ("Light", light), ("Custom", custom)] {
                if !names.is_empty() {
                    lines.push(format!("{kind}: {}", names.join(" · ")));
                }
            }
            lines.push(String::new());
            lines.push(
                "Switch with /theme <name> (saved globally), or preview live in /settings.".into(),
            );
            lines.push(format!(
                "Custom themes are JSON files in {}; they reload live.",
                self.themes.dir().display()
            ));
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

/// Plain words for the scheduler state recorded in `auto_status`.
fn state_words(state: &str) -> String {
    match state {
        "" => "no cycle has run yet".into(),
        "idle" => "idle; the next cycle runs when due".into(),
        "queued" => "queued".into(),
        "running" => "running".into(),
        "paused" => "paused (daily compute budget reached)".into(),
        "stopped" => "stopped at your request".into(),
        "disabled" => "paused (automatic improvement is off)".into(),
        "failed" => "the last cycle failed".into(),
        other => other.replace('_', " "),
    }
}

/// One line per version: id, status, kind and parent. The live version is marked.
fn version_line(version: &crate::config::types::Version, live: Option<i64>) -> String {
    let mut line = format!("v{} · {} · {}", version.id, version.status, version.kind);
    if let Some(parent) = version.parent {
        line.push_str(&format!(" · from v{parent}"));
    }
    if live == Some(version.id) {
        line.push_str(" · live");
    }
    line
}

/// The steps of a saved cycle report, one short line each.
fn cycle_steps(report: &Value) -> Vec<String> {
    report["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|step| {
            let name = step["step"].as_str().unwrap_or("step").replace('_', " ");
            let detail = match &step["data"] {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            if detail.is_empty() {
                name
            } else {
                format!("{name}: {}", agent::truncate(&detail, 120))
            }
        })
        .collect()
}

/// `/improve jobs`: project-wide compute status. It reads job files only, so it runs during a turn.
fn improve_jobs_text(app: &App) -> String {
    auto::jobs_summary(&app.layout).unwrap_or_else(|error| format!("{error:#}"))
}

/// `/improve stop <id>`. A bare stop lists the running jobs and their ids instead of guessing one.
fn improve_stop_text(app: &App, id: &str) -> String {
    let id = id.trim();
    if id.is_empty() {
        let running: Vec<String> = auto::jobs(&app.layout)
            .ok()
            .and_then(|jobs| jobs["project_jobs"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter(|job| job["control_busy"] == true)
            .filter_map(|job| job["state_id"].as_str().map(str::to_owned))
            .collect();
        return if running.is_empty() {
            "usage: /improve stop <state id>. No improvement job is running in this project.".into()
        } else {
            format!(
                "usage: /improve stop <state id>. Running jobs: {}",
                running.join(", ")
            )
        };
    }
    match auto::request_stop(&app.layout, id) {
        Ok(value) => value["message"]
            .as_str()
            .unwrap_or("stop requested")
            .to_owned(),
        Err(error) => format!("{error:#}"),
    }
}

/// The `/improve` panel's snapshot: the facts of `/improve status` as plain values. It reads the
/// store and job files, so the controller calls it on open and while a job runs, never from draw.
fn improve_view(app: &App) -> crate::ui::overlay::improve::View {
    use crate::ui::overlay::improve::{JobRow, VersionRow, View};
    let cached = Store::open_read_only(&app.config.state_dir).ok();
    let needed = crate::learn::improve::min_examples(&app.config);
    let Some(store) = app.store.as_ref().or(cached.as_ref()) else {
        return View {
            enabled: app.config.improve.enabled,
            readiness: "the learning store is unavailable".into(),
            state: "unavailable".into(),
            needed,
            ..View::default()
        };
    };
    let status = store
        .get_meta("auto_status")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    let state = status["state"].as_str().unwrap_or("");
    let mut progress = Vec::new();
    if matches!(state, "queued" | "running") {
        let cycle = store
            .get_meta("cycle_progress")
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or(Value::Null);
        progress.push(format!(
            "Stage: {}",
            cycle["stage"].as_str().unwrap_or("starting")
        ));
        if let Some(started) = cycle["started"].as_u64() {
            progress.push(format!(
                "Elapsed: {}",
                auto::duration(crate::learn::improve::now().saturating_sub(started))
            ));
        }
        if let (Some(iteration), Some(total)) = (
            cycle["training"]["iteration"].as_u64(),
            cycle["training"]["total"].as_u64(),
        ) {
            progress.push(format!(
                "Training: {} {iteration}/{total}",
                cycle["training"]["phase"].as_str().unwrap_or("step")
            ));
        }
    }
    let last_cycle = store
        .get_meta("last_cycle_report")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(|report| cycle_steps(&report))
        .unwrap_or_default();
    let active = store.active().ok();
    let live = active.as_ref().map(|version| version.id);
    let previous = active.as_ref().and_then(|version| version.parent);
    let mut jobs: Vec<JobRow> = auto::jobs(&app.layout)
        .ok()
        .and_then(|jobs| jobs["project_jobs"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|job| job.get("error").is_none())
        .map(|job| {
            let active = job["control_busy"] == true;
            let stage = job["progress"]["stage"].as_str().unwrap_or(if active {
                "running"
            } else {
                "idle"
            });
            JobRow {
                id: job["state_id"].as_str().unwrap_or("").to_owned(),
                label: format!(
                    "{} · {stage}{}",
                    job["model"].as_str().unwrap_or("unknown model"),
                    if job["stop_requested"] == true {
                        " · stop requested"
                    } else {
                        ""
                    }
                ),
                active,
                stopping: job["stop_requested"] == true,
            }
        })
        .collect();
    jobs.sort_by_key(|job| !job.active);
    let versions = store
        .versions()
        .unwrap_or_default()
        .iter()
        .rev()
        .map(|version| VersionRow {
            id: version.id,
            text: version_line(version, None),
            live: live == Some(version.id),
            previous: previous == Some(version.id),
        })
        .collect();
    View {
        enabled: app.config.improve.enabled,
        readiness: auto::readiness(&app.config, store)
            .unwrap_or_else(|error| format!("unavailable ({error:#})")),
        state: state_words(state),
        live_version: live,
        approved: store.verified_live_turns().unwrap_or(0),
        needed,
        progress,
        last_cycle,
        jobs,
        versions,
    }
}

fn improve_summary(app: &App) -> String {
    let cached = Store::open_read_only(&app.config.state_dir).ok();
    let Some(store) = app.store.as_ref().or(cached.as_ref()) else {
        return format!(
            "{TEACH_OFF}\nWithout it, coding sessions are not recorded; `dial improve cycle` still runs an explicit evaluation."
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
    let state_text = state_words(&state);
    let mut summary = format!(
        "Selected model: {} · version {version} · {state_text} · next cycle: {}\nlearning records stay in {}",
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
    summary.push_str(&format!("\nTraining requires at least {} verified examples and a validation split. Cooldowns are eligibility delays, not training-duration estimates.", crate::learn::improve::min_examples(&app.config)));
    if app.store.is_none() {
        summary.push_str(&format!("\n{TEACH_OFF}"));
    }
    match store.verified_live_turns() {
        Ok(n) if app.store.is_some() => summary.push_str(&format!(
            "\nTeaching: {n} approved turns recorded. A turn is approved when you commit its changes, press Alt+Up, or its behavioral checks pass; /undo or Alt+Down rejects it."
        )),
        Ok(n) => summary.push_str(&format!("\nSession learning: {n} verified live turns recorded earlier.")),
        Err(error) => summary.push_str(&format!("\nSession learning unavailable: {error:#}")),
    }
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
    match store.latest_lora_admission() {
        Ok(Some((report, activated, decision))) => {
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
        }
        Ok(None) => summary.push_str("\nNo LoRA evaluation recorded yet."),
        Err(error) => summary.push_str(&format!("\nLoRA evaluation status unavailable: {error:#}")),
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
    Improve(Box<crate::ui::overlay::improve::ImproveOverlay>),
    Extensions(Box<crate::ui::overlay::extensions::ExtensionsOverlay>),
    Settings(Box<SettingsOverlay>),
    ModelSettings(Box<crate::ui::overlay::model_settings::ModelSettingsOverlay>),
    Models(Box<ModelOverlay>),
    Providers(ProviderOverlay),
    Shortcuts(ShortcutOverlay),
    Git(Box<crate::ui::overlay::git::GitOverlay>),
    Subagents(Box<crate::ui::subagent_hub::SubagentHub>),
    SubagentViewer(Box<crate::ui::overlay::subagent_viewer::SubagentViewer>),
    Jobs(Box<crate::ui::overlay::jobs::JobsOverlay>),
}

impl ActiveOverlay {
    fn draw(&mut self, frame: &mut ratatui::Frame, ctx: &DrawCtx) {
        match self {
            Self::Palette(o) => o.draw(frame, ctx),
            Self::Settings(o) => o.draw(frame, ctx),
            Self::ModelSettings(o) => o.draw(frame, ctx),
            Self::Compute(o) => o.draw(frame, ctx),
            Self::Improve(o) => o.draw(frame, ctx),
            Self::Extensions(o) => o.draw(frame, ctx),
            Self::Models(o) => o.draw(frame, ctx),
            Self::Providers(o) => o.draw(frame, ctx),
            Self::Shortcuts(o) => o.draw(frame, ctx),
            Self::Git(o) => o.draw(frame, ctx),
            Self::Subagents(o) => o.draw(frame, ctx),
            Self::SubagentViewer(o) => o.draw(frame, ctx),
            Self::Jobs(o) => o.draw(frame, ctx),
        }
    }
    /// Native image placements for the frame just drawn. Only the git overlay previews images.
    fn native_placements(&self) -> Vec<crate::ui::images::Placement> {
        match self {
            Self::Git(o) => o.native_placements(),
            _ => Vec::new(),
        }
    }
    fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if let Event::Mouse(mouse) = event
            && !matches!(self, Self::Git(_) | Self::SubagentViewer(_))
        {
            return self.mouse(*mouse, keys);
        }
        match self {
            Self::Palette(o) => o.handle(event, keys),
            Self::Settings(o) => o.handle(event, keys),
            Self::ModelSettings(o) => o.event(event, keys),
            Self::Compute(o) => o.handle(event, keys),
            Self::Improve(o) => o.handle(event, keys),
            Self::Extensions(o) => o.handle(event, keys),
            Self::Models(o) => o.handle(event, keys),
            Self::Providers(o) => o.handle(event, keys),
            Self::Shortcuts(o) => o.handle(event, keys),
            Self::Subagents(o) => o.handle(event, keys),
            Self::Git(o) => o.handle(event, keys),
            Self::SubagentViewer(o) => o.handle(event, keys, std::time::Instant::now()),
            Self::Jobs(o) => o.handle(event, keys),
        }
    }
    /// Mouse input for panels without their own mouse handling (git has one). A left click selects
    /// the row drawn there in the palette and jobs; the wheel moves the selection like the arrow
    /// keys. Jobs ignore the wheel while attached, because arrow keys then belong to the job.
    fn mouse(&mut self, mouse: crossterm::event::MouseEvent, keys: &Keymap) -> OverlayEvent {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
        let arrow = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                return match self {
                    Self::Palette(o) => o.click(mouse.row),
                    Self::Jobs(o) => o.click(mouse.row),
                    Self::Subagents(o) => {
                        o.click(mouse.column, mouse.row, std::time::Instant::now())
                    }
                    _ => OverlayEvent::None,
                };
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = matches!(mouse.kind, MouseEventKind::ScrollDown);
                if let Self::Subagents(o) = self
                    && o.wheel_over_inspector(mouse.column, mouse.row, down)
                {
                    return OverlayEvent::None;
                }
                if down { KeyCode::Down } else { KeyCode::Up }
            }
            _ => return OverlayEvent::None,
        };
        if matches!(self, Self::Jobs(o) if o.attached().is_some()) {
            return OverlayEvent::None;
        }
        self.handle(&Event::Key(KeyEvent::new(arrow, KeyModifiers::NONE)), keys)
    }
    fn banner(&mut self, message: String, is_error: bool) {
        match self {
            Self::Models(o) => o.set_status(message, is_error),
            Self::Compute(o) => o.banner(message, is_error),
            Self::Improve(o) => o.banner(message, is_error),
            Self::Extensions(o) => o.banner(message),
            Self::Providers(o) => o.set_status(message, is_error),
            Self::ModelSettings(o) => o.banner(message, is_error),
            Self::Subagents(o) => o.banner(message, is_error),
            Self::SubagentViewer(o) => o.set_notice(message),
            Self::Jobs(o) => o.banner(message, is_error),
            Self::Settings(o) => o.banner(message, is_error),
            Self::Palette(_) | Self::Shortcuts(_) | Self::Git(_) => {}
        }
    }
}

/// Starts `work` as the background job in `slot`. Progress lines it reports reach the transcript as
/// the main loop drains them. Returns false, leaving `slot` alone, when another job is still running.
pub(crate) fn start_streaming_job(
    slot: &mut Option<AuthJob>,
    label: &'static str,
    work: impl FnOnce(
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        &mut dyn FnMut(&str),
    ) -> anyhow::Result<String>
    + Send
    + 'static,
) -> bool {
    if slot.is_some() {
        return false;
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let (line_sender, line_receiver) = std::sync::mpsc::channel::<String>();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    std::thread::spawn(move || {
        let mut progress = |line: &str| {
            let _ = line_sender.send(line.to_owned());
        };
        let _ = sender.send(work(worker_cancel, &mut progress));
    });
    *slot = Some(AuthJob {
        label,
        receiver,
        cancel,
        lines: Some(line_receiver),
        model: None,
    });
    true
}

/// Read-only findings beside the settings rows that depend on installed programs.
fn apply_settings_hints(overlay: &mut SettingsOverlay, effective: &Config) {
    for (path, hint) in crate::voice::settings_hints(effective) {
        overlay.set_hint(path, hint);
    }
}

/// Missing-dependency offers for the settings overlay, keyed by the setting path each one serves.
fn setting_setup_offers(
    user_dir: &std::path::Path,
    config: &crate::config::Config,
) -> std::collections::BTreeMap<String, crate::ui::overlay::settings::SetupOffer> {
    let env = crate::setup::deps::Env::current(user_dir);
    let mut offers: std::collections::BTreeMap<String, crate::ui::overlay::settings::SetupOffer> =
        crate::setup::deps::run::setting_offers(
            &env,
            &config.voice.tts.backend,
            &config.voice.stt.backend,
        )
        .into_iter()
        .map(|offer| {
            (
                offer.path.clone(),
                crate::ui::overlay::settings::SetupOffer {
                    id: offer.id.to_owned(),
                    title: offer.title.to_owned(),
                    preview: offer.preview,
                },
            )
        })
        .collect();
    // The embedding model row offers a local model until one is configured.
    if let Some(offer) = crate::setup::embeddings::setting_offer(user_dir, config) {
        offers.insert(
            offer.path.to_owned(),
            crate::ui::overlay::settings::SetupOffer {
                id: offer.id,
                title: offer.title,
                preview: offer.preview,
            },
        );
    }
    offers
}

/// Discovered models the Settings Embedding model row offers for the `embedding` role.
fn settings_embedding_models(
    discovery: Option<&catalog::Discovery>,
) -> Vec<crate::ui::overlay::settings::EmbeddingModel> {
    discovery
        .map(crate::ui::overlay::models::embedding_candidates)
        .unwrap_or_default()
        .into_iter()
        .map(
            |(entry, classified)| crate::ui::overlay::settings::EmbeddingModel {
                selection: entry.selection,
                label: entry.label,
                classified,
            },
        )
        .collect()
}

/// One setup offer per curated embedding model, so Ctrl+L sets up the model picked on the row.
fn embedding_setup_offers(
    user_dir: &std::path::Path,
) -> std::collections::BTreeMap<String, crate::ui::overlay::settings::SetupOffer> {
    crate::setup::embeddings::catalog_offers(user_dir)
        .into_iter()
        .zip(crate::setup::embeddings::CATALOG)
        .map(|(offer, model)| {
            (
                model.name.to_owned(),
                crate::ui::overlay::settings::SetupOffer {
                    id: offer.id,
                    title: offer.title,
                    preview: offer.preview,
                },
            )
        })
        .collect()
}

struct PreparedModel {
    changes: Vec<Change>,
    role: String,
    scope: Scope,
}

pub(crate) struct AuthJob {
    label: &'static str,
    receiver: std::sync::mpsc::Receiver<Result<String>>,
    cancel: Arc<AtomicBool>,
    /// Progress lines a streaming job sends while it runs; `None` for plain jobs.
    lines: Option<std::sync::mpsc::Receiver<String>>,
    /// Published only after preparation succeeds and the UI has not cancelled the job.
    model: Option<PreparedModel>,
}
impl Drop for AuthJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}
impl AuthJob {
    /// Progress lines received since the last call, tagged with the job label.
    fn drain_lines(slot: &mut Option<Self>) -> Vec<(&'static str, String)> {
        let Some(job) = slot.as_ref() else {
            return Vec::new();
        };
        let Some(lines) = job.lines.as_ref() else {
            return Vec::new();
        };
        lines.try_iter().map(|line| (job.label, line)).collect()
    }
    fn collect(slot: &mut Option<Self>, app: &App) -> Option<(&'static str, Result<String>)> {
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

        result.map(|(label, result)| {
            let job = slot.take().expect("completed background job");
            let result = result.and_then(|message| {
                ensure!(!job.cancel.load(Ordering::SeqCst), "operation cancelled");
                if let Some(model) = &job.model {
                    app.save_settings(model.scope, &model.changes, Some(&model.role))?;
                }
                Ok(message)
            });
            (label, result)
        })
    }
}

struct ComputeMonitor {
    snapshot: Arc<Mutex<Option<Value>>>,
    message: Arc<Mutex<Option<(String, bool)>>>,
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
        let config = app.config.clone();
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
                                ensure!(
                                    config.improve.enabled,
                                    "Automatic improvement is off for this model. Turn on Enabled in {} first.",
                                    crate::learn::improve::SETTINGS_PATH
                                );
                                auto::resume(&config.state_dir)?;
                                let store = Store::open(
                                    &config.state_dir,
                                    &config.model,
                                    &config.use_case,
                                    &config.harness,
                                )?;
                                store.set_meta("auto_enabled", "true")?;
                                auto::start_cycle(&layout, &config, &store)?
                                    .ensure_started(&config.state_dir)?;
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
                    *shared(&status) = Some(match result {
                        Ok(text) => (text, false),
                        Err(error) => (format!("{error:#}"), true),
                    });
                    next = Instant::now();
                }
                if Instant::now() >= next {
                    match crate::learn::compute_controls::snapshot(&layout.user_dir) {
                        Ok(value) => *shared(&output) = Some(value),
                        Err(error) => {
                            *shared(&status) =
                                Some((format!("Compute status unavailable: {error:#}"), true))
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

/// The compute release a panel may run on a compute action; absent without an agent (welcome).
fn overlay_compute_release<'a>(
    app: &'a App,
    agent: Option<&'a mut Agent<'_>>,
) -> Option<impl FnMut() + 'a> {
    agent.map(move |agent| {
        move || {
            app.release_idle_compute(agent);
        }
    })
}

/// Compute panel actions that start local training, which needs the session's compute lease and
/// unloaded models; stop, pause and inspection actions keep residency.
fn compute_starts(payload: &str) -> bool {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|action| {
            action["action"]
                .as_str()
                .map(|name| matches!(name, "start_current" | "start"))
        })
        .unwrap_or(false)
}

struct OverlayEnv<'a> {
    runtime: &'a crate::runtime::Tree,
    compute_monitor: Option<ComputeMonitor>,
    live_tasks: Option<&'a crate::agent::LiveTaskControl>,
    verify_lora: Option<ModelSelection>,
    auth_job: &'a mut Option<AuthJob>,
    extensions_pending: &'a mut Option<commands::extensions_browser::Pending>,
    extensions_job: &'a mut Option<commands::extensions_browser::InstallJob>,
    background_control: Option<&'a Arc<dyn crate::tools::ext::Tool>>,
    pty_approval: Approval,
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
    /// Unloads resident models before a compute action starts training. Absent in busy panels,
    /// where the chat loop's compute handoff runs while the panel is open.
    release_compute: Option<&'a mut dyn FnMut()>,
}

fn settings_tables(
    app: &App,
    effective: &Config,
) -> Result<(toml::Table, toml::Table, toml::Table)> {
    Ok((
        toml::Table::try_from(effective).context("serializing effective settings")?,
        settings_store::load_layer(&app.layout, Scope::Global)
            .context("reading global settings")?,
        settings_store::load_layer(&app.layout, Scope::Project)
            .context("reading project settings")?,
    ))
}

fn favorite_routes(config: &Config) -> Vec<crate::ui::overlay::models::FavoriteRoute> {
    config
        .model_favorites
        .iter()
        .map(|selection| {
            let effective = match config.selection_endpoint(selection) {
                Some(endpoint) => ModelSelection {
                    provider: selection.provider.clone(),
                    model: selection.model.clone(),
                    base_url: Some(endpoint.base_url),
                    api_key_env: endpoint.api_key_env,
                    allow_insecure: endpoint.allow_insecure,
                    protocol: endpoint.protocol,
                    headers: endpoint.headers,
                    account_id: endpoint.account_id,
                    context_window: selection.context_window,
                },
                None => selection.clone(),
            };
            crate::ui::overlay::models::FavoriteRoute {
                selection: selection.clone(),
                effective,
            }
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
            lines: None,
            model: None,
        });
        if let ActiveOverlay::Providers(providers) = overlay {
            providers.set_chatgpt_pending(label == "ChatGPT");
        }
        self.report(overlay, Ok(format!("Waiting for {label} sign-in in your browser; sign-in continues if you close this panel")));
    }
    fn collect_auth(&mut self, overlay: &mut ActiveOverlay) {
        // The event loop collects Smithery sign-ins so it can resume the parked command.
        if self
            .auth_job
            .as_ref()
            .is_some_and(|job| job.label == "Smithery")
        {
            return;
        }
        if self
            .auth_job
            .as_ref()
            .is_some_and(|job| job.model.is_some())
        {
            for (_, line) in AuthJob::drain_lines(self.auth_job) {
                overlay.banner(line, false);
            }
        }
        let result = AuthJob::collect(self.auth_job, self.app);
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
            let result = if provider == "Model" {
                result.and_then(|message| {
                    self.reload_saved()?;
                    Ok(message)
                })
            } else {
                result
            };
            // A finished embedding setup while memory is not semantic: offer the switch on close.
            let result = match result {
                Ok(message)
                    if provider == "Setup"
                        && message.starts_with(crate::setup::embeddings::SUMMARY_PREFIX) =>
                {
                    if let Ok(fresh) = self.app.reload() {
                        self.latest = Some(fresh);
                    }
                    Ok(match self.queue_semantic_offer() {
                        Some(note) => format!("{message}\n{note}"),
                        None => message,
                    })
                }
                other => other,
            };
            self.refresh_roles(overlay);
            self.report(overlay, result);
            self.catalog.refresh(self.app);
        }
    }

    /// After an embedding model is set up or assigned while memory is not semantic, queue the
    /// single-key switch offer for when this panel closes (decision: never switch silently).
    /// Returns the line that tells the user so.
    fn queue_semantic_offer(&mut self) -> Option<String> {
        use crate::config::memory_backend::MemoryBackendId;
        let config = self.latest.clone().or_else(|| self.app.reload().ok())?;
        if config.memory.enabled && config.memory.backend == MemoryBackendId::Semantic {
            return None;
        }
        if self.next.is_none() {
            self.next = Some(format!(
                "command:/setup memory {}",
                crate::setup::embeddings::OFFER_SEMANTIC_ARG
            ));
        }
        let current = if config.memory.enabled {
            config.memory.backend.as_str()
        } else {
            "off"
        };
        Some(format!(
            "Only semantic memory uses embeddings and memory is {current}: closing this panel asks whether to switch."
        ))
    }

    /// Persist, then reload the effective config (files plus command-line flags) and apply the UI part live.
    fn save(&mut self, changes: &[Change], scope: Scope) -> Result<Config> {
        self.app.save_settings(scope, changes, None)?;
        self.reload_saved()
    }

    fn reload_saved(&mut self) -> Result<Config> {
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
        crate::config::schema::get(&table, path)
            .cloned()
            .unwrap_or_else(|| toml::Value::String(String::new()))
    }

    /// Starts the dependency install and its settings save as a streaming background job.
    fn start_setup(&mut self, overlay: &mut ActiveOverlay, id: String) {
        let layout = self.app.layout.clone();
        let started = start_streaming_job(self.auth_job, "Setup", move |cancel, progress| {
            // `embeddings:<model>` ids pull a local embedding model; every other id is a dependency.
            if let Some(model) = crate::setup::embeddings::model_from_id(&id) {
                return crate::setup::embeddings::setup(&layout, model, &cancel, progress);
            }
            let saved = crate::setup::deps::run::install_and_save(&layout, &id, &cancel, progress)?;
            Ok(format!("Set up and saved:\n{}", saved.join("\n")))
        });
        if let (true, ActiveOverlay::Settings(settings)) = (started, &mut *overlay) {
            settings.set_setup_pending(true);
        }
        if !started {
            self.report(
                overlay,
                Err(anyhow::anyhow!(
                    "Another background operation is still running; try again when it finishes."
                )),
            );
        }
    }

    /// Returns true when the overlay should close.
    fn apply(&mut self, event: OverlayEvent, overlay: &mut ActiveOverlay) -> bool {
        match event {
            OverlayEvent::Close => {
                if self
                    .auth_job
                    .as_ref()
                    .is_some_and(|job| job.model.is_some())
                {
                    self.auth_job.take();
                }
                return true;
            }
            OverlayEvent::None => {}
            OverlayEvent::SetupDep { id } => self.start_setup(overlay, id),
            OverlayEvent::JobInput { name, bytes } => {
                let outcome = if matches!(self.pty_approval, Approval::Readonly) {
                    Err(anyhow::anyhow!(
                        "PTY input is unavailable in read-only approval mode"
                    ))
                } else if bytes.len() > 64 * 1024 {
                    Err(anyhow::anyhow!("interactive PTY input exceeds 64 KiB"))
                } else if let Some(background) = &self.background_control {
                    background
                        .attach_user_pty(&name, &bytes)
                        .map(|written| format!("sent {written} bytes to {name}"))
                        .map_err(anyhow::Error::msg)
                } else {
                    Err(anyhow::anyhow!("background job control is unavailable"))
                };
                self.report(overlay, outcome);
            }
            OverlayEvent::Set { path, value, scope } => {
                *self.scope = scope;
                if path == "model_favorites" {
                    let outcome = self.save(&[Change::Set { path, value }], scope);
                    self.report(overlay, outcome.map(|_| "Favorites saved".to_owned()));
                    if let (ActiveOverlay::Models(models), Some(config)) = (overlay, &self.latest) {
                        models.set_favorites(favorite_routes(config));
                    }
                } else {
                    let mut changes = vec![Change::Set {
                        path: path.clone(),
                        value,
                    }];
                    // The embedding override saves as a pair: add the half the panel held back.
                    let partner = crate::ui::overlay::settings::pair_partner(&path);
                    if let ActiveOverlay::Settings(settings) = &mut *overlay
                        && let Some((staged, staged_value)) = settings.take_staged_pair()
                        && Some(staged.as_str()) == partner
                    {
                        changes.push(Change::Set {
                            path: staged,
                            value: staged_value,
                        });
                    }
                    self.setting(overlay, &path, &changes, scope);
                }
            }
            OverlayEvent::Unset { path, scope } => {
                *self.scope = scope;
                let mut changes = vec![Change::Remove { path: path.clone() }];
                // Clearing one half of the embedding override clears the pair.
                if let Some(partner) = crate::ui::overlay::settings::pair_partner(&path) {
                    changes.push(Change::Remove {
                        path: partner.to_owned(),
                    });
                }
                self.setting(overlay, &path, &changes, scope);
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
                let embedding_role = role == "embedding";
                let outcome = (|| -> Result<String> {
                    let changes = selection_changes(&config, &selection, &role)?;
                    settings_store::preview_model_config(&self.app.layout, scope, &changes, &role)?;
                    let message = if role == "main" {
                        format!("switched to {}", selection.display_name())
                    } else {
                        format!("{role} set to {}", selection.display_name())
                    };
                    // Apple's on-device model lives in the OS; there is nothing to download or prepare.
                    if config.selection_endpoint(&selection).is_none()
                        && selection.provider != crate::llm::apple::PROVIDER
                    {
                        let model = selection.model.clone();
                        let python = config.python.clone();
                        let started =
                            start_streaming_job(self.auth_job, "Model", move |cancel, progress| {
                                let progress = RefCell::new(progress);
                                crate::llm::download::ensure_local_model_with(
                                    &python,
                                    &model,
                                    &crate::llm::download::Choice::Auto,
                                    &cancel,
                                    &|line| progress.borrow_mut()(line),
                                )?;
                                ensure!(
                                    !cancel.load(Ordering::SeqCst),
                                    "model selection cancelled"
                                );
                                Ok(message)
                            });
                        ensure!(
                            started,
                            "Another background operation is still running; try again when it finishes."
                        );
                        self.auth_job.as_mut().unwrap().model = Some(PreparedModel {
                            changes,
                            role,
                            scope,
                        });
                        Ok("Preparing model in the background; settings save only on success. Esc closes this panel and cancels preparation.".into())
                    } else {
                        ensure!(
                            !self
                                .auth_job
                                .as_ref()
                                .is_some_and(|job| job.model.is_some()),
                            "Cancel the pending model selection first (Esc)."
                        );
                        self.app.save_settings(scope, &changes, Some(&role))?;
                        self.reload_saved()?;
                        Ok(message)
                    }
                })();
                let assigned =
                    outcome.is_ok() && self.auth_job.as_ref().is_none_or(|job| job.model.is_none());
                let outcome = match outcome {
                    Ok(message) if assigned && embedding_role => {
                        Ok(match self.queue_semantic_offer() {
                            Some(note) => format!("{message}. {note}"),
                            None => message,
                        })
                    }
                    other => other,
                };
                self.report(overlay, outcome);
                self.refresh_roles(overlay);
                // The Settings Embedding model row shows the role and may complete a pending switch.
                if assigned
                    && let (ActiveOverlay::Settings(settings), Some(fresh)) =
                        (&mut *overlay, &self.latest)
                    && let Ok((values, global, project)) = settings_tables(self.app, fresh)
                {
                    settings.replace_values(values, project, global);
                    settings.roles_changed();
                }
            }
            OverlayEvent::ClearRole { role, scope } => {
                *self.scope = scope;
                let outcome =
                    clear_role_changes(&role, scope).and_then(|changes| self.save(&changes, scope));
                self.report(
                    overlay,
                    outcome.map(|config| {
                        if config.role_selection(&role).is_some() {
                            format!("{role}: {scope:?} assignment removed, but another scope still assigns a model; Ctrl+S switches scope")
                        } else {
                            format!("{role} assignment cleared")
                        }
                    }),
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
                let dir = self.app.layout.user_dir.clone();
                // A streaming job: the sign-in URL is progress, so it reaches the transcript even
                // when no browser opens and the user completes the callback by hand.
                let started =
                    start_streaming_job(self.auth_job, "OpenRouter", move |cancel, progress| {
                        let mut key = crate::llm::auth::openrouter_login(cancel, progress)?;
                        catalog::verify_and_save_credential(
                            &dir,
                            "openrouter",
                            catalog::OPENROUTER_BASE_URL,
                            key.as_mut_string(),
                        )?;
                        Ok("OpenRouter sign-in verified; key saved locally".into())
                    });
                if started {
                    self.report(
                        overlay,
                        Ok("Waiting for OpenRouter sign-in in your browser; sign-in continues if you close this panel".into()),
                    );
                } else {
                    self.report(
                        overlay,
                        Err(anyhow::anyhow!(
                            "An authentication action is already running"
                        )),
                    );
                }
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
                // A login still waiting for its callback would re-save tokens after sign-out.
                if self
                    .auth_job
                    .as_ref()
                    .is_some_and(|job| job.label == "Claude")
                {
                    self.auth_job.take();
                }
                let outcome = crate::llm::anthropic_oauth::sign_out(&self.app.layout.user_dir);
                self.report(overlay, outcome);
                self.catalog.refresh(self.app);
            }
            OverlayEvent::OAuthLogin { id } => match crate::llm::oauth::flow(&id) {
                Some(flow) => {
                    let dir = self.app.layout.user_dir.clone();
                    self.start_auth(overlay, flow.name, move |cancel| {
                        let ui = crate::llm::oauth::BannerInteraction { id: flow.id };
                        crate::llm::oauth::login(&dir, flow.id, &ui, cancel)
                    });
                }
                None => self.report(
                    overlay,
                    Err(anyhow::anyhow!("Unknown sign-in provider '{id}'")),
                ),
            },
            OverlayEvent::OAuthSignOut { id } => match crate::llm::oauth::flow(&id) {
                Some(flow) => {
                    if self
                        .auth_job
                        .as_ref()
                        .is_some_and(|job| job.label == flow.name)
                    {
                        self.auth_job.take();
                    }
                    let outcome = crate::llm::oauth::sign_out(&self.app.layout.user_dir, flow.id);
                    self.report(overlay, outcome);
                    self.catalog.refresh(self.app);
                }
                None => self.report(
                    overlay,
                    Err(anyhow::anyhow!("Unknown sign-in provider '{id}'")),
                ),
            },
            OverlayEvent::RemoveCredential { base_url } => {
                let dir = self.app.layout.user_dir.clone();
                let mut outcome = catalog::remove_credential(&dir, &base_url)
                    .map(|_| "API key removed".to_owned());
                // The Anthropic menu entry promises a sign-out too: drop a stored subscription grant.
                if base_url.trim_end_matches('/') == crate::llm::anthropic::BASE_URL
                    && crate::llm::anthropic_oauth::signed_in(&dir)
                {
                    outcome = outcome.and_then(|_| {
                        crate::llm::anthropic_oauth::sign_out(&dir).map(|_| {
                            "API key removed and Claude subscription signed out".to_owned()
                        })
                    });
                }
                self.report(overlay, outcome);
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
                if let Some(payload) = action.strip_prefix("extensions:") {
                    Shell::queue_extensions(self.extensions_pending, *self.scope, payload, overlay);
                } else if let Some(payload) = action.strip_prefix("compute:") {
                    if compute_starts(payload)
                        && let Some(release) = self.release_compute.as_mut()
                    {
                        // The training worker needs the compute lease and the resident server's memory.
                        release();
                    }
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
                    } else {
                        overlay.banner(
                            "Compute control is unavailable here; the compute monitor is not running.".into(),
                            true,
                        );
                    }
                    return false;
                }
                if action.starts_with("subagent-open:") || action.starts_with("subagent-hub") {
                    self.next = Some(action);
                    return true;
                }
                if let ActiveOverlay::SubagentViewer(viewer) = &mut *overlay
                    && action.starts_with("subagent-")
                {
                    match crate::ui::subagent_model::act(&action, self.live_tasks) {
                        Some(Ok(crate::ui::subagent_model::ActionOutcome::Banner(text))) => {
                            viewer.set_notice(text);
                        }
                        Some(Ok(crate::ui::subagent_model::ActionOutcome::Resume {
                            id,
                            message,
                        })) => {
                            self.next = Some(format!("subagent-resume:{id}\n{message}"));
                            return true;
                        }
                        Some(Err(error)) => viewer.set_notice(format!("{error:#}")),
                        None => {}
                    }
                    return false;
                }
                if let ActiveOverlay::Subagents(hub) = &mut *overlay
                    && action.starts_with("subagent-")
                {
                    if let Some(action) = hub.act(&action, self.live_tasks) {
                        self.next = Some(action);
                        return true;
                    }
                    return false;
                }
                if action.starts_with("open:")
                    || action.starts_with("command:")
                    || action.starts_with("test:")
                {
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
        if let ActiveOverlay::ModelSettings(settings) = overlay {
            let mut updates: Vec<_> = changes
                .iter()
                .map(|change| match change {
                    Change::Set { path, value } => Change::Set {
                        path: path.clone(),
                        value: value.clone(),
                    },
                    Change::Remove { path } => Change::Remove { path: path.clone() },
                })
                .collect();
            updates.push(Change::Set {
                path: "model_profile".into(),
                value: toml::Value::String(settings.name.clone()),
            });
            let result = self
                .save(&updates, scope)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"));
            settings.settled(result);
            return;
        }
        let ActiveOverlay::Settings(settings) = overlay else {
            return;
        };
        match self
            .save(changes, scope)
            .and_then(|fresh| settings_tables(self.app, &fresh).map(|tables| (fresh, tables)))
        {
            Ok((fresh, (values, global, project))) => {
                // Paired changes (the embedding override) settle every path they wrote.
                for other in changes
                    .iter()
                    .map(change_path)
                    .filter(|other| *other != path)
                {
                    settings.settled(other, Ok(Self::effective_value(&fresh, other)));
                }
                settings.settled(path, Ok(Self::effective_value(&fresh, path)));
                settings.replace_values(values, project, global);
            }
            Err(error) => {
                for other in changes
                    .iter()
                    .map(change_path)
                    .filter(|other| *other != path)
                {
                    settings.settled(other, Err(format!("{error:#}")));
                }
                settings.settled(path, Err(format!("{error:#}")));
                settings.banner(format!("{error:#}"), true);
            }
        }
    }

    /// Once a setup job this overlay started has settled, reload settings and re-probe
    /// dependencies so the open rows show the new state.
    fn refresh_setup(&mut self, overlay: &mut ActiveOverlay) {
        let ActiveOverlay::Settings(settings) = overlay else {
            return;
        };
        if !settings.setup_pending() || self.auth_job.is_some() {
            return;
        }
        settings.set_setup_pending(false);
        let (config, (values, global, project)) =
            match self.app.reload().and_then(|config| {
                settings_tables(self.app, &config).map(|tables| (config, tables))
            }) {
                Ok(values) => values,
                Err(error) => {
                    settings.banner(format!("{error:#}"), true);
                    return;
                }
            };
        settings.replace_values(values, project, global);
        apply_settings_hints(settings, &config);
        settings.set_setup_offers(setting_setup_offers(&self.app.layout.user_dir, &config));
    }

    fn report(&mut self, overlay: &mut ActiveOverlay, outcome: Result<String>) {
        match outcome {
            Ok(message) => overlay.banner(message, false),
            Err(error) => overlay.banner(format!("{error:#}"), true),
        }
    }

    fn refresh_roles(&mut self, overlay: &mut ActiveOverlay) {
        if let (ActiveOverlay::Models(models), Some(config)) = (overlay, &self.latest) {
            models.set_roles(config.role_statuses());
            models.set_memory_required(crate::config::memory_backend::requires_memory_role(
                &config.memory,
            ));
            models.set_memory_optional(
                crate::config::memory_backend::auto_retain_uses_memory_role(&config.memory),
            );
            models.set_favorites(favorite_routes(config));
        }
    }
}

/// What a blocking wait (compute lock, local preparation) does after one terminal event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitFlow {
    /// Keep waiting.
    Continue,
    /// The configured interrupt (or an explicit exit) abandons the wait.
    Cancel,
    /// A submitted message must run after the wait, so the wait ends without its work.
    Stop,
}

/// Owned panel state lets agent callbacks keep draining while an overlay is visible.
struct BusyOverlay {
    overlay: ActiveOverlay,
    paste_guard: crate::ui::input::PasteGuard,
    compute_monitor: Option<ComputeMonitor>,
    verify_lora: Option<ModelSelection>,
    latest: Option<Config>,
    next: Option<String>,
    seen_generation: u64,
}

fn refresh_overlay(env: &mut OverlayEnv<'_>, overlay: &mut ActiveOverlay) {
    if let Some(finished) =
        commands::extensions_browser::step(env.extensions_pending, env.extensions_job, env.app)
    {
        commands::extensions_browser::finish(finished, |changes, scope| {
            env.save(changes, scope).map(|_| ())
        });
    }
    if let ActiveOverlay::Palette(palette) = overlay {
        palette.set_busy(env.status.phase != Phase::Idle);
    }
    if let ActiveOverlay::Improve(panel) = &mut *overlay
        && panel.wants_refresh()
    {
        panel.update(improve_view(env.app));
    }
    if let (Some(monitor), ActiveOverlay::Compute(panel)) = (&env.compute_monitor, &mut *overlay) {
        if let Some(value) = shared(&monitor.snapshot).take() {
            panel.update(value);
        }
        if let Some((message, is_error)) = shared(&monitor.message).take() {
            panel.banner(message, is_error);
        }
    }
    if let ActiveOverlay::Extensions(panel) = overlay {
        panel.tick();
        if panel.take_refresh()
            && let Ok(config) = env.app.reload()
        {
            commands::extensions_browser::refresh(panel, env.app.layout(), &config);
        }
    }
    if let ActiveOverlay::Models(models) = &mut *overlay
        && models.take_refresh()
    {
        env.catalog.refresh(env.app);
    }
    if let ActiveOverlay::Subagents(hub) = &mut *overlay {
        hub.refresh(
            &env.app.layout,
            std::time::Instant::now(),
            env.runtime,
            &env.app.config.role_statuses(),
            env.live_tasks,
        );
    }
    if let ActiveOverlay::SubagentViewer(viewer) = &mut *overlay {
        viewer.refresh(
            std::time::Instant::now(),
            &env.app.layout,
            env.runtime,
            env.live_tasks,
        );
    }
    if let (Some(background), ActiveOverlay::Jobs(panel)) = (env.background_control, &mut *overlay)
    {
        let attached = panel.attached().map(str::to_owned);
        if let Some(view) = background.jobs_view(attached.as_deref()) {
            panel.set_jobs(
                view.jobs
                    .into_iter()
                    .map(|job| crate::ui::overlay::jobs::JobRow {
                        name: job.name,
                        command: job.command,
                        status: job.status,
                        running: job.running,
                        terminal: job.terminal,
                        mode: "session".into(),
                    })
                    .collect(),
            );
            panel.set_screen(view.screen, view.cursor);
        }
    }
    if let ActiveOverlay::Git(git) = overlay {
        git.tick();
    }
    env.refresh_setup(overlay);
    env.collect_auth(overlay);
    env.catalog.collect();
    if env.catalog.generation != env.seen_generation {
        env.seen_generation = env.catalog.generation;
        if let Some(discovery) = env.catalog.get() {
            match overlay {
                ActiveOverlay::Models(models) => models.set_discovery(discovery),
                ActiveOverlay::Providers(providers) => providers.set_discovery(discovery),
                ActiveOverlay::Settings(settings) => {
                    settings.set_embedding_models(settings_embedding_models(Some(&discovery)))
                }
                _ => {}
            }
        }
    }
    env.themes.reload_if_changed();
}

fn draw_overlay_frame(
    frame: &mut ratatui::Frame<'_>,
    env: &OverlayEnv<'_>,
    overlay: &mut ActiveOverlay,
) -> Vec<crate::ui::images::Placement> {
    let themes = &*env.themes;
    let preview = |cfg: &UiConfig, width: u16| {
        statusline::preview_row(cfg, env.status, themes.current(), width)
    };
    let ctx = DrawCtx {
        theme: themes.current(),
        keys: env.keys,
        status: env.status,
        preview_status: &preview,
    };
    overlay.draw(frame, &ctx);
    overlay.native_placements()
}

fn overlay_input(
    env: &mut OverlayEnv<'_>,
    overlay: &mut ActiveOverlay,
    paste_guard: &mut crate::ui::input::PasteGuard,
    mut event: Event,
) -> bool {
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
                        return false;
                    }
                    event = Event::Paste(value.into());
                }
            }
            _ => {}
        }
    }
    if matches!(event, Event::Resize(..)) {
        return false;
    }
    let outcome = overlay.handle(&event, env.keys);
    env.apply(outcome, overlay)
}

fn run_overlay(
    ot: &mut OverlayTerminal<'_>,
    env: &mut OverlayEnv<'_>,
    overlay: &mut ActiveOverlay,
) -> Result<()> {
    let mut paste_guard = crate::ui::input::PasteGuard::default();
    // Every panel unrolls from the top over a short reveal when it opens, then stays still.
    let opened = Instant::now();
    let motion = crate::ui::fx::motion(env.app.config.ui.motion, env.themes.current());
    loop {
        refresh_overlay(env, overlay);
        let progress = if motion {
            crate::ui::fx::progress(opened.elapsed(), crate::ui::fx::REVEAL)
        } else {
            1.0
        };
        ot.draw(|frame| {
            let placements = draw_overlay_frame(frame, env, overlay);
            if progress < 1.0 {
                let area = frame.area();
                crate::ui::fx::reveal(frame.buffer_mut(), area, progress);
                return Vec::new();
            }
            placements
        })?;
        let wait = if progress < 1.0 {
            crate::ui::fx::FRAME
        } else {
            TICK
        };
        if !poll(wait)? {
            continue;
        }
        if overlay_input(env, overlay, &mut paste_guard, read()?) {
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
    /// A successful `write` is previewed by the lines it put on disk: the result text is written
    /// for the model and carries no content. Capped so a huge file never reaches the transcript.
    fn attach_write_preview(&mut self, id: &str, output: &ToolOutput) {
        const PREVIEW_BYTES: u64 = 64 * 1024;
        if output.is_error {
            return;
        }
        let Some((name, summary)) = self.shell.transcript.tool_call_summary(id) else {
            return;
        };
        let path = summary.split_whitespace().next().unwrap_or_default();
        if name != "write" || path.is_empty() {
            return;
        }
        let full = self.app.layout.project_root.join(path);
        let Ok(file) = std::fs::File::open(&full) else {
            return;
        };
        let mut content = String::new();
        use std::io::Read as _;
        if file
            .take(PREVIEW_BYTES)
            .read_to_string(&mut content)
            .is_ok()
        {
            self.shell.transcript.tool_input(
                id,
                serde_json::json!({ "path": path, "content": content }).to_string(),
            );
        }
    }

    fn repaint(&mut self, force: bool) {
        if self.failed || (!force && self.last_paint.elapsed() < PAINT_EVERY) {
            return;
        }
        self.last_paint = Instant::now();
        self.pump();
        self.shell.refresh_resources(self.app, false, true);
        self.shell.refresh_workers(self.app);
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
        let paint_result = if self.shell.busy_overlay.is_some() {
            self.shell
                .step_busy_overlay(self.app, None)
                .map(|interrupt| {
                    if interrupt {
                        self.interrupt.store(true, Ordering::SeqCst);
                    }
                })
        } else {
            self.shell.paint(None)
        };
        if paint_result.is_err() {
            self.failed = true;
            self.interrupt.store(true, Ordering::SeqCst);
        }
    }

    /// Immediate feedback for an interrupt: the turn only stops at its next safe point, so name
    /// the tool it is waiting on. Activity stays on this label until the turn settles.
    fn show_stopping(&mut self) {
        let activity = match self.pending.last() {
            Some((_, name, _)) => format!("stopping after {name}"),
            None => "stopping".to_owned(),
        };
        // Set directly: `set_activity` holds the label once the interrupt flag is up.
        self.shell.status.activity = activity;
        self.repaint(true);
    }

    fn set_activity(&mut self, activity: &str) {
        if self.interrupt.load(Ordering::SeqCst) {
            return;
        }
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
        // Lands a finished push-to-talk transcript in the draft while the turn keeps running.
        self.shell.poll_voice();
        // Hosted guests reach a running turn only through here: an abort stops it, prompts wait.
        for notice in crate::collab::room::pump() {
            self.shell.note("Collab", notice);
        }
        if crate::collab::room::take_aborts() {
            self.interrupt.store(true, Ordering::SeqCst);
        }
        for _ in 0..64 {
            if !poll(Duration::ZERO).unwrap_or(false) {
                break;
            }
            let Ok(event) = read() else { break };
            if let Event::Key(key) = &event
                && self.shell.consume_voice_edge(key)
            {
                continue;
            }
            if self.shell.busy_overlay.is_some() {
                let cancel = matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release
                    && self.shell.keys.matches(Action::Interrupt, key)
                    && !self.shell.keys.matches(Action::SelectCancel, key));
                let result = if cancel {
                    if let Some(state) = &mut self.shell.busy_overlay {
                        state.next = None;
                    }
                    self.shell.finish_busy_overlay(self.app)
                } else {
                    self.shell.step_busy_overlay(self.app, Some(event))
                };
                match result {
                    Ok(stop) if stop || cancel => {
                        self.interrupt.store(true, Ordering::SeqCst);
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        self.failed = true;
                        self.interrupt.store(true, Ordering::SeqCst);
                        return;
                    }
                }
                continue;
            }
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
                            self.interrupt.store(true, Ordering::SeqCst);
                            self.show_stopping();
                        }
                        ComposerEvent::Submit(text) => {
                            if shell.route_busy_command(self.app, text) {
                                self.interrupt.store(true, Ordering::SeqCst);
                                return;
                            }
                            continue;
                        }
                        ComposerEvent::Action(Action::ModelSelect) => {
                            shell.route_busy_command(self.app, "/model".into());
                        }
                        ComposerEvent::Action(Action::ModelRoles) => {
                            shell.route_busy_command(self.app, "/roles".into());
                        }
                        ComposerEvent::Action(Action::ModelFavoriteNext) => {
                            shell.route_busy_command(self.app, "/model next".into());
                        }
                        ComposerEvent::Action(Action::ToolsExpand) => {
                            shell.tools_expanded = !shell.tools_expanded;
                            shell.display_reset();
                        }
                        ComposerEvent::Action(Action::ToolsToggle) => {
                            shell.ui.show_tools = !shell.ui.show_tools
                        }
                        ComposerEvent::Action(Action::ThinkingToggle) => {
                            shell.ui.show_thinking = !shell.ui.show_thinking
                        }
                        ComposerEvent::Action(Action::SubagentsHub) => {
                            shell.route_busy_command(self.app, "/agents".into());
                        }
                        ComposerEvent::Action(Action::Settings) => {
                            shell.route_busy_command(self.app, "/settings".into());
                        }
                        ComposerEvent::Action(Action::ProviderSetup) => {
                            shell.route_busy_command(self.app, "/providers".into());
                        }
                        ComposerEvent::Action(Action::EditExternal) => {
                            shell.deferred_editor = true;
                            shell.note("Editor", "External editor opens after the current turn.");
                        }
                        ComposerEvent::Action(Action::ApprovalCycle) => {
                            shell.deferred_approval_cycle = true;
                            shell.note("Approval", "approval mode changes after the current turn.");
                        }
                        ComposerEvent::Action(Action::DisplayReset) => {
                            shell.deferred_display_reset = true;
                            shell.note("Display", "display reset after the current turn.");
                        }
                        ComposerEvent::Action(Action::PasteImage) => {
                            if let Err(error) = shell.composer.paste_clipboard_image() {
                                shell.note(
                                    "Image",
                                    format!("clipboard image unavailable: {error:#}"),
                                );
                            }
                        }
                        ComposerEvent::Action(Action::VoiceInput) => shell.voice_press(self.app),
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
    fn renders_workers(&self) -> bool {
        true
    }
    fn runtime(&mut self, tree: &crate::runtime::Tree) {
        self.shell.runtime = tree.clone();
    }
    fn result_node(&mut self, body: &crate::runtime::Body) {
        let (tool, prose, running) = body.read(|n| {
            (
                n.kind == "tool",
                matches!(n.kind.as_str(), "assistant" | "reasoning"),
                n.result.phase == crate::runtime::Phase::Running,
            )
        });
        if tool {
            self.shell.pending_result = running.then(|| body.clone());
        } else if prose {
            self.shell.transcript.bind_prose(body);
            self.shell.shared_prose = running;
        }
    }
    fn maintenance_ready(&mut self) -> bool {
        self.pump();
        self.shell.startup.is_none()
            && self.shell.deferred.is_empty()
            && !self.shell.composer.has_queued_work()
            && self.shell.pending_config.is_none()
            && !self.shell.pending_resource_rebuild
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
        if self.shell.request_phase == agent::RequestPhase::Cancelling {
            return; // Ignore stale worker telemetry while cancellation settles.
        }
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
        self.set_activity(&if name.is_empty() {
            "preparing tool call".into()
        } else {
            format!("preparing {name}")
        });
        self.repaint(false);
    }
    fn tool_output(&mut self, name: &str, text: &str) {
        if let Some((id, _, _)) = self.pending.iter().rev().find(|(_, tool, _)| tool == name) {
            self.shell.transcript.tool_delta(id, text);
        }
        self.repaint(false);
    }
    fn tool_progress(&mut self, name: &str, text: &str) {
        self.shell.transcript.tool_progress(name, text);
        self.repaint(false);
    }
    fn background_output(&mut self, name: &str, text: &str, replace: bool) {
        self.shell.transcript.background_output(name, text, replace);
        self.repaint(false);
    }
    fn background_notice(&mut self, message: &str) {
        self.shell.transcript.notice("Background", message);
        self.repaint(true);
    }
    fn event(&mut self, event: &crate::agent::ui_event::UiEvent) {
        self.shell.transcript.event(event);
        self.repaint(true);
    }
    /// Cache hits are bookkeeping, not conversation: usage accounting keeps the counts for `/cost`,
    /// and the transcript stays about the work.
    fn cached_usage(&mut self, _read: Option<u64>, _write: Option<u64>) {}
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
            body.correlate(&id);
            self.shell.transcript.bind_tool(&id, body);
        }
        self.pending.push((id, name.to_owned(), Instant::now()));
        self.set_activity(&format!("running {name}"));
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
        self.attach_write_preview(&id, output);
        let activity = self.pending.last().map_or_else(
            || "thinking".to_owned(),
            |(_, name, _)| format!("running {name}"),
        );
        self.set_activity(&activity);
        self.repaint(true);
    }
    fn tool_call_with_id(&mut self, id: &str, name: &str, summary: &str) {
        self.shell.status.model_progress = None;
        self.shell.transcript.tool_start(id, name, summary, summary);
        if let Some(body) = self.shell.pending_result.take() {
            self.shell.transcript.bind_tool(id, body);
        }
        self.pending
            .push((id.to_owned(), name.to_owned(), Instant::now()));
        self.set_activity(&format!("running {name}"));
        self.repaint(true);
    }

    fn tool_call_with_args(&mut self, id: &str, name: &str, summary: &str, args: &Value) {
        self.tool_call_with_id(id, name, summary);
        self.shell.transcript.tool_input(id, args.to_string());
    }

    fn tool_presentation(&mut self, id: &str, view: &crate::tools::presentation::Presentation) {
        self.shell.transcript.tool_presentation(id, view);
    }

    fn tool_result_with_id(&mut self, id: &str, _name: &str, output: &ToolOutput) {
        let started = self
            .pending
            .iter()
            .position(|(pending_id, _, _)| pending_id == id)
            .map(|index| self.pending.remove(index).2)
            .unwrap_or_else(Instant::now);
        self.shell.transcript.tool_end(
            id,
            &output.text,
            output.is_error,
            started.elapsed().as_secs_f64(),
        );
        if let Some(data) = &output.data {
            self.shell.transcript.tool_data(id, data);
        }
        self.attach_write_preview(id, output);
        let activity = self.pending.last().map_or_else(
            || "thinking".to_owned(),
            |(_, name, _)| format!("running {name}"),
        );
        self.set_activity(&activity);
        self.repaint(true);
    }

    fn request_policy(&mut self, control: &Value) {
        self.shell.request_policy = control.clone();
    }
    fn notice(&mut self, message: &str) {
        // Provider retries arrive once per attempt. The first one is explained in plain words;
        // later attempts stay off the transcript (the status line already shows the wait).
        if let Some(retry) = RetryNotice::parse(message) {
            if !std::mem::replace(&mut self.shell.retry_noted, true) {
                self.shell.note(
                    "Retrying",
                    format!(
                        "The model request failed ({}). Dial retries up to {} times with growing waits; Esc stops.",
                        retry.cause, retry.max
                    ),
                );
            }
            self.repaint(true);
            return;
        }
        self.shell.note("Notice", message);
        self.repaint(true);
    }
    fn changed_files(&mut self, _paths: &[String]) {
        // The completion footer owns changed-file counts and review/undo actions.
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
        if matches!(status, "error" | "failed" | "unavailable") {
            self.shell.note("Decision advice", message);
        }
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

    /// The turn's outcome joins its closing rule in the transcript; changed files are already in
    /// the session rail and `/review-file` lists them.
    fn completion(&mut self, summary: &Value) {
        self.shell
            .transcript
            .completion(crate::ui::transcript::TurnOutcome {
                status: summary["outcome"].as_str().unwrap_or("unknown").to_owned(),
                changed_files: summary["changed_files"].as_array().map_or(0, Vec::len),
                open_tasks: summary["open_tasks"].as_u64().unwrap_or(0) as usize,
            });
        self.repaint(true);
    }
    fn request_phase(&mut self, phase: agent::RequestPhase) {
        let changed = phase != self.shell.request_phase;
        self.shell.request_phase = phase;
        if matches!(
            phase,
            agent::RequestPhase::Waiting
                | agent::RequestPhase::Retrying
                | agent::RequestPhase::Cancelling
        ) {
            self.shell.status.model_progress = None;
        }
        if changed && phase == agent::RequestPhase::Waiting {
            self.shell.transcript.end_text();
        }
        if changed || phase == agent::RequestPhase::Waiting {
            self.set_activity(phase.label());
            self.repaint(true);
        }
    }
    fn model_wait(&mut self, silence: Duration) {
        if self.shell.status.model_progress.is_some() {
            self.shell.status.model_silence_secs = silence.as_secs();
            return;
        }
        if silence.is_zero() {
            self.set_activity(self.shell.request_phase.label());
        } else if silence >= Duration::from_secs(15) {
            self.set_activity(&format!(
                "{} · no progress for {}s",
                self.shell.request_phase.label(),
                silence.as_secs()
            ));
        }
    }

    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        self.approve_with_cancel(tool, summary, &std::sync::atomic::AtomicBool::new(false))
    }

    fn approve_with_cancel(
        &mut self,
        tool: &str,
        summary: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Decision {
        self.shell.status.phase = Phase::Working;
        let choice = self.shell.ask_with_cancel(
            &format!("Allow {tool}?"),
            summary,
            &[
                ("Allow once", 'y'),
                ("Always allow this session", 'a'),
                ("Always allow in this project", 'p'),
                ("Deny", 'n'),
            ],
            Some(cancel),
        );
        let decision = match choice {
            Some(0) => Decision::Allow,
            Some(1) => Decision::AllowAlways,
            Some(2) => Decision::AllowProject,
            _ => Decision::Deny,
        };
        self.repaint(true);
        decision
    }

    fn interactive_input(&self) -> bool {
        true
    }

    fn ask_questions(
        &mut self,
        request: &agent::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> agent::questions::QuestionOutcome {
        use agent::questions::QuestionOutcome;
        let mut dialog = crate::ui::questions::Dialog::new(request, &self.app.layout.project_root);
        if request.notify && self.shell.ui.notify {
            crate::ui::termcaps::notify(
                crate::ui::termcaps::NotifyKind::Question,
                "Dial",
                "A response is needed",
                &self.shell.session_id,
            );
        }
        let outcome = loop {
            if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) {
                break QuestionOutcome::Cancelled;
            }
            let (width, height) = self.shell.term.size();
            let rows = crate::ui::questions::framed(
                dialog.rows(
                    usize::from(width).saturating_sub(4),
                    usize::from(height).saturating_sub(5),
                ),
                usize::from(width),
                usize::from(height).saturating_sub(1),
                self.shell.themes.current(),
            );
            if self.shell.paint(Some(rows)).is_err() {
                break QuestionOutcome::Disconnected;
            }
            if let Err(error) = dialog.presented() {
                break QuestionOutcome::Unsupported(error);
            }
            match poll(Duration::from_millis(50)) {
                Ok(false) => {
                    if let Some(outcome) = dialog.expired() {
                        break outcome;
                    }
                    continue;
                }
                Err(_) => break QuestionOutcome::Disconnected,
                Ok(true) => {}
            }
            match read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    if let Some(outcome) = dialog.key(key, &self.shell.keys) {
                        break outcome;
                    }
                }
                Ok(Event::Paste(value)) => dialog.paste(&value),
                Ok(Event::Resize(..)) => self.shell.term.on_resize(),
                Ok(_) => {}
                Err(_) => break QuestionOutcome::Disconnected,
            }
        };
        self.repaint(true);
        outcome
    }

    fn ask_user(&mut self, title: &str, question: &str, options: &[String]) -> Option<String> {
        let mut choices: Vec<(String, char)> = options
            .iter()
            .enumerate()
            .map(|(index, option)| (option.clone(), char::from(b'1'.wrapping_add(index as u8))))
            .collect();
        choices.push(("Other (type your own)".into(), 'o'));
        let choice_refs: Vec<(&str, char)> = choices
            .iter()
            .map(|(label, key)| (label.as_str(), *key))
            .collect();

        let answer = match self.shell.ask(title, question, &choice_refs) {
            Some(index) if index < options.len() => Some(options[index].clone()),
            Some(_) => {
                let mut editor = crate::ui::editor::Editor::default();
                let prompt = format!("{} (Enter sends, Esc cancels)", text::one_line(question));
                let mut answer = None;
                loop {
                    let (width, _) = self.shell.term.size();
                    let theme = self.shell.themes.current();
                    let rows = vec![
                        Line::from(Span::styled(
                            text::clip(&prompt, width as usize),
                            theme.fg(Tok::Warning),
                        )),
                        Line::from(text::clip(&text::one_line(&editor.text), width as usize)),
                    ];
                    if self.shell.paint(Some(rows)).is_err() {
                        break;
                    }
                    match read() {
                        Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                            match key.code {
                                KeyCode::Enter => {
                                    if !editor.text.trim().is_empty() {
                                        answer = Some(editor.text);
                                        break;
                                    }
                                }
                                KeyCode::Esc => break,
                                KeyCode::Char('c')
                                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    break;
                                }
                                _ => {
                                    editor.key(key);
                                }
                            }
                        }
                        Ok(Event::Paste(value)) => editor.insert(&text::one_line(&value)),
                        Ok(Event::Resize(..)) => self.shell.term.on_resize(),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                answer
            }
            None => None,
        };
        self.repaint(true);
        answer
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

/// Title of the agent's stop panel. The summary/raw toggle applies only to this panel.
const RECOVERY_PANEL: &str = "Recovery needs direction";

fn panel_rows_window(
    theme: &crate::ui::theme::Theme,
    width: u16,
    title: &str,
    body: &str,
    options: &[(&str, char)],
    selected: usize,
    offset: usize,
    height: usize,
) -> Rows {
    let width = width as usize;
    if width < 6 {
        return vec![Line::from(text::clip(title, width))];
    }
    let inner = panel_inner(width as u16);
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
    // Same title shape as every panel: a mark, then the title in bold body text. Tiny panels
    // have no room for the mark.
    let mark = if width < 14 {
        ""
    } else if theme.ascii() {
        "*"
    } else {
        "\u{25c6}"
    };
    let title_text = text::ellipsize(
        &text::one_line(title),
        inner.saturating_sub(4),
        theme.ascii(),
    );
    let title_text = if mark.is_empty() {
        title_text
    } else {
        format!(" {title_text}")
    };
    let dashes = width.saturating_sub(text::width(&title_text) + text::width(mark) + 5);
    rows.push(Line::from(vec![
        Span::styled(
            format!("{}{} ", theme.sym(Sym::BoxTl), theme.sym(Sym::BoxH)),
            border,
        ),
        Span::styled(mark, border.add_modifier(ratatui::style::Modifier::BOLD)),
        Span::styled(
            format!("{title_text} "),
            theme
                .fg(Tok::Text)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{}{}",
                theme.sym(Sym::BoxH).repeat(dashes),
                theme.sym(Sym::BoxTr)
            ),
            border,
        ),
    ]));
    let lines = text::wrap_words(&text::sanitize(body), inner);
    let body_limit = panel_body_limit(options.len(), height);
    let offset = offset.min(lines.len().saturating_sub(body_limit));
    for line in lines.iter().skip(offset).take(body_limit) {
        rows.push(pad_row(vec![Span::styled(
            text::clip(line, inner),
            theme.fg(Tok::ToolOutput),
        )]));
    }
    let option_limit = options.len().min(height.saturating_sub(5).max(1));
    let start = selected
        .saturating_sub(option_limit.saturating_sub(1))
        .min(options.len().saturating_sub(option_limit));
    for (index, (label, hotkey)) in options.iter().enumerate().skip(start).take(option_limit) {
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
    let hint = if lines.len() > body_limit {
        format!(
            "Choose · Enter confirm · Esc cancel · Page keys scroll details {}-{} of {}",
            offset + 1,
            (offset + body_limit).min(lines.len()),
            lines.len()
        )
    } else {
        "Choose · Enter confirm · Esc cancel · Page keys scroll details".to_owned()
    };
    rows.push(pad_row(vec![Span::styled(hint, theme.fg(Tok::Dim))]));
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

/// Text columns inside a modal panel's border and padding.
fn panel_inner(width: u16) -> usize {
    (width as usize).saturating_sub(4).max(1)
}

/// Body rows a modal panel shows for `options` choices within `height` rows: the same budget the renderer and
/// the page keys use, so paging never jumps past rows the user has not seen.
fn panel_body_limit(options: usize, height: usize) -> usize {
    let option_limit = options.min(height.saturating_sub(5).max(1));
    height.saturating_sub(option_limit + 3).clamp(1, 8)
}

/// The next body offset for a page key: one visible page minus a row of overlap, never past the last page.
fn panel_scroll(offset: usize, lines: usize, visible: usize, down: bool) -> usize {
    let page = visible.saturating_sub(1).max(1);
    if down {
        offset
            .saturating_add(page)
            .min(lines.saturating_sub(visible))
    } else {
        offset.saturating_sub(page)
    }
}

/// Show an earlier session's conversation as transcript blocks (for `dial -c`).
fn replay_session(transcript: &mut Transcript, session: &Session) {
    replay(transcript, &session.messages());
    for body in session.runtime().bodies("tool") {
        if let Some(id) = body.read(|node| node.call_id.clone()) {
            transcript.bind_tool(&id, body);
        }
    }
    let streams = session.interrupted_streams();
    if !streams.is_empty() {
        transcript.notice(
            "Interrupted response",
            "Recovered output from a response that did not complete.",
        );
    }
    for body in streams {
        transcript.restore_prose(&body);
    }
}

fn replay(transcript: &mut Transcript, messages: &[Value]) {
    let mut replay = crate::ui::transcript::Replay::default();
    for message in messages {
        replay.push(transcript, message);
    }
    transcript.finish_turn();
}

#[cfg(test)]
mod tests;

/// A provider retry notice: `"<reason>: <cause>; retrying in 3.4s (attempt 4/10)"`.
struct RetryNotice {
    cause: String,
    max: String,
}

impl RetryNotice {
    fn parse(message: &str) -> Option<Self> {
        let (head, tail) = message.rsplit_once("; retrying in ")?;
        let attempts = tail.rsplit_once("(attempt ")?.1.strip_suffix(')')?;
        let (_, max) = attempts.split_once('/')?;
        let cause = head
            .split_once(": ")
            .map_or(head, |(_, cause)| cause)
            .trim();
        Some(Self {
            cause: cause.to_owned(),
            max: max.trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod retry_notice_tests;

/// `32768` → `32,768`, for counts people read rather than parse.
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod group_digits_tests;

#[cfg(test)]
mod command_registry_tests;
