//! The interactive shell: the `Shell` state that owns the terminal, transcript and composer.

use crate::agent;
use crate::agent::window::context;
use crate::agent::workflow::todo::TodoList;
use crate::app::discovery::Catalog;
use crate::app::git::Git;
use crate::app::learning::Learning;
use crate::app::overlay::ImproveWatch;
use crate::app::overlay::auth::AuthJob;
use crate::app::overlay::run::BusyOverlay;
use crate::app::resources::ResourceWatch;
use crate::app::selection::PendingFavorite;
use crate::app::shell::teach::TeachState;
use crate::config::settings_store::Scope;
use crate::config::types::{Approval, Config};
use crate::llm::model_config::{ModelSelection, UiConfig};
use crate::storage::skills;
use crate::ui::composer::Composer;
use crate::ui::keys::Keymap;
use crate::ui::status::StatusData;
use crate::ui::term::InlineTerminal;
use crate::ui::theme::ThemeManager;
use crate::ui::transcript::Transcript;
use crate::ui::welcome;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

pub(in crate::app) mod busy;
mod checkpoint_review;
pub(in crate::app) mod command;
pub(in crate::app) mod help;
pub(in crate::app) mod improve;
pub(in crate::app) mod input;
pub(in crate::app) mod lifecycle;
mod live;
pub(in crate::app) mod notes;
pub(in crate::app) mod overlays;
pub(in crate::app) mod paint;
pub(in crate::app) mod pickers;
pub(in crate::app) mod prompts;
pub(in crate::app) mod run;
pub(in crate::app) mod sessions;
pub(in crate::app) mod submit;
pub(in crate::app) mod teach;
pub(in crate::app) mod theme;
pub(in crate::app) mod turn;
pub(in crate::app) mod voice;
pub(in crate::app) mod wait;

/// How long a keystroke may go unnoticed while the UI is idle; also the animation frame time.
pub(super) const TICK: Duration = Duration::from_millis(80);

/// Learning state is a store query, so it is refreshed on a timer, not per keystroke.
pub(super) const STATUS_TTL: Duration = Duration::from_secs(2);

/// Streaming text repaints at most this often; tool events always repaint.
pub(super) const PAINT_EVERY: Duration = Duration::from_millis(40);

pub(super) struct Shell {
    pub(super) loop_mode: crate::agent::modes::loop_runner::LoopMode,
    pub(super) runtime: crate::runtime::Tree,
    pub(super) pending_result: Option<crate::runtime::Body>,
    pub(super) shared_prose: bool,
    pub(super) auth_job: Option<AuthJob>,
    /// `/extensions` intent waiting for the main loop to start it.
    pub(crate) extensions_pending: Option<crate::app::commands::extensions_browser::Pending>,
    /// The running `/extensions` install, if any.
    pub(crate) extensions_job: Option<crate::app::commands::extensions_browser::InstallJob>,
    /// Set by `/restart`; `run_interactive` relaunches the binary once the terminal is restored.
    pub(super) restart_requested: bool,
    /// A provider retry was already explained in this turn; later attempts update nothing new.
    pub(super) retry_noted: bool,
    pub(super) smithery_resume: Option<crate::app::commands::smithery::Resume>,
    pub(super) commit_job: Option<crate::app::commands::commit::CommitJob>,
    pub(super) busy_overlay: Option<BusyOverlay>,
    pub(super) term: InlineTerminal,
    pub(super) themes: ThemeManager,
    pub(super) keys: Keymap,
    pub(super) composer: Composer,
    pub(super) paste_guard: crate::ui::input::PasteGuard,
    pub(super) transcript: Transcript,
    pub(super) status: StatusData,
    usage_status: crate::app::commands::UsageStatus,
    pub(super) ui: UiConfig,
    pub(super) catalog: Catalog,
    pub(super) git: Git,
    pub(super) learning: Learning,
    yield_compute: bool,
    pub(super) adapter_monitor: Option<crate::llm::provider::AdapterMonitor>,
    pub(super) request_policy: Value,
    pub(super) commands: Vec<context::CommandDef>,
    pub(super) scope: Scope,
    pub(super) tools_expanded: bool,
    /// The live home screen, shown above the composer until the conversation starts.
    pub(super) home: Option<welcome::Home>,
    pub(super) started: Instant,
    pub(super) request_phase: agent::RequestPhase,
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
    pub(super) startup: Option<String>,
    /// The configured approval mode last applied to the agent (session cycling may differ from it).
    cfg_approval: Approval,
    pub(super) dark: Option<bool>,
    pub(super) session_id: String,
    pub(super) title: String,
    pub(super) turn_output_tokens: u64,
    /// The agent's task list as last reported; drawn above the composer while any task is open.
    pub(super) todo: TodoList,
    /// Skills found at the last command refresh, so `/help` and `/skills` work while a turn runs.
    pub(super) skills: Vec<skills::Skill>,
    skills_listing: String,
    /// Commands typed during a turn that need the agent but are not meant to stop it; they run, in order,
    /// as soon as the turn finishes.
    pub(super) deferred: std::collections::VecDeque<String>,
    pub(super) deferred_editor: bool,
    /// Shift+Tab / display reset pressed during a turn; applied at the next idle boundary.
    pub(super) deferred_approval_cycle: bool,
    pub(super) deferred_display_reset: bool,
    /// Input read before the main loop could take it (typing while the first agent is built);
    /// the loop handles these before reading the terminal again.
    pub(super) held: std::collections::VecDeque<crossterm::event::Event>,
    /// Handle to the agent's live jobs, usable while a turn holds the agent.
    pub(super) live_tasks: Option<crate::agent::LiveTaskControl>,
    /// Session workers for the pinned block, the `workers` segment and finish notices.
    pub(super) workers: crate::ui::worker_pins::WorkerBoard,
    /// Worker last opened by `Alt+Shift+A`, so repeated presses cycle through the running ones.
    recent_worker: Option<String>,
    pub(super) background_control: Option<Arc<dyn crate::tools::integrations::ext::Tool>>,
    pub(super) pty_approval: Approval,
    maintenance_control: Option<crate::agent::compact::compaction_runtime::CompactionControl>,
    pub(super) pending_config: Option<Config>,
    pending_verification: Option<ModelSelection>,
    /// A worker resume raised during a turn; launched when the turn settles.
    pending_resume: Option<String>,
    pending_favorite: Option<PendingFavorite>,
    pub(super) resources: ResourceWatch,
    pub(super) pending_resource_rebuild: bool,
    /// Push-to-talk capture in progress (`Alt+V`); its transcript goes into the draft.
    voice_input: Option<crate::voice::tui::PushToTalk>,
    /// Press/release state for the voice key; decides hold-to-talk or press-to-toggle.
    voice_gesture: crate::voice::gesture::Gesture,
    /// The read-out of the last answer (`voice.tts.auto_speak`) and the flag that stops it.
    speech: Option<(Arc<AtomicBool>, std::thread::JoinHandle<anyhow::Result<()>>)>,
}
