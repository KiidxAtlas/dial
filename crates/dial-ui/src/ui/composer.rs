//! The composer: the input box with the status line in its top border and key hints in its bottom
//! border, a fuzzy `/` command menu and an incremental history search below it, history browsing,
//! inline ghost suggestions and bracketed-paste collapsing.
//!
//! The component is pure: [`Composer::handle_key`] maps a key to a [`ComposerEvent`], and
//! [`Composer::view`] returns styled rows plus the cursor position. Every app-level shortcut comes
//! from the [`Keymap`], every color and glyph from the [`Theme`], so hints and behavior always agree.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::llm::model_config::{BusySend, UiConfig};

use super::word_complete::{WordIndex, WordModel};
use super::{
    Rows,
    editor::Editor,
    keys::{Action, Keymap},
    status::{InputMode, StatusData},
    theme::Theme,
};
#[cfg(test)]
use super::{
    statusline, text,
    theme::{Sym, Tok},
};
#[cfg(test)]
use crate::llm::model_config::ComposerShape;
use menu::{SYNC_RANK_LIMIT, rank, rank_until};
use paste::expand_pastes;
#[cfg(test)]
use paste::quote_mention;
#[cfg(test)]
use queue::STEER_FADE;
use queue::{DRAFT_CHOICES, QUEUE_CHOICES};
#[cfg(test)]
use ratatui::style::Modifier;
use render::list_rows;
#[cfg(test)]
use render::{LANDING_PLACEHOLDERS, MAX_INPUT_ROWS, MIN_BOX_WIDTH, PLACEHOLDERS};

pub mod attachments;
mod completion;
mod history;
mod menu;
mod paste;
mod queue;
pub mod recovery;
mod render;

/// App-level shortcuts the composer reports instead of handling (owned by the keymap, so the
/// shortcut editor can flag editing chords they shadow).
const APP_ACTIONS: [Action; 20] = Action::COMPOSER_APP;

/// Two presses of one key inside this window count as a double tap.
const DOUBLE_TAP_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

/// Reports whether a press completes a double tap with the previous one.
#[derive(Default)]
pub struct DoubleTap {
    last: Option<std::time::Instant>,
}

impl DoubleTap {
    /// Records a press. A completed pair clears state so a third press starts over.
    pub fn press(&mut self, now: std::time::Instant) -> bool {
        let hit = self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) <= DOUBLE_TAP_WINDOW);
        self.last = if hit { None } else { Some(now) };
        hit
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    Builtin,
    Custom,
    Skill,
    /// A project file offered by `@` mentions.
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    pub kind: CommandKind,
    pub args_hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposerEvent {
    None,
    Submit(String),
    Interrupt,
    Exit,
    /// An application-level shortcut (model selector, settings, tool toggles, …).
    Action(Action),
}

/// Whether submitted text is a slash command. Command names never contain a slash, so text whose
/// first word has a second `/` (a pasted path such as `/Users/me/notes.md`) is an ordinary message.
pub fn is_slash_command(text: &str) -> bool {
    text.trim_start()
        .strip_prefix('/')
        .and_then(|rest| rest.split(char::is_whitespace).next())
        .is_some_and(|name| !name.contains('/'))
}

#[derive(Clone, Copy)]
pub struct ComposerCtx<'a> {
    pub theme: &'a Theme,
    pub keys: &'a Keymap,
    pub ui: &'a UiConfig,
    pub status: &'a StatusData,
}

#[derive(Clone, Debug)]
pub struct ComposerView {
    pub rows: Rows,
    /// `(row, column)` within `rows`.
    pub cursor: (u16, u16),
}

struct Match {
    index: usize,
    name_hits: Vec<usize>,
    desc_hits: Vec<usize>,
}

/// `(generation, query, file_start, ranked matches)` produced by a background ranking.
type AutocompleteResult = (u64, String, Option<usize>, Vec<Match>);

struct Menu {
    query: String,
    file_start: Option<usize>,
    arguments: Option<Vec<CommandEntry>>,
    matches: Vec<Match>,
    selected: usize,
    /// Enter accepts a command suggestion only after deliberate list navigation.
    navigated: bool,
    scroll: usize,
    /// `true` while a background ranking for `query` is still running.
    pending: bool,
}

struct Search {
    query: Editor,
    entries: Vec<String>,
    selected: usize,
    scroll: usize,
}

/// A queued message loaded into the composer for editing, kept so cancelling restores it exactly.
struct QueuedEdit {
    index: usize,
    text: String,
    follow_up: bool,
}

/// A decision the busy composer offers before it acts on a message. Nothing changes until one is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BusyChoice {
    /// Let the agent decide whether this changes the current task or starts independent work.
    Auto,
    /// Explicitly redirect the running task at its next safe point.
    Redirect,
    /// Request separate work without redirecting the running task.
    Independent,
    /// Send the draft after the running turn finishes.
    AfterTurn,
    /// Leave the queue alone and let the turn finish.
    KeepWorking,
    /// Stop the running turn and send the oldest queued message.
    StopAndSend,
}

/// Where an explicit message is in its journey. Only states the agent actually reported are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SteerStage {
    /// Typed here; the agent has not taken it yet (it acts at the next safe point).
    Pending,
    /// The controller took it; delivery or local replanning has not yet been confirmed.
    Received,
    /// Recorded as the current instruction (steering) or a worker was started (independent).
    Applied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SteerKind {
    Steer,
    Independent,
}

struct Explicit {
    text: String,
    kind: SteerKind,
    stage: SteerStage,
    /// Not yet handed to the agent through `take_explicit_*`.
    untaken: bool,
    /// When it reached [`SteerStage::Applied`]; its row fades [`STEER_FADE`] later.
    applied_at: Option<std::time::Instant>,
}

/// An open busy chooser: its options and the highlighted one. It holds no draft state, so closing it
/// never touches the draft, pastes or images.
struct Choice {
    options: &'static [BusyChoice],
    selected: usize,
}

/// Returns newest-first history entries matching a case-insensitive substring, up to a limit.
type HistoryProvider = std::sync::Arc<dyn Fn(&str, usize) -> Vec<String> + Send + Sync>;

/// One in-flight lookup and one replaceable, debounced request: slow providers never block input
/// or grow a thread per keystroke. Results are collected by the existing autocomplete poll loop.
struct HistoryLookup {
    tx: std::sync::mpsc::Sender<(u64, Vec<String>)>,
    rx: std::sync::mpsc::Receiver<(u64, Vec<String>)>,
    epoch: u64,
    query: Option<String>,
    pending: Option<(u64, String, std::time::Instant)>,
    running: bool,
}

impl HistoryLookup {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            epoch: 0,
            query: None,
            pending: None,
            running: false,
        }
    }
}

pub struct Composer {
    editor: Editor,
    attachments: attachments::Preflight,
    double_left: DoubleTap,
    history: Vec<String>,
    history_limit: usize,
    history_index: Option<usize>,
    saved_draft: Option<Editor>,
    commands: Vec<CommandEntry>,
    command_context: crate::ui::command_surface::AvailabilityContext,
    /// Push-to-talk is capturing: `Some(hold)` while recording (`hold` means release ends it).
    recording: Option<bool>,
    argument_completions: Vec<String>,
    /// Shared with the background ranker, so large file lists are not copied per keystroke.
    files: std::sync::Arc<Vec<CommandEntry>>,
    autocomplete_rx: std::sync::mpsc::Receiver<AutocompleteResult>,
    autocomplete_tx: std::sync::mpsc::Sender<AutocompleteResult>,
    /// Bumped whenever the candidate lists change; background results from older generations are dropped.
    candidates_generation: u64,
    /// Latest background-ranking request; a worker whose request is no longer the latest stops.
    rank_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    history_provider: Option<HistoryProvider>,
    history_lookup: HistoryLookup,
    menu: Option<Menu>,
    /// The exact draft the user dismissed the menu/ghost on; editing the text ends the dismissal.
    dismissed: Option<String>,
    search: Option<Search>,
    busy: bool,
    busy_send: BusySend,
    queued: std::collections::VecDeque<(String, bool)>,
    recovery_held: bool,
    recovery_sink: Option<recovery::RecoverySink>,
    /// Explicit steer / independent messages sent this turn, shown with their stage. Not part of the queue.
    explicit: Vec<Explicit>,
    /// The queued message currently loaded into the draft. Cancelling or replacing the draft restores it
    /// unchanged at its original position; only submitting replaces it.
    editing_queue: Option<QueuedEdit>,
    /// The busy chooser open above the box, if any.
    choice: Option<Choice>,
    /// The draft route last picked in the busy chooser this session; the next chooser opens on it.
    /// Enter still confirms, so remembering never sends anything by itself.
    last_draft_choice: Option<BusyChoice>,
    queue_notice: Option<String>,
    exit_armed: bool,
    /// Ctrl+D was pressed once on an empty idle draft; a second press exits.
    eof_armed: bool,
    /// The last key edited the draft (or only stepped an open menu), so an `@file` word ending at
    /// a mid-draft cursor is being typed rather than passed over by cursor movement.
    mention_edit: bool,
    /// Payloads referenced by the draft, saved draft, undo/redo or bounded kill rings.
    pastes: Vec<(String, String)>,
    paste_count: usize,
    /// Keep pasted-image files alive for the session because message expansion happens after submit.
    pasted_images: Vec<tempfile::TempPath>,
    /// A clipboard image being read on a worker thread; the shell attaches it when it arrives.
    clipboard_read: Option<std::sync::mpsc::Receiver<anyhow::Result<Vec<u8>>>>,
    /// Learned words; `word_store` is the file they are saved to, when one is configured.
    words: WordModel,
    word_store: Option<std::path::PathBuf>,
    /// Mirrors `ui.word_completion`; when false no ghost word is offered.
    word_completion: bool,
    project_root: Option<std::path::PathBuf>,
    /// Identifiers and file-name stems of the project, scanned off the UI thread.
    project_words: WordIndex,
    vocab_tx: std::sync::mpsc::Sender<(u64, WordIndex)>,
    vocab_rx: std::sync::mpsc::Receiver<(u64, WordIndex)>,
    vocab_generation: u64,
    vocab_cancel: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// A load or save problem with the learned words, reported once by the app.
    word_notice: Option<String>,
    /// Whether the terminal speaks the kitty keyboard protocol; `None` asks the live terminal.
    /// Hints never advertise a chord the terminal cannot report (Shift+Enter on legacy terminals).
    keyboard_enhanced: Option<bool>,
    /// A busy Ctrl+C just cleared the draft; the next one stops the turn (hint only).
    stop_armed: bool,
}

impl Composer {
    pub fn new(history: Vec<String>, history_limit: usize) -> Self {
        let (autocomplete_tx, autocomplete_rx) = std::sync::mpsc::channel();
        let (vocab_tx, vocab_rx) = std::sync::mpsc::channel();
        let mut composer = Composer {
            editor: Editor::default(),
            attachments: Default::default(),
            history: Vec::new(),
            history_limit,
            history_index: None,
            saved_draft: None,
            commands: Vec::new(),
            command_context: Default::default(),
            menu: None,
            dismissed: None,
            search: None,
            busy: false,
            busy_send: BusySend::Choose,
            recording: None,
            exit_armed: false,
            eof_armed: false,
            mention_edit: false,
            pastes: Vec::new(),
            clipboard_read: None,
            paste_count: 0,
            pasted_images: Vec::new(),
            queued: std::collections::VecDeque::new(),
            recovery_held: false,
            recovery_sink: None,
            explicit: Vec::new(),
            editing_queue: None,
            choice: None,
            last_draft_choice: None,
            queue_notice: None,
            files: std::sync::Arc::new(Vec::new()),
            argument_completions: Vec::new(),
            history_provider: None,
            history_lookup: HistoryLookup::new(),
            double_left: DoubleTap::default(),
            autocomplete_rx,
            autocomplete_tx,
            candidates_generation: 0,
            rank_epoch: std::sync::Arc::default(),
            words: WordModel::default(),
            word_store: None,
            word_completion: true,
            project_root: None,
            project_words: WordIndex::default(),
            vocab_tx,
            vocab_rx,
            vocab_generation: 0,
            vocab_cancel: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            word_notice: None,
            keyboard_enhanced: None,
            stop_armed: false,
        };
        for entry in history {
            composer.push_history(entry);
        }
        composer
    }

    pub fn draft(&self) -> String {
        self.editor.text.clone()
    }

    /// Whether a turn is running (see [`Composer::set_busy`]).
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Whether the draft is empty and nothing (chooser, menu, search) is open over it, so a
    /// shortcut that acts on the conversation cannot be mistaken for editing.
    pub fn is_resting(&self) -> bool {
        self.editor.text.is_empty()
            && self.choice.is_none()
            && self.menu.is_none()
            && self.search.is_none()
    }

    /// The draft with collapsed paste placeholders replaced by their text, for handing to an editor.
    pub fn expanded_draft(&self) -> String {
        expand_pastes(&self.editor.text, &self.pastes)
    }

    pub fn set_draft(&mut self, text: &str) {
        self.editor.set(text);
        self.history_index = None;
        self.saved_draft = None;
        self.exit_armed = false;
        self.eof_armed = false;
        self.mention_edit = false;
        self.dismissed = None;
        self.prune_pastes();
        self.choice = None;
        // Replacing the draft abandons an in-progress queue edit: put the original back, never drop it.
        self.restore_queued_edit();
        self.queue_notice = None;
        self.refresh();
    }

    pub fn clear(&mut self) {
        self.set_draft("");
    }

    /// Apply the explicitly saved busy-send preference. Sending never changes this preference.
    pub fn set_busy_send(&mut self, preference: BusySend) {
        if self.busy_send != preference {
            self.busy_send = preference;
            self.choice = None;
        }
    }

    /// What Enter does with a draft while busy. With the chooser, a route picked earlier this
    /// session is named, since the chooser opens on it and Enter twice repeats it.
    fn busy_send_hint(&self) -> String {
        match self.busy_send {
            BusySend::Choose => match self.last_draft_choice {
                Some(last) => format!("choose route (last: {})", last.short()),
                None => "choose send action".to_owned(),
            },
            BusySend::Redirect => "redirect task".to_owned(),
            BusySend::AfterTurn => "send after turn".to_owned(),
            BusySend::Independent => "run independently".to_owned(),
            BusySend::Auto => "route automatically".to_owned(),
        }
    }

    /// While busy, Enter uses the explicitly saved preference (a chooser by default).
    /// Alt-Enter queues a follow-up directly. Ending the turn closes any open
    /// choice and leaves the draft as it was.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        self.exit_armed = false;
        self.stop_armed = false;
        if !busy {
            self.choice = None;
        }
        // Anything the agent never took before the turn ended is not lost: it is sent next, after
        // the messages already queued ahead of it.
        let mut leftover = Vec::new();
        if !busy && !self.recovery_held {
            for item in self.explicit.drain(..) {
                // Pending or received but never journaled (the turn ended or was stopped first):
                // keep it as a follow-up instead of losing it. Applied ones are already recorded.
                if item.stage != SteerStage::Applied {
                    leftover.push(item.text);
                }
            }
        }
        if !leftover.is_empty() {
            for message in leftover.into_iter().rev() {
                self.queued.push_front((message, true));
            }
            self.queue_notice = Some(
                "The turn ended before your steering was applied; it is queued to send next."
                    .into(),
            );
        }
        self.refresh();
    }

    /// Shows the push-to-talk recording indicator in the bottom border. `hold` words it as
    /// "release to stop" (key-release reporting) rather than "press to stop".
    pub fn set_recording(&mut self, recording: bool, hold: bool) {
        let next = recording.then_some(hold);
        if self.recording != next {
            self.recording = next;
            self.refresh();
        }
    }

    pub fn mode(&self) -> InputMode {
        if self.editor.text.starts_with('!') {
            InputMode::Shell
        } else {
            InputMode::Normal
        }
    }

    pub fn menu_open(&self) -> bool {
        self.menu.as_ref().is_some_and(|m| !m.matches.is_empty())
    }

    /// Whether the busy chooser is open, so the app can leave Escape to the composer while it is.
    pub fn choice_open(&self) -> bool {
        self.choice.is_some()
    }

    /// Pins whether the terminal reports enhanced keys (tests and terminals detected elsewhere);
    /// `None` reads the live terminal state.
    pub fn set_keyboard_enhanced(&mut self, enhanced: Option<bool>) {
        self.keyboard_enhanced = enhanced;
    }

    fn keyboard_enhanced(&self) -> bool {
        self.keyboard_enhanced
            .unwrap_or_else(super::term::reports_key_releases)
    }

    /// Whether a local input layer should get the dismiss key before an enclosing wait cancels.
    pub fn dismisses_interrupt(&self, key: &KeyEvent, keys: &Keymap) -> bool {
        keys.matches(Action::Dismiss, key)
            && (self.search.is_some()
                || self.choice.is_some()
                || self.editing_queue.is_some()
                || self.menu_open()
                || self.ghost().is_some())
    }

    pub fn handle_key(&mut self, key: KeyEvent, ctx: &ComposerCtx) -> ComposerEvent {
        if key.kind == KeyEventKind::Release {
            return ComposerEvent::None;
        }
        // Held submit keys must never confirm missing or truncated attachments.
        if key.kind == KeyEventKind::Repeat
            && (ctx.keys.matches(Action::Submit, &key) || key.code == KeyCode::Enter)
        {
            return ComposerEvent::None;
        }
        // A held Enter repeats: while busy, a repeat must not open, confirm or flush a choice by itself.
        if self.busy && key.kind == KeyEventKind::Repeat && ctx.keys.matches(Action::Submit, &key) {
            return ComposerEvent::None;
        }
        self.stop_armed = false;
        // Stopping is global even while a completion, history search or chooser is open.
        // A user-mapped dismiss chord still belongs to the local layer first.
        if self.busy
            && ctx.keys.matches(Action::Interrupt, &key)
            && !self.dismisses_interrupt(&key, ctx.keys)
        {
            self.editor.cancel_jump();
            // With a draft, the shared Ctrl+C clears it first (like idle); the next press stops.
            if self.search.is_none()
                && !self.editor.text.is_empty()
                && ctx.keys.matches(Action::ClearDraft, &key)
            {
                if self.history_index.is_some() {
                    self.leave_history();
                }
                if !self.editor.text.is_empty() {
                    self.set_draft("");
                }
                self.stop_armed = true;
                self.refresh();
                return ComposerEvent::None;
            }
            return ComposerEvent::Interrupt;
        }
        // A pending Ctrl-] jump takes the next typed letter; any other key cancels it before dispatch.
        let plain = matches!(key.code, KeyCode::Char(_))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        if self.search.is_none() && self.editor.jump_pending() && plain {
            self.editor.key_with(key, ctx.keys);
            self.refresh();
            return ComposerEvent::None;
        }
        self.editor.cancel_jump();
        let (length, cursor) = (self.editor.text.len(), self.editor.cursor);
        let event = if self.search.is_some() {
            self.search_key(key, ctx)
        } else {
            self.edit_key(key, ctx)
        };
        self.mention_edit =
            self.editor.text.len() != length || (self.mention_edit && self.editor.cursor == cursor);
        self.refresh();
        event
    }

    fn edit_key(&mut self, key: KeyEvent, ctx: &ComposerCtx) -> ComposerEvent {
        let keys = ctx.keys;
        let rows = list_rows(ctx.ui);
        let armed = std::mem::take(&mut self.exit_armed);
        let eof_armed = std::mem::take(&mut self.eof_armed);
        if let Some(event) = self.choice_key(key, ctx) {
            return event;
        }

        if self.editing_queue.is_some()
            && key.code == KeyCode::Esc
            && !self.menu_open()
            && self.ghost().is_none()
        {
            self.clear();
            return ComposerEvent::None;
        }
        if self.step_queue(&key, keys) {
            return ComposerEvent::None;
        }
        if keys.triggers(Action::FollowUp, &key) {
            if !self.busy {
                self.editor.insert("\n");
                return ComposerEvent::None;
            }
            return if is_slash_command(&self.editor.text) {
                self.submit()
            } else {
                self.submit_queued(true)
            };
        }
        if keys.triggers(Action::SubagentsHub, &key) {
            return ComposerEvent::Action(Action::SubagentsHub);
        }
        // Left twice on an empty draft is the gesture form of the hub key.
        if key.code == KeyCode::Left
            && key.modifiers.is_empty()
            && self.editor.text.is_empty()
            && self.double_left.press(std::time::Instant::now())
        {
            return ComposerEvent::Action(Action::SubagentsHub);
        }
        // An open menu owns Up/Down, so a remapped app key that shares them (Ctrl-P) moves the selection.
        if self.menu_open()
            && (keys.matches(Action::SelectUp, &key) || keys.matches(Action::SelectDown, &key))
        {
            let up = keys.matches(Action::SelectUp, &key);
            self.step_menu(if up { -1 } else { 1 }, true, rows);
            return ComposerEvent::None;
        }
        if let Some(action) = APP_ACTIONS.into_iter().find(|a| keys.triggers(*a, &key)) {
            return ComposerEvent::Action(action);
        }
        if keys.matches(Action::HistorySearch, &key) {
            // Search every remembered prompt, as `ui.history_limit` promises (0 = unlimited).
            // Newest first, each prompt once: a repeated `/cost` keeps only its latest place.
            let mut seen = std::collections::HashSet::new();
            let entries = self
                .history
                .iter()
                .rev()
                .filter(|entry| seen.insert(entry.trim().to_owned()))
                .cloned()
                .collect();
            self.search = Some(Search {
                query: Editor::default(),
                entries,
                selected: 0,
                scroll: 0,
            });
            return ComposerEvent::None;
        }
        if keys.matches(Action::Dismiss, &key) && (self.menu_open() || self.ghost().is_some()) {
            self.dismissed = Some(self.editor.text.clone());
            return ComposerEvent::None;
        }
        if self.busy && keys.matches(Action::Interrupt, &key) {
            return ComposerEvent::Interrupt;
        }
        if keys.matches(Action::ClearDraft, &key) {
            if self.history_index.is_some() {
                // Ctrl-C while browsing returns to the unsent draft instead of discarding it.
                self.leave_history();
                return ComposerEvent::None;
            }
            if !self.editor.text.is_empty() {
                self.set_draft("");
                return ComposerEvent::None;
            }
            if armed {
                return ComposerEvent::Exit;
            }
            self.exit_armed = true;
            return ComposerEvent::None;
        }
        if keys.matches(Action::Exit, &key) {
            if self.editor.text.is_empty() {
                // Like Ctrl+C, exiting takes a second press so a stray key cannot end the session.
                if self.busy {
                    return ComposerEvent::None;
                }
                if eof_armed {
                    return ComposerEvent::Exit;
                }
                self.eof_armed = true;
                return ComposerEvent::None;
            }
            // Like readline: Ctrl-D in a draft deletes the character under the cursor.
            if !self.delete_chip(true) {
                self.editor.apply(Action::EditorDeleteForward);
            }
            return ComposerEvent::None;
        }
        if keys.matches(Action::Newline, &key) {
            self.editor.insert("\n");
            return ComposerEvent::None;
        }
        if keys.matches(Action::Submit, &key) {
            return self.submit();
        }
        if keys.matches(Action::Complete, &key) && (self.complete_menu() || self.accept_ghost()) {
            return ComposerEvent::None;
        }

        let up = keys.matches(Action::SelectUp, &key);
        if up || keys.matches(Action::SelectDown, &key) {
            if self.menu_open() {
                self.step_menu(if up { -1 } else { 1 }, true, rows);
            } else if !self.editor.vertical(up) {
                self.browse_history(up);
            }
            return ComposerEvent::None;
        }
        let page_up = keys.matches(Action::SelectPageUp, &key);
        if (page_up || keys.matches(Action::SelectPageDown, &key)) && self.menu_open() {
            let step = rows as isize;
            self.step_menu(if page_up { -step } else { step }, false, rows);
            return ComposerEvent::None;
        }
        let backward = keys.matches(Action::EditorDeleteBackward, &key)
            || keys.matches(Action::EditorDeleteWordBackward, &key);
        let forward = keys.matches(Action::EditorDeleteForward, &key)
            || keys.matches(Action::EditorDeleteWordForward, &key);
        if (backward || forward) && self.delete_chip(forward) {
            return ComposerEvent::None;
        }
        self.editor.key_with(key, keys);
        ComposerEvent::None
    }

    /// Sends a finished speech draft without accepting autocomplete. During a turn it uses the
    /// existing after-turn queue rather than interrupting work or opening the busy chooser.
    pub fn submit_voice(&mut self) -> ComposerEvent {
        // An empty draft can still be an in-progress queue edit. Leave it for review rather than
        // consuming the original queued message or changing its route.
        if self.editing_queue.is_some() {
            return ComposerEvent::None;
        }
        self.menu = None;
        self.choice = None;
        if self.busy {
            self.submit_queued(true)
        } else {
            self.submit()
        }
    }

    fn submit(&mut self) -> ComposerEvent {
        // File mentions complete the draft unless the typed path already names the highlighted
        // file exactly. Commands only accept a suggestion after navigation; otherwise Enter
        // executes exactly the command the user typed.
        let file_menu = self
            .menu
            .as_ref()
            .is_some_and(|menu| menu.file_start.is_some());
        let typed_exactly = self.menu.as_ref().is_some_and(|menu| {
            !menu.navigated
                && menu.file_start.is_some()
                && menu.matches.get(menu.selected).is_some_and(|matched| {
                    self.menu_entries(menu)[matched.index].name == menu.query
                })
        });
        let accept = self
            .menu
            .as_ref()
            .is_some_and(|menu| (file_menu && !typed_exactly) || menu.navigated);
        if accept && self.complete_menu() {
            if file_menu {
                return ComposerEvent::None;
            }
            // Command completion adds a space for further typing, not execution.
            // `set` needs `&mut` on the editor, so the trimmed text must be an owned copy.
            let trimmed = self.editor.text.trim_end().to_owned();
            self.editor.set(&trimmed);
        }
        // A second Enter explicitly sends the oldest queued message now. Do not
        // add it to history again: queuing already recorded the original draft.
        // While busy that stops the turn, so an empty Enter asks first (the default keeps working).
        // Editing a queued item never asks: the draft is saved on the route it was queued with.
        if self.editor.text.is_empty() && !self.queued.is_empty() {
            if !self.busy {
                return self.send_oldest_queued();
            }
            if self.editing_queue.is_none() {
                self.open_choice(QUEUE_CHOICES);
            }
            return ComposerEvent::None;
        }
        if self.busy && !is_slash_command(&self.editor.text) {
            if self.editing_queue.is_some() {
                return self.submit_queued(false);
            }
            // The chooser is the safe default. Direct sending requires an explicitly saved preference.
            if !self.editor.text.trim().is_empty() {
                return match self.busy_send {
                    BusySend::Choose => {
                        self.open_choice(DRAFT_CHOICES);
                        ComposerEvent::None
                    }
                    BusySend::Redirect => self.send_explicit(SteerKind::Steer),
                    BusySend::AfterTurn => self.submit_queued(true),
                    BusySend::Independent => self.send_explicit(SteerKind::Independent),
                    BusySend::Auto => self.submit_queued(false),
                };
            }
            return ComposerEvent::None;
        }
        if self.editor.text.trim().is_empty() {
            return ComposerEvent::None;
        }
        if !self.attachment_send_allowed() {
            return ComposerEvent::None;
        }
        if !self.recovery_before_take(recovery::Take::Draft) {
            return ComposerEvent::None;
        }
        let sent = expand_pastes(&self.editor.text, &self.pastes);
        self.learn_words(&sent);
        self.finish_draft();
        // Sending an edited queue item consumes it; the original is intentionally not restored.
        self.editing_queue = None;
        self.push_history(sent.clone());
        ComposerEvent::Submit(sent)
    }

    /// A successful send ends this draft's recovery lifetime. History keeps the expanded message;
    /// a marker typed in a later draft must not resurrect a previously submitted payload.
    fn finish_draft(&mut self) {
        self.editing_queue = None;
        self.editor = Editor::default();
        self.pastes.clear();
        self.clear();
    }

    fn refresh(&mut self) {
        self.prune_pastes();
        if self
            .dismissed
            .as_deref()
            .is_some_and(|d| d != self.editor.text)
        {
            self.dismissed = None;
        }
        self.poll_autocomplete();
        match self.menu_query() {
            None => {
                self.supersede_rankings();
                self.menu = None;
            }
            Some((query, start))
                if self
                    .menu
                    .as_ref()
                    .is_some_and(|m| m.query == query && m.file_start == start) => {}
            Some((query, file_start)) => {
                let query = query.to_owned();
                let epoch = self.supersede_rankings();
                let arguments = if file_start.is_none() {
                    self.argument_entries(&query)
                } else {
                    None
                };
                // Small candidate sets rank inline so the menu is current on the same keystroke.
                // Large file lists rank on a worker and stay pending until their result is drained.
                let background = arguments.is_none()
                    && file_start.is_some()
                    && self.files.len() > SYNC_RANK_LIMIT;
                let (matches, pending) = if background {
                    let files = std::sync::Arc::clone(&self.files);
                    let tx = self.autocomplete_tx.clone();
                    let generation = self.candidates_generation;
                    let worker_query = query.clone();
                    let latest = std::sync::Arc::clone(&self.rank_epoch);
                    std::thread::spawn(move || {
                        let superseded =
                            || latest.load(std::sync::atomic::Ordering::Relaxed) != epoch;
                        if let Some(matches) = rank_until(&files, &worker_query, &superseded) {
                            let _ = tx.send((generation, worker_query, file_start, matches));
                        }
                    });
                    (Vec::new(), true)
                } else {
                    let entries: &[CommandEntry] = if let Some(entries) = &arguments {
                        entries
                    } else if file_start.is_some() {
                        &self.files
                    } else {
                        &self.commands
                    };
                    let mut matches = rank(entries, &query);
                    if query.is_empty() && file_start.is_none() && arguments.is_none() {
                        matches.retain(|matched| {
                            entries[matched.index].kind != CommandKind::Builtin
                                || super::command_surface::everyday(&entries[matched.index].name)
                        });
                    }
                    (matches, false)
                };
                self.menu = Some(Menu {
                    query,
                    file_start,
                    arguments,
                    matches,
                    selected: 0,
                    navigated: false,
                    scroll: 0,
                    pending,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
