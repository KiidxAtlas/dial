//! The composer: the input box with the status line in its top border and key hints in its bottom
//! border, a fuzzy `/` command menu and an incremental history search below it, history browsing,
//! inline ghost suggestions and bracketed-paste collapsing.
//!
//! The component is pure: [`Composer::handle_key`] maps a key to a [`ComposerEvent`], and
//! [`Composer::view`] returns styled rows plus the cursor position. Every app-level shortcut comes
//! from the [`Keymap`], every color and glyph from the [`Theme`], so hints and behavior always agree.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::llm::model_config::{ComposerShape, UiConfig};

use super::word_complete::{WordIndex, WordModel, build_vocabulary, completion_from_project};
use super::{
    Rows,
    editor::Editor,
    keys::{Action, Keymap},
    status::{InputMode, StatusData},
    statusline, text,
    theme::{Sym, Theme, Tok},
};

/// Pastes longer than this many lines or characters collapse to a placeholder.
const PASTE_COLLAPSE_LINES: usize = 20;
const PASTE_COLLAPSE_CHARS: usize = 2000;
/// Draft rows shown at once; the window follows the cursor.
const MAX_INPUT_ROWS: usize = 10;
/// Under this many cells the composer degrades to a bare input row.
const MIN_BOX_WIDTH: usize = 8;
const MAX_NAME_COLUMN: usize = 24;
/// App-level shortcuts the composer reports instead of handling.
const APP_ACTIONS: [Action; 17] = [
    Action::CommandPalette,
    Action::ModelSelect,
    Action::ModelRoles,
    Action::ModelFavoriteNext,
    Action::ProviderSetup,
    Action::Settings,
    Action::ToolsExpand,
    Action::ToolsToggle,
    Action::ThinkingToggle,
    Action::ApprovalCycle,
    Action::DisplayReset,
    Action::EditExternal,
    Action::PasteImage,
    Action::VoiceInput,
    Action::SubagentsRecent,
    Action::RateGood,
    Action::RateBad,
];

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
    /// Steer the running turn with the draft; it is taken at the next steering point.
    Steer,
    /// Send the draft after the running turn finishes.
    AfterTurn,
    /// Leave the queue alone and let the turn finish.
    KeepWorking,
    /// Stop the running turn and send the oldest queued message.
    StopAndSend,
}

impl BusyChoice {
    fn label(self) -> &'static str {
        match self {
            BusyChoice::Steer => "Add direction now",
            BusyChoice::AfterTurn => "Queue for after this turn",
            BusyChoice::KeepWorking => "Keep working",
            BusyChoice::StopAndSend => "Stop and send oldest",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            BusyChoice::Steer => "applies at the next step of this turn",
            BusyChoice::AfterTurn => "sent when this turn finishes",
            BusyChoice::KeepWorking => "queued messages stay queued",
            BusyChoice::StopAndSend => "stops this turn and sends the oldest queued message",
        }
    }
}

/// Offered for a non-empty draft while busy. Steering is first, so a plain Enter steers.
const DRAFT_CHOICES: &[BusyChoice] = &[BusyChoice::Steer, BusyChoice::AfterTurn];
/// Offered for an empty Enter with queued work. Stopping the turn is never the default.
const QUEUE_CHOICES: &[BusyChoice] = &[BusyChoice::KeepWorking, BusyChoice::StopAndSend];

/// An open busy chooser: its options and the highlighted one. It holds no draft state, so closing it
/// never touches the draft, pastes or images.
struct Choice {
    options: &'static [BusyChoice],
    selected: usize,
}

/// Returns history entries matching a prefix, up to a limit.
type HistoryProvider = std::sync::Arc<dyn Fn(&str, usize) -> Vec<String> + Send + Sync>;

pub struct Composer {
    editor: Editor,
    double_left: DoubleTap,
    history: Vec<String>,
    history_limit: usize,
    history_index: Option<usize>,
    saved_draft: Option<Editor>,
    commands: Vec<CommandEntry>,
    /// Push-to-talk is capturing: `Some(hold)` while recording (`hold` means release ends it).
    recording: Option<bool>,
    argument_completions: Vec<String>,
    /// Shared with the background ranker, so large file lists are not copied per keystroke.
    files: std::sync::Arc<Vec<CommandEntry>>,
    autocomplete_rx: std::sync::mpsc::Receiver<AutocompleteResult>,
    autocomplete_tx: std::sync::mpsc::Sender<AutocompleteResult>,
    /// Bumped whenever the candidate lists change; background results from older generations are dropped.
    candidates_generation: u64,
    history_provider: Option<HistoryProvider>,
    menu: Option<Menu>,
    /// The exact draft the user dismissed the menu/ghost on; editing the text ends the dismissal.
    dismissed: Option<String>,
    search: Option<Search>,
    busy: bool,
    queued: std::collections::VecDeque<(String, bool)>,
    /// The queued message currently loaded into the draft. Cancelling or replacing the draft restores it
    /// unchanged at its original position; only submitting replaces it.
    editing_queue: Option<QueuedEdit>,
    /// The busy chooser open above the box, if any.
    choice: Option<Choice>,
    queue_notice: Option<String>,
    exit_armed: bool,
    /// `(placeholder, full text)` for collapsed pastes still referenced by the draft.
    pastes: Vec<(String, String)>,
    paste_count: usize,
    /// Keep pasted-image files alive for the session because message expansion happens after submit.
    pasted_images: Vec<tempfile::TempPath>,
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
}

impl Composer {
    pub fn new(history: Vec<String>, history_limit: usize) -> Self {
        let (autocomplete_tx, autocomplete_rx) = std::sync::mpsc::channel();
        let (vocab_tx, vocab_rx) = std::sync::mpsc::channel();
        let mut composer = Composer {
            editor: Editor::default(),
            history: Vec::new(),
            history_limit,
            history_index: None,
            saved_draft: None,
            commands: Vec::new(),
            menu: None,
            dismissed: None,
            search: None,
            busy: false,
            recording: None,
            exit_armed: false,
            pastes: Vec::new(),
            paste_count: 0,
            pasted_images: Vec::new(),
            queued: std::collections::VecDeque::new(),
            editing_queue: None,
            choice: None,
            queue_notice: None,
            files: std::sync::Arc::new(Vec::new()),
            argument_completions: Vec::new(),
            history_provider: None,
            double_left: DoubleTap::default(),
            autocomplete_rx,
            autocomplete_tx,
            candidates_generation: 0,
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
        };
        for entry in history {
            composer.push_history(entry);
        }
        composer
    }

    pub fn set_history_provider(&mut self, provider: Option<HistoryProvider>) {
        self.history_provider = provider;
    }

    /// Attach an image from the platform clipboard and insert its quoted path in the draft.
    pub fn paste_clipboard_image(&mut self) -> anyhow::Result<()> {
        use std::io::Write;
        let bytes = clipboard_image_bytes()?;
        anyhow::ensure!(!bytes.is_empty(), "clipboard contains no image");
        let mut file = tempfile::Builder::new()
            .prefix("dial-paste-")
            .suffix(".png")
            .tempfile()?;
        file.write_all(&bytes)?;
        let owner = file.into_temp_path();
        let path = owner.to_string_lossy().into_owned();
        self.pasted_images.push(owner);
        self.handle_paste(&format!("\"{path}\""));
        Ok(())
    }

    pub fn draft(&self) -> String {
        self.editor.text.clone()
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
        self.dismissed = None;
        self.pastes.clear();
        self.choice = None;
        // Replacing the draft abandons an in-progress queue edit: put the original back, never drop it.
        self.restore_queued_edit();
        self.queue_notice = None;
        self.refresh();
    }

    pub fn clear(&mut self) {
        self.set_draft("");
    }

    pub fn history(&self) -> Vec<String> {
        self.history.clone()
    }

    pub fn push_history(&mut self, entry: String) {
        let entry = text::sanitize(&entry);
        if entry.trim().is_empty() || self.history.last() == Some(&entry) {
            return;
        }
        self.history.push(entry);
        self.trim_history();
    }

    /// Applies a changed `ui.history_limit` to the live history, dropping the oldest entries first.
    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_limit = limit;
        self.trim_history();
    }

    fn trim_history(&mut self) {
        if self.history_limit != 0 {
            let excess = self.history.len().saturating_sub(self.history_limit);
            self.history.drain(..excess);
        }
    }

    /// Commands come from the controller so the menu matches real dispatch. Names that could not be
    /// typed as a command (empty, whitespace, slashes, controls) and duplicates are dropped.
    pub fn set_commands(&mut self, commands: Vec<CommandEntry>) {
        self.candidates_generation += 1;
        let selection = self.menu_selection();
        let mut seen = std::collections::HashSet::new();
        self.commands = commands
            .into_iter()
            .filter_map(|c| {
                let bad = c.name.is_empty()
                    || c.name
                        .chars()
                        .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '/');
                if bad || !seen.insert(c.name.clone()) {
                    return None;
                }
                Some(CommandEntry {
                    description: text::one_line(&c.description),
                    args_hint: c
                        .args_hint
                        .map(|a| text::one_line(&a))
                        .filter(|a| !a.trim().is_empty()),
                    ..c
                })
            })
            .collect();
        self.menu = None;
        self.refresh();
        self.restore_selection(selection);
    }

    fn menu_selection(&self) -> Option<(String, usize, usize)> {
        let menu = self.menu.as_ref()?;
        let hit = menu.matches.get(menu.selected)?;
        Some((
            self.menu_entries(menu).get(hit.index)?.name.clone(),
            menu.selected,
            menu.scroll,
        ))
    }

    fn restore_selection(&mut self, selection: Option<(String, usize, usize)>) {
        let Some((name, previous, scroll)) = selection else {
            return;
        };
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let selected = menu
            .matches
            .iter()
            .position(|hit| self.menu_entries(menu)[hit.index].name == name)
            .unwrap_or(previous.min(menu.matches.len().saturating_sub(1)));
        if let Some(menu) = &mut self.menu {
            menu.selected = selected;
            menu.scroll = scroll.min(selected);
        }
    }

    /// Valid argument continuations supplied by the controller, in preference order. Unlike history,
    /// these are available before a command has ever been run. Never insert display placeholders.
    pub fn set_argument_completions(&mut self, candidates: Vec<String>) {
        self.candidates_generation += 1;
        let mut seen = std::collections::HashSet::new();
        self.argument_completions = candidates
            .into_iter()
            .filter(|candidate| {
                candidate.starts_with('/')
                    && candidate.contains(' ')
                    && !candidate.chars().any(char::is_control)
                    && seen.insert(candidate.clone())
            })
            .collect();
        self.menu = None;
        self.refresh();
    }

    /// While busy, Enter asks whether to steer or queue (an empty Enter asks before stopping),
    /// Alt-Enter queues a follow-up directly, and Esc interrupts. Ending the turn closes any open
    /// choice and leaves the draft as it was.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        self.exit_armed = false;
        if !busy {
            self.choice = None;
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

    pub fn set_files(&mut self, paths: Vec<String>) {
        let selection = self.menu_selection();
        self.candidates_generation += 1;
        self.scan_vocabulary(&paths);
        self.files = std::sync::Arc::new(
            paths
                .into_iter()
                .map(|name| CommandEntry {
                    name,
                    description: "project file".into(),
                    kind: CommandKind::File,
                    args_hint: None,
                })
                .collect(),
        );
        self.menu = None;
        self.refresh();
        self.restore_selection(selection);
    }

    fn menu_entries<'a>(&'a self, menu: &'a Menu) -> &'a [CommandEntry] {
        if let Some(arguments) = &menu.arguments {
            arguments
        } else if menu.file_start.is_some() {
            &self.files
        } else {
            &self.commands
        }
    }

    pub fn take_steering(&mut self) -> Vec<String> {
        let mut steering = Vec::new();
        for _ in 0..self.queued.len() {
            if let Some((text, follow_up)) = self.queued.pop_front() {
                if follow_up {
                    self.queued.push_back((text, true));
                } else {
                    steering.push(text);
                }
            }
        }
        steering
    }

    pub fn take_follow_up(&mut self) -> Option<String> {
        self.queued.pop_front().map(|(text, _)| text)
    }
    pub fn has_queued_work(&self) -> bool {
        !self.queued.is_empty()
    }

    pub fn queued_messages(&self) -> Vec<(String, bool)> {
        self.queued.iter().cloned().collect()
    }
    pub fn remove_queued(&mut self, index: usize) -> bool {
        let removed = self.queued.remove(index).is_some();
        if removed {
            self.queue_notice = None;
        }
        removed
    }
    pub fn edit_queued(&mut self, index: usize) -> bool {
        if !self.editor.text.is_empty() {
            return false;
        }
        self.restore_queued_edit();
        let Some((text, follow_up)) = self.queued.remove(index) else {
            return false;
        };
        self.begin_queued_edit(index, text, follow_up);
        true
    }

    /// Load a queued item (already removed from the queue) into the draft, remembering where it came from.
    fn begin_queued_edit(&mut self, index: usize, text: String, follow_up: bool) {
        self.set_draft(&text);
        self.editing_queue = Some(QueuedEdit {
            index,
            text,
            follow_up,
        });
    }

    fn restore_queued_edit(&mut self) {
        if let Some(edit) = self.editing_queue.take() {
            let index = edit.index.min(self.queued.len());
            self.queued.insert(index, (edit.text, edit.follow_up));
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

    // -----------------------------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------------------------

    /// Bracketed paste never submits. Long pastes collapse to `[Pasted text #n +X lines]` and are
    /// expanded when the draft is submitted.
    pub fn handle_paste(&mut self, pasted: &str) {
        let clean = text::sanitize(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
        self.exit_armed = false;
        self.choice = None;
        if let Some(search) = &mut self.search {
            search.query.insert(&clean.replace('\n', " "));
            search.selected = 0;
            search.scroll = 0;
            return;
        }
        let lines = clean.trim_end_matches('\n').split('\n').count();
        if lines > PASTE_COLLAPSE_LINES || clean.chars().count() > PASTE_COLLAPSE_CHARS {
            self.paste_count += 1;
            let placeholder = format!("[Pasted text #{} +{lines} lines]", self.paste_count);
            self.editor.insert(&placeholder);
            self.pastes.push((placeholder, clean));
        } else {
            self.editor.insert(&clean);
        }
        self.refresh();
    }

    pub fn handle_key(&mut self, key: KeyEvent, ctx: &ComposerCtx) -> ComposerEvent {
        if key.kind == KeyEventKind::Release {
            return ComposerEvent::None;
        }
        // A held Enter repeats: while busy, a repeat must not open, confirm or flush a choice by itself.
        if self.busy && key.kind == KeyEventKind::Repeat && ctx.keys.matches(Action::Submit, &key) {
            return ComposerEvent::None;
        }
        let event = if self.search.is_some() {
            self.search_key(key, ctx)
        } else {
            self.edit_key(key, ctx)
        };
        self.refresh();
        event
    }

    fn edit_key(&mut self, key: KeyEvent, ctx: &ComposerCtx) -> ComposerEvent {
        let keys = ctx.keys;
        let rows = list_rows(ctx.ui);
        let armed = std::mem::take(&mut self.exit_armed);
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
        if self.editor.text.is_empty() && key.code == KeyCode::Up {
            self.restore_queued_edit();
            if let Some((text, follow_up)) = self.queued.pop_back() {
                let index = self.queued.len();
                self.begin_queued_edit(index, text, follow_up);
                return ComposerEvent::None;
            }
        }
        if keys.matches(Action::FollowUp, &key) {
            if !self.busy {
                self.editor.insert("\n");
                return ComposerEvent::None;
            }
            return if self.editor.text.trim_start().starts_with('/') {
                self.submit()
            } else {
                self.submit_queued(true)
            };
        }
        if keys.matches(Action::SubagentsHub, &key) {
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
        if let Some(action) = APP_ACTIONS.into_iter().find(|a| keys.matches(*a, &key)) {
            return ComposerEvent::Action(action);
        }
        if keys.matches(Action::HistorySearch, &key) {
            let entries = self
                .history_provider
                .as_ref()
                .map(|provider| provider("", 500))
                .unwrap_or_else(|| self.history.iter().rev().take(500).cloned().collect());
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
                return if self.busy {
                    ComposerEvent::None
                } else {
                    ComposerEvent::Exit
                };
            }
            // Like readline: Ctrl-D in a draft deletes the character under the cursor.
            self.editor
                .key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
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
        if matches!(key.code, KeyCode::Left | KeyCode::Right)
            && key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
        {
            self.move_word(key.code == KeyCode::Right);
            return ComposerEvent::None;
        }
        self.editor.key(key);
        ComposerEvent::None
    }

    fn submit(&mut self) -> ComposerEvent {
        // Enter submits the typed command; suggestions are accepted with Complete.
        // File mentions keep their existing Enter-to-complete behavior.
        if self
            .menu
            .as_ref()
            .is_some_and(|menu| menu.file_start.is_some() && !menu.matches.is_empty())
        {
            self.complete_menu();
            return ComposerEvent::None;
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
        if self.busy && !self.editor.text.trim_start().starts_with('/') {
            if self.editing_queue.is_some() {
                return self.submit_queued(false);
            }
            // Enter while busy asks whether to steer or queue; nothing is sent until the choice is made.
            if !self.editor.text.trim().is_empty() {
                self.open_choice(DRAFT_CHOICES);
            }
            return ComposerEvent::None;
        }
        if self.editor.text.trim().is_empty() {
            return ComposerEvent::None;
        }
        let draft = std::mem::take(&mut self.editor.text);
        self.editor.cursor = 0;
        let sent = expand_pastes(&draft, &self.pastes);
        self.learn_words(&sent);
        self.pastes.clear();
        self.history_index = None;
        self.saved_draft = None;
        // Sending an edited queue item consumes it; the original is intentionally not restored.
        self.editing_queue = None;
        self.push_history(sent.clone());
        ComposerEvent::Submit(sent)
    }

    fn submit_queued(&mut self, follow_up: bool) -> ComposerEvent {
        if self.editor.text.trim().is_empty() {
            return ComposerEvent::None;
        }
        let sent = expand_pastes(&self.editor.text, &self.pastes);
        let bytes: usize = self.queued.iter().map(|(text, _)| text.len()).sum();
        if self.queued.len() >= 32
            || sent.len() > 256 * 1024
            || bytes.saturating_add(sent.len()) > 1024 * 1024
        {
            self.queue_notice = Some(
                "Queue limit reached. Draft kept; use /queue to edit or remove messages.".into(),
            );
            return ComposerEvent::None;
        }
        self.learn_words(&sent);
        self.push_history(sent.clone());
        // A saved edit replaces the original in place, so editing never reorders the queue.
        let edit = self.editing_queue.take();
        // A saved edit keeps the route it was queued with; the caller's choice only applies to new messages.
        let follow_up = edit.as_ref().map_or(follow_up, |edit| edit.follow_up);
        let index = edit.map_or(self.queued.len(), |edit| edit.index.min(self.queued.len()));
        self.queued.insert(index, (sent, follow_up));
        self.clear();
        self.queue_notice = None;
        ComposerEvent::None
    }

    /// Opens the busy chooser with its safe default highlighted. The draft is left as it is.
    fn open_choice(&mut self, options: &'static [BusyChoice]) {
        self.choice = Some(Choice {
            options,
            selected: 0,
        });
        self.exit_armed = false;
    }

    /// Keys while the busy chooser is open. `Some` is the event for a key it consumed. `None` closes
    /// the chooser and lets the key reach ordinary editing, so typed or pasted text is never dropped.
    fn choice_key(&mut self, key: KeyEvent, ctx: &ComposerCtx) -> Option<ComposerEvent> {
        let keys = ctx.keys;
        let (options, selected) = {
            let choice = self.choice.as_ref()?;
            (choice.options, choice.selected)
        };
        if keys.matches(Action::Dismiss, &key) {
            self.choice = None;
            return Some(ComposerEvent::None);
        }
        if keys.matches(Action::SelectUp, &key) || keys.matches(Action::SelectDown, &key) {
            let next = if keys.matches(Action::SelectUp, &key) {
                selected.saturating_sub(1)
            } else {
                (selected + 1).min(options.len() - 1)
            };
            if let Some(choice) = self.choice.as_mut() {
                choice.selected = next;
            }
            return Some(ComposerEvent::None);
        }
        if keys.matches(Action::Submit, &key) {
            self.choice = None;
            let event = match options[selected] {
                BusyChoice::Steer => self.submit_queued(false),
                BusyChoice::AfterTurn => self.submit_queued(true),
                BusyChoice::KeepWorking => ComposerEvent::None,
                BusyChoice::StopAndSend => self.send_oldest_queued(),
            };
            return Some(event);
        }
        if self.busy && keys.matches(Action::Interrupt, &key) {
            self.choice = None;
            return Some(ComposerEvent::Interrupt);
        }
        self.choice = None;
        None
    }

    /// Sends the oldest queued message now. Only an explicit stop does this; queuing already recorded it.
    fn send_oldest_queued(&mut self) -> ComposerEvent {
        let Some((text, _)) = self.queued.pop_front() else {
            return ComposerEvent::None;
        };
        self.queue_notice = None;
        ComposerEvent::Submit(text)
    }

    fn complete_menu(&mut self) -> bool {
        let Some(menu) = self.menu.as_ref().filter(|m| !m.matches.is_empty()) else {
            return false;
        };
        let entry = &self.menu_entries(menu)[menu.matches[menu.selected].index];
        let arguments = menu.arguments.is_some();
        let completed = if let Some(start) = menu.file_start {
            format!("{}@{} ", &self.editor.text[..start], entry.name)
        } else if menu.arguments.is_some() {
            format!("/{}", entry.name)
        } else {
            format!("/{} ", entry.name)
        };
        self.editor.set(&completed);
        self.dismissed = arguments.then_some(completed);
        true
    }

    /// The word being typed at the end of the draft, completed from learned prompts first and then
    /// from this project's identifiers and file names.
    fn word_ghost(&self, draft: &str) -> Option<&str> {
        if !self.word_completion {
            return None;
        }
        let start = draft
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let word = &draft[start..];
        self.words
            .complete(word)
            .or_else(|| completion_from_project(&self.project_words, word))
            .filter(|rest| !rest.is_empty())
    }

    /// Learns the words of a submitted prompt and saves the model. Nothing is learned while word
    /// completion is off, so turning it off also stops the file from growing.
    fn learn_words(&mut self, prompt: &str) {
        if !self.word_completion || !self.words.learn_prompt(prompt) {
            return;
        }
        let Some(path) = self.word_store.as_deref() else {
            return;
        };
        match self.words.to_bytes() {
            Ok(bytes) => {
                if let Err(error) = crate::config::settings_store::write_private(path, &bytes) {
                    self.word_notice = Some(format!("Could not save learned words: {error:#}"));
                }
            }
            Err(error) => {
                self.word_notice = Some(format!("Could not save learned words: {error:#}"));
            }
        }
    }

    /// Rebuilds the project vocabulary on a worker thread for `paths`, superseding any scan that
    /// is still running. Does nothing until a project root is set.
    fn scan_vocabulary(&mut self, paths: &[String]) {
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.vocab_generation += 1;
        let generation = self.vocab_generation;
        self.vocab_cancel
            .store(generation, std::sync::atomic::Ordering::Relaxed);
        let cancel = self.vocab_cancel.clone();
        let tx = self.vocab_tx.clone();
        let files = paths.to_vec();
        std::thread::spawn(move || {
            let index = build_vocabulary(&root, &files, &cancel, generation);
            let _ = tx.send((generation, index));
        });
    }

    /// Turns inline word completion on or off, from `ui.word_completion`.
    pub fn set_word_completion(&mut self, enabled: bool) {
        self.word_completion = enabled;
    }

    /// Sets the project root whose identifiers and file names feed word completion.
    pub fn set_project_root(&mut self, root: std::path::PathBuf) {
        self.project_root = Some(root);
    }

    /// Loads the learned-words file at `store` and saves to it from now on. A missing file is an
    /// empty model; an unreadable or invalid one leaves an empty model and a notice.
    pub fn load_learned_words(&mut self, store: std::path::PathBuf) {
        match crate::platform::atomic::read_private(&store) {
            Ok(Some(bytes)) => match WordModel::from_bytes(&bytes) {
                Ok(model) => self.words = model,
                Err(error) => {
                    self.word_notice = Some(format!("Learned words were reset: {error:#}"));
                }
            },
            Ok(None) => {}
            Err(error) => {
                self.word_notice = Some(format!("Could not read learned words: {error:#}"));
            }
        }
        self.word_store = Some(store);
    }

    /// Takes the pending word-completion load or save problem, if any.
    pub fn take_word_notice(&mut self) -> Option<String> {
        self.word_notice.take()
    }

    fn accept_ghost(&mut self) -> bool {
        let Some(suffix) = self.ghost() else {
            return false;
        };
        let completed = format!("{}{suffix}", self.editor.text);
        self.editor.set(&completed);
        self.dismissed = Some(completed);
        true
    }

    fn step_menu(&mut self, delta: isize, wrap: bool, rows: usize) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let total = menu.matches.len() as isize;
        if total == 0 {
            return;
        }
        let next = menu.selected as isize + delta;
        menu.selected = if wrap {
            next.rem_euclid(total)
        } else {
            next.clamp(0, total - 1)
        } as usize;
        menu.scroll = window(menu.selected, menu.scroll, rows, menu.matches.len());
    }

    fn move_word(&mut self, forward: bool) {
        let text = &self.editor.text;
        let marks: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
        let blank = |g: &str| g.chars().all(char::is_whitespace);
        let mut at = marks
            .iter()
            .position(|(i, _)| *i >= self.editor.cursor)
            .unwrap_or(marks.len());
        if forward {
            while at < marks.len() && blank(marks[at].1) {
                at += 1;
            }
            while at < marks.len() && !blank(marks[at].1) {
                at += 1;
            }
        } else {
            while at > 0 && blank(marks[at - 1].1) {
                at -= 1;
            }
            while at > 0 && !blank(marks[at - 1].1) {
                at -= 1;
            }
        }
        let cursor = marks.get(at).map_or(text.len(), |(i, _)| *i);
        self.editor.cursor = cursor;
    }

    /// Up/Down past the first/last line browse history. The unsent draft (and its cursor) is kept and
    /// comes back when browsing past the newest entry; edits to a browsed entry are not saved.
    fn browse_history(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        if up {
            let index = match self.history_index {
                Some(index) => index.saturating_sub(1),
                None => {
                    self.saved_draft = Some(std::mem::take(&mut self.editor));
                    self.history.len() - 1
                }
            };
            self.history_index = Some(index);
            self.editor.set(&self.history[index]);
        } else if let Some(index) = self.history_index {
            if index + 1 < self.history.len() {
                self.history_index = Some(index + 1);
                self.editor.set(&self.history[index + 1]);
            } else {
                self.history_index = None;
                self.editor = self.saved_draft.take().unwrap_or_default();
            }
        }
    }

    fn search_matches(&self) -> Vec<usize> {
        let Some(search) = self.search.as_ref() else {
            return Vec::new();
        };
        let query = search.query.text.to_lowercase();
        search
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    fn search_key(&mut self, key: KeyEvent, ctx: &ComposerCtx) -> ComposerEvent {
        let keys = ctx.keys;
        let rows = list_rows(ctx.ui);
        let matches = self.search_matches();
        let Some(search) = self.search.as_mut() else {
            return ComposerEvent::None;
        };
        let total = matches.len();
        if keys.matches(Action::Dismiss, &key) || keys.matches(Action::ClearDraft, &key) {
            self.search = None;
            return ComposerEvent::None;
        }
        let accept = keys.matches(Action::Submit, &key)
            || (keys.matches(Action::Complete, &key) && key.code != KeyCode::Right);
        if accept {
            let entry = matches
                .get(search.selected)
                .map(|&i| search.entries[i].clone());
            self.search = None;
            if let Some(entry) = entry {
                self.set_draft(&entry);
            }
            return ComposerEvent::None;
        }
        let step = |search: &mut Search, delta: isize| {
            if total > 0 {
                search.selected =
                    (search.selected as isize + delta).clamp(0, total as isize - 1) as usize;
            }
            search.scroll = window(search.selected, search.scroll, rows, total);
        };
        if keys.matches(Action::SelectUp, &key) {
            step(search, -1);
        } else if keys.matches(Action::SelectDown, &key)
            || keys.matches(Action::HistorySearch, &key)
        {
            step(search, 1);
        } else if keys.matches(Action::SelectPageUp, &key) {
            step(search, -(rows as isize));
        } else if keys.matches(Action::SelectPageDown, &key) {
            step(search, rows as isize);
        } else if search.query.key(key) {
            search.selected = 0;
            search.scroll = 0;
        }
        ComposerEvent::None
    }

    // -----------------------------------------------------------------------------------------
    // Derived state
    // -----------------------------------------------------------------------------------------

    /// Slash commands or the trailing `@file` word at the cursor.
    fn menu_query(&self) -> Option<(&str, Option<usize>)> {
        if self.search.is_some() {
            return None;
        }
        let text = self.editor.text.as_str();
        if self.editor.cursor != text.len() || self.dismissed.as_deref() == Some(text) {
            return None;
        }
        let start = text
            .rfind(char::is_whitespace)
            .map_or(0, |i| i + text[i..].chars().next().unwrap().len_utf8());
        if let Some(query) = text[start..].strip_prefix('@') {
            return (!self.files.is_empty()).then_some((query, Some(start)));
        }
        if self.commands.is_empty() {
            return None;
        }
        let rest = text.strip_prefix('/')?;
        if rest.chars().any(char::is_control) {
            return None;
        }
        (!rest.chars().any(char::is_whitespace) || self.argument_entries(rest).is_some())
            .then_some((rest, None))
    }

    /// Offer only the next word at the current command depth. This prevents nested flags or skill
    /// names from overwhelming the initial subcommand list.
    fn argument_entries(&self, query: &str) -> Option<Vec<CommandEntry>> {
        let (prefix, _) = query.rsplit_once(' ')?;
        let root = query.split(' ').next()?;
        let command = self.commands.iter().find(|entry| entry.name == root)?;
        let stem = format!("/{prefix} ");
        let mut seen = std::collections::HashSet::new();
        let entries: Vec<_> = self
            .argument_completions
            .iter()
            .filter_map(|candidate| {
                let next = candidate.strip_prefix(&stem)?.split(' ').next()?;
                if next.is_empty() || !seen.insert(next.to_owned()) {
                    return None;
                }
                Some(CommandEntry {
                    name: format!("{prefix} {next}"),
                    description: String::new(),
                    kind: command.kind,
                    args_hint: None,
                })
            })
            .collect();
        (!entries.is_empty()).then_some(entries)
    }

    /// Applies finished background rankings. A result is kept only while the menu still shows the
    /// query it was computed for, so stale results never reach the screen. Returns `true` when the
    /// open menu changed and the caller should repaint.
    pub fn poll_autocomplete(&mut self) -> bool {
        let mut changed = false;
        while let Ok((generation, query, file_start, matches)) = self.autocomplete_rx.try_recv() {
            if generation == self.candidates_generation
                && self.menu_query() == Some((query.as_str(), file_start))
                && let Some(menu) = self.menu.as_mut()
                && menu.query == query
                && menu.file_start == file_start
            {
                menu.matches = matches;
                menu.pending = false;
                menu.selected = menu.selected.min(menu.matches.len().saturating_sub(1));
                changed = true;
            }
        }
        while let Ok((generation, index)) = self.vocab_rx.try_recv() {
            if generation == self.vocab_generation {
                self.project_words = index;
                changed = true;
            }
        }
        changed
    }

    /// Whether a background ranking is outstanding, so the event loop must wake to collect it.
    pub fn autocomplete_pending(&self) -> bool {
        self.menu.as_ref().is_some_and(|menu| menu.pending)
    }

    fn refresh(&mut self) {
        if self
            .dismissed
            .as_deref()
            .is_some_and(|d| d != self.editor.text)
        {
            self.dismissed = None;
        }
        self.poll_autocomplete();
        match self.menu_query() {
            None => self.menu = None,
            Some((query, start))
                if self
                    .menu
                    .as_ref()
                    .is_some_and(|m| m.query == query && m.file_start == start) => {}
            Some((query, file_start)) => {
                let query = query.to_owned();
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
                    std::thread::spawn(move || {
                        let matches = rank(&files, &worker_query);
                        let _ = tx.send((generation, worker_query, file_start, matches));
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
                    scroll: 0,
                    pending,
                });
            }
        }
    }

    /// Dim inline continuation from command metadata, then newest history. Only at the end of an
    /// idle, non-empty draft, and never over half a grapheme. Acceptance never submits the draft.
    fn ghost(&self) -> Option<&str> {
        let draft = self.editor.text.as_str();
        if self.busy
            || self.search.is_some()
            || self.history_index.is_some()
            || (self.menu_open()
                && self
                    .menu
                    .as_ref()
                    .is_some_and(|menu| menu.arguments.is_none()))
            || draft.is_empty()
            || self.editor.cursor != draft.len()
            || self.dismissed.as_deref() == Some(draft)
        {
            return None;
        }
        if let Some(menu) = self.menu.as_ref().filter(|_| self.menu_open()) {
            let entry = &self.menu_entries(menu)[menu.matches.get(menu.selected)?.index];
            return entry
                .name
                .strip_prefix(&draft[1..])
                .filter(|suffix| !suffix.is_empty());
        }
        let extends = |entry: &str| {
            entry.starts_with(draft)
                && entry.len() > draft.len()
                && entry
                    .grapheme_indices(true)
                    .any(|(offset, _)| offset == draft.len())
        };
        // Names still use the command menu; argument ghosts start after its first space.
        if draft.starts_with('/')
            && draft.contains(' ')
            && let Some(entry) = self
                .argument_completions
                .iter()
                .find(|entry| extends(entry))
        {
            return Some(&entry[draft.len()..]);
        }
        let history = self
            .history
            .iter()
            .rev()
            .find(|entry| extends(entry))
            .map(|entry| &entry[draft.len()..]);
        history.or_else(|| self.word_ghost(draft))
    }

    // -----------------------------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------------------------

    /// The composer box plus the menu or history-search rows below it, and the cursor within them.
    pub fn view(&self, width: u16, ctx: &ComposerCtx) -> ComposerView {
        let w = width as usize;
        let theme = ctx.theme;
        let ghost = self.ghost().unwrap_or("");
        if w < MIN_BOX_WIDTH {
            let mut view = self.bare_view(w, ghost, theme);
            if let Some(choice) = &self.choice
                && w > 0
            {
                let rows = self.choice_rows(choice, w, ctx);
                prepend_rows(&mut view, rows);
            }
            if let Some(notice) = &self.queue_notice {
                view.rows.insert(
                    0,
                    Line::from(Span::styled(
                        text::ellipsize(notice, w, theme.ascii()),
                        theme.fg(Tok::Warning),
                    )),
                );
                view.cursor.0 += 1;
            }
            return view;
        }

        let banded = ctx.ui.composer_shape == ComposerShape::Band;
        let boxed = ctx.ui.composer_shape == ComposerShape::Box;
        let inner = if boxed {
            w - 4
        } else if banded {
            w - BAND_LEAD - 1
        } else {
            w
        };
        let laid = layout(
            &self.editor.text,
            ghost,
            self.editor.cursor,
            inner,
            self.mode() == InputMode::Shell,
        );
        let start = laid
            .cursor
            .0
            .saturating_sub(MAX_INPUT_ROWS - 1)
            .min(laid.rows.len().saturating_sub(MAX_INPUT_ROWS));
        let border = self.border_style(ctx);
        let (bar, side, top_l, top_r, bottom_l, bottom_r) = if boxed {
            (
                theme.sym(Sym::BoxH),
                theme.sym(Sym::BoxV),
                theme.sym(Sym::BoxTl),
                theme.sym(Sym::BoxTr),
                theme.sym(Sym::BoxBl),
                theme.sym(Sym::BoxBr),
            )
        } else {
            let bar = theme.sym(Sym::BoxH);
            (bar, theme.sym(Sym::BoxV), bar, bar, bar, bar)
        };

        let activity = statusline::activity_row(ctx.ui, ctx.status, theme, width);
        let mut status_ui;
        let header_ui = if activity.is_some() {
            status_ui = ctx.ui.clone();
            let (left, right) = statusline::segments(ctx.ui);
            let metadata = |names: Vec<String>| {
                names
                    .into_iter()
                    .filter(|name| !name.trim().eq_ignore_ascii_case("spinner"))
                    .collect()
            };
            status_ui.status = crate::llm::model_config::StatusPreset::Custom;
            status_ui.status_left = metadata(left);
            status_ui.status_right = metadata(right);
            &status_ui
        } else {
            ctx.ui
        };
        let activity_rows = usize::from(activity.is_some());
        let mut rows: Rows = activity.into_iter().collect();
        let tint = theme.bg(Tok::UserBg);
        let band = |spans: Vec<Span<'static>>| band_row(spans, w, tint);
        if banded {
            rows.push(band(Vec::new()));
        } else {
            rows.push(statusline::border_line_with(
                header_ui, ctx.status, theme, w as u16, border, bar, top_l, top_r,
            ));
        }
        let prompt_style = if self.mode() == InputMode::Shell || self.busy {
            border
        } else {
            theme.fg(Tok::Accent)
        }
        .add_modifier(Modifier::BOLD);
        for (i, row) in laid
            .rows
            .iter()
            .skip(start)
            .take(MAX_INPUT_ROWS)
            .enumerate()
        {
            let mut spans = Vec::new();
            if boxed {
                spans.push(Span::styled(format!("{side} "), border));
            } else if banded {
                let mark = if i == 0 && start == 0 {
                    theme.sym(Sym::Prompt)
                } else {
                    ""
                };
                let lead = format!(" {mark}");
                let pad = BAND_LEAD.saturating_sub(text::width(&lead));
                spans.push(Span::styled(
                    format!("{lead}{}", " ".repeat(pad)),
                    prompt_style,
                ));
            }
            let placeholder = self.editor.text.is_empty();
            let used = if placeholder {
                let label = if self.editing_queue.is_some() {
                    "Edit the queued message"
                } else if self.busy {
                    "Add direction while Dial works"
                } else {
                    "Ask anything, or describe what to build"
                };
                let label = text::clip(label, inner);
                let used = text::width(&label);
                spans.push(Span::styled(label, theme.fg(Tok::Muted)));
                used
            } else {
                let used: usize = row.iter().map(|(t, _)| text::width(t)).sum();
                spans.extend(
                    row.iter()
                        .map(|(t, kind)| Span::styled(t.clone(), kind.style(theme))),
                );
                used
            };
            if boxed {
                spans.push(Span::raw(" ".repeat(inner - used + 1)));
                spans.push(Span::styled(side.to_owned(), border));
            }
            rows.push(if banded {
                band(spans)
            } else {
                Line::from(spans)
            });
        }
        let cursor = (
            (activity_rows + 1 + laid.cursor.0 - start) as u16,
            (laid.cursor.1
                + if boxed {
                    2
                } else if banded {
                    BAND_LEAD
                } else {
                    0
                }) as u16,
        );
        let mut hints = self.hint_items(ctx);
        if let Some(hold) = self.recording {
            let key = ctx.keys.label(Action::VoiceInput);
            let what = if hold {
                format!("recording, release {key} to stop")
            } else {
                format!("recording, {key} to stop")
            };
            hints.insert(0, Hint { key: None, what });
        }
        if banded {
            // The status line closes the band, aligned with the input; the hints sit under it.
            rows.push(band(Vec::new()));
            rows.push(band(
                statusline::border_line_with(
                    header_ui,
                    ctx.status,
                    theme,
                    w as u16,
                    theme.fg(Tok::Dim),
                    " ",
                    " ",
                    " ",
                )
                .spans,
            ));
            rows.push(bottom_border(
                w,
                &hints,
                " ",
                " ",
                " ",
                Style::default(),
                theme,
            ));
        } else {
            rows.push(bottom_border(
                w, &hints, bottom_l, bottom_r, bar, border, theme,
            ));
        }

        let mut cursor = cursor;
        if let Some(search) = &self.search {
            let (list, col) = self.search_rows(search, w, ctx);
            cursor = (rows.len() as u16, col as u16);
            rows.extend(list);
        } else if let Some(menu) = &self.menu {
            // The menu opens upward, into the space above the input, so the input never moves
            // down and the closest rows to the cursor are the selected command's details.
            let mut menu_rows = self.menu_rows(menu, w, ctx);
            cursor.0 += menu_rows.len() as u16;
            menu_rows.append(&mut rows);
            rows = menu_rows;
        }
        if let Some(choice) = &self.choice {
            let mut choice_rows = self.choice_rows(choice, w, ctx);
            cursor.0 += choice_rows.len() as u16;
            choice_rows.append(&mut rows);
            rows = choice_rows;
        }
        if let Some(notice) = &self.queue_notice {
            rows.insert(
                0,
                Line::from(Span::styled(
                    text::ellipsize(notice, w, theme.ascii()),
                    theme.fg(Tok::Warning),
                )),
            );
            cursor.0 += 1;
        }
        if !self.queued.is_empty() {
            let mut queued = Vec::new();
            for (message, follow_up) in self.queued.iter().take(3) {
                queued.push(Line::from(Span::styled(
                    text::ellipsize(
                        &format!(
                            "queued {}: {}",
                            if *follow_up { "follow-up" } else { "steering" },
                            text::one_line(message)
                        ),
                        w,
                        theme.ascii(),
                    ),
                    theme.fg(Tok::Accent),
                )));
            }
            let hidden = self.queued.len().saturating_sub(3);
            if hidden > 0 {
                queued.push(Line::from(Span::styled(
                    text::ellipsize(&format!("+{hidden} more queued"), w, theme.ascii()),
                    theme.fg(Tok::Dim),
                )));
            }
            queued.push(Line::from(Span::styled(
                text::ellipsize(
                    &format!(
                        "{} queued {} /queue manage {} {} {} {} {} edit last",
                        self.queued.len(),
                        theme.sym(Sym::Dot),
                        theme.sym(Sym::Dot),
                        ctx.keys.label(Action::Submit),
                        if self.busy {
                            "choose stop or keep"
                        } else {
                            "send oldest"
                        },
                        theme.sym(Sym::Dot),
                        ctx.keys.label(Action::SelectUp)
                    ),
                    w,
                    theme.ascii(),
                ),
                theme.fg(Tok::Dim),
            )));
            cursor.0 += queued.len() as u16;
            queued.extend(rows);
            rows = queued;
        }
        ComposerView { rows, cursor }
    }

    /// Fewer than `MIN_BOX_WIDTH` cells: just the input row around the cursor, no chrome.
    fn bare_view(&self, w: usize, ghost: &str, theme: &Theme) -> ComposerView {
        if w == 0 {
            return ComposerView {
                rows: vec![Line::default()],
                cursor: (0, 0),
            };
        }
        let laid = layout(&self.editor.text, ghost, self.editor.cursor, w, false);
        let row = laid
            .rows
            .get(laid.cursor.0)
            .map(|r| r.as_slice())
            .unwrap_or_default();
        let spans = row
            .iter()
            .map(|(t, kind)| Span::styled(t.clone(), kind.style(theme)))
            .collect::<Vec<_>>();
        ComposerView {
            rows: vec![Line::from(spans)],
            cursor: (0, laid.cursor.1.min(w - 1) as u16),
        }
    }

    /// The busy chooser: a one-line question and its options above the box, with the selected option
    /// highlighted. Each option's detail is cut to the width rather than wrapped.
    fn choice_rows(&self, choice: &Choice, w: usize, ctx: &ComposerCtx) -> Rows {
        let theme = ctx.theme;
        let question = if choice.options.contains(&BusyChoice::StopAndSend) {
            let queued = self.queued.len();
            format!(
                "Dial is still working; {queued} queued {}. Stop now?",
                if queued == 1 { "message" } else { "messages" }
            )
        } else {
            "Dial is still working. Choose how this message is sent:".to_owned()
        };
        let mut rows = vec![finish_row(
            vec![Span::styled(question, theme.fg(Tok::Muted))],
            w,
        )];
        let total = choice.options.len();
        let pointer = theme.sym(Sym::Pointer);
        let blank_pointer = " ".repeat(text::width(pointer));
        let accent = theme.fg(Tok::Accent);
        rows.extend(render_list(
            theme,
            w,
            total,
            total,
            0,
            choice.selected,
            |i, budget, selected| {
                let option = choice.options[i];
                let prefix = text::width(pointer) + 1;
                let label =
                    text::ellipsize(option.label(), budget.saturating_sub(prefix), theme.ascii());
                let used = prefix + text::width(&label);
                let mut spans = vec![
                    Span::styled(
                        if selected {
                            pointer
                        } else {
                            blank_pointer.as_str()
                        }
                        .to_owned(),
                        accent,
                    ),
                    Span::raw(" "),
                    Span::styled(label, theme.fg(Tok::Text)),
                ];
                let room = budget.saturating_sub(used + 2);
                if room > 0 {
                    let detail = text::ellipsize(option.detail(), room, theme.ascii());
                    spans.push(Span::styled(format!("  {detail}"), theme.fg(Tok::Muted)));
                }
                spans
            },
        ));
        rows
    }

    /// Border color by state: shell mode, busy, the optional per-session accent, else the plain border.
    fn border_style(&self, ctx: &ComposerCtx) -> Style {
        let theme = ctx.theme;
        let token = if self.mode() == InputMode::Shell {
            Tok::ShellMode
        } else if self.busy {
            Tok::Accent
        } else if ctx.ui.session_accent && !ctx.status.session.is_empty() {
            accent_for(&ctx.status.session)
        } else {
            Tok::Border
        };
        theme.fg(token)
    }

    /// Priority-ordered, indivisible hints; disabled or shadowed bindings are not advertised.
    fn hint_items(&self, ctx: &ComposerCtx) -> Vec<Hint> {
        if !ctx.ui.hints {
            return Vec::new();
        }
        let keys = ctx.keys;
        let label = |action: Action| {
            keys.chords(action)
                .iter()
                .find(|chord| {
                    let key = KeyEvent::new(chord.code, chord.mods);
                    // State-specific actions must not advertise a shadowed shortcut.
                    !(self.search.is_none()
                        && ((self.editing_queue.is_some() && key.code == KeyCode::Esc)
                            || (self.busy
                                && action != Action::FollowUp
                                && keys.matches(Action::FollowUp, &key))
                            || (action == Action::Interrupt
                                && keys.matches(Action::Dismiss, &key)
                                && (self.menu_open() || self.ghost().is_some()))))
                })
                .map(|chord| chord.display())
                .unwrap_or_default()
        };
        let hint = |action: Action, what: &str| {
            let key = label(action);
            (!key.is_empty()).then(|| Hint {
                key: Some(key),
                what: what.to_owned(),
            })
        };
        let select = || {
            let labels: Vec<_> = [Action::SelectUp, Action::SelectDown]
                .into_iter()
                .map(label)
                .filter(|s| !s.is_empty())
                .collect();
            (!labels.is_empty()).then(|| Hint {
                key: Some(labels.join("/")),
                what: "select".to_owned(),
            })
        };
        let keyed = |key: &str, what: &str| {
            Some(Hint {
                key: Some(key.to_owned()),
                what: what.to_owned(),
            })
        };
        let plain = |what: String| Some(Hint { key: None, what });
        let mut pieces = Vec::new();
        if self.choice.is_some() {
            pieces.extend([
                hint(Action::Submit, "confirm"),
                hint(Action::Dismiss, "cancel"),
                select(),
            ]);
        } else if self.search.is_some() {
            pieces.extend([
                hint(Action::Submit, "accept"),
                select(),
                hint(Action::Dismiss, "cancel"),
            ]);
        } else {
            if self.busy {
                pieces.push(hint(Action::Interrupt, "interrupt"));
            }
            if let Some(QueuedEdit { follow_up, .. }) = self.editing_queue {
                pieces.push(plain(format!(
                    "editing queued {}",
                    if follow_up { "follow-up" } else { "steering" }
                )));
                pieces.push(hint(
                    Action::Submit,
                    if self.busy {
                        "save queued item"
                    } else {
                        "send"
                    },
                ));
                pieces.push(keyed("Esc", "cancel edit"));
            } else if self.exit_armed {
                pieces.push(hint(Action::ClearDraft, "again to exit"));
            } else if self.menu_open() {
                pieces.extend([
                    hint(Action::Complete, "complete"),
                    select(),
                    hint(Action::Dismiss, "dismiss"),
                ]);
            } else if self.busy {
                let command = self.editor.text.trim_start().starts_with('/');
                let blank = self.editor.text.trim().is_empty();
                // Mirrors `submit`: an empty Enter asks to stop (only with queued work); a blank draft
                // is ignored; otherwise Enter asks whether to steer or queue the draft.
                if command {
                    pieces.push(hint(Action::Submit, "run command"));
                } else if self.editor.text.is_empty() {
                    if !self.queued.is_empty() {
                        pieces.push(hint(Action::Submit, "stop or keep"));
                    }
                } else if !blank {
                    pieces.push(hint(Action::Submit, "steer or queue"));
                }
                // Alt-Enter only queues a non-blank draft.
                if !command && !blank {
                    pieces.push(hint(Action::FollowUp, "follow-up"));
                }
            } else if !self.editor.text.is_empty() {
                pieces.extend([
                    hint(
                        Action::Submit,
                        if self.mode() == InputMode::Shell {
                            "run"
                        } else {
                            "send"
                        },
                    ),
                    hint(Action::Newline, "newline"),
                ]);
                // The dim inline ghost is the cue for a suggestion; advertising it here made the
                // bar flicker as the suggestion appeared and vanished with each keystroke.
                if self.mode() == InputMode::Shell {
                    pieces.push(plain("shell mode".to_owned()));
                }
            } else {
                pieces.extend([
                    hint(Action::Submit, "send"),
                    keyed("/", "commands"),
                    hint(Action::CommandPalette, "actions"),
                    hint(Action::ModelSelect, "model"),
                    hint(Action::Settings, "settings"),
                    hint(Action::HistorySearch, "history"),
                ]);
            }
        }
        pieces.into_iter().flatten().collect()
    }

    /// The hint texts (`Enter send`, `/ commands`), for assertions.
    #[cfg(test)]
    fn hint(&self, ctx: &ComposerCtx) -> Vec<String> {
        self.hint_items(ctx).iter().map(Hint::text).collect()
    }

    fn menu_rows(&self, menu: &Menu, w: usize, ctx: &ComposerCtx) -> Rows {
        let theme = ctx.theme;
        // On the band layout the list sits two cells inside an inset panel.
        let panel = ctx.ui.composer_shape == ComposerShape::Band && w >= 24;
        let outer = w;
        let w = if panel { w - 4 } else { w };
        let total = menu.matches.len();
        if total == 0 {
            let message = if menu.file_start.is_some() {
                "  No matching files"
            } else if menu.arguments.is_some() {
                "  No matching subcommands or options"
            } else {
                "  No matching commands"
            };
            return vec![finish_row(
                vec![Span::styled(message, theme.fg(Tok::Dim))],
                w,
            )];
        }
        let rows = list_rows(ctx.ui).min(total);
        let start = window(menu.selected, menu.scroll, rows, total);
        let visible = &menu.matches[start..start + rows];
        let entries = self.menu_entries(menu);
        let name_col = visible
            .iter()
            .map(|m| text::width(&entries[m.index].name))
            .max()
            .unwrap_or(0)
            .min(if menu.file_start.is_some() || menu.arguments.is_some() {
                w.saturating_sub(6)
            } else {
                MAX_NAME_COLUMN
            });
        let pointer = theme.sym(Sym::Pointer);
        let blank_pointer = " ".repeat(text::width(pointer));
        let accent = theme.fg(Tok::Accent);
        let hit = accent.add_modifier(Modifier::BOLD);
        let groups = menu.file_start.is_none() && menu.arguments.is_none() && menu.query.is_empty();
        let mut rows = render_list(
            theme,
            w,
            total,
            rows,
            start,
            menu.selected,
            |i, budget, selected| {
                let found = &menu.matches[i];
                let command = &entries[found.index];
                let icon = theme.sym(match command.kind {
                    CommandKind::Builtin => Sym::IconCommand,
                    CommandKind::Custom => Sym::IconSettings,
                    CommandKind::Skill => Sym::IconSkill,
                    CommandKind::File => Sym::IconFile,
                });
                let mut spans = vec![
                    Span::styled(
                        if selected {
                            pointer
                        } else {
                            blank_pointer.as_str()
                        }
                        .to_owned(),
                        accent,
                    ),
                    Span::raw(" "),
                ];
                if !icon.is_empty() {
                    spans.push(Span::styled(format!("{icon} "), accent));
                }
                let prefix: usize = spans.iter().map(|s| text::width(&s.content)).sum();
                let name = text::ellipsize(&command.name, name_col, theme.ascii());
                let name_cells = text::width(&name);
                spans.extend(highlighted(
                    &name,
                    &found.name_hits,
                    theme.fg(Tok::Text),
                    hit,
                ));
                spans.push(Span::raw(
                    " ".repeat(name_col.saturating_sub(name_cells) + 2),
                ));
                let used = prefix + name_col + 2;
                // Unfiltered, the list is grouped; each group's first row names it at the right.
                let group = if groups
                    && (i == start
                        || group_of(&entries[menu.matches[i - 1].index]) != group_of(command))
                {
                    group_of(command)
                } else {
                    ""
                };
                let group_w = if group.is_empty() {
                    0
                } else {
                    text::width(group) + 2
                };
                let room = budget.saturating_sub(used + group_w);
                if room > 0 {
                    let description = text::ellipsize(&command.description, room, theme.ascii());
                    let description_w = text::width(&description);
                    spans.extend(highlighted(
                        &description,
                        &found.desc_hits,
                        theme.fg(Tok::Muted),
                        hit,
                    ));
                    if !group.is_empty() {
                        spans.push(Span::raw(" ".repeat(room - description_w + 2)));
                        spans.push(Span::styled(group.to_uppercase(), theme.fg(Tok::Dim)));
                    }
                }
                spans
            },
        );
        if menu.file_start.is_none()
            && menu.arguments.is_none()
            && let Some(found) = menu.matches.get(menu.selected)
        {
            rows.extend(Self::command_detail_rows(&entries[found.index], w, theme));
        }
        if panel {
            // On the band layout the menu is an inset panel: its kind and position as a title.
            let title = if menu.file_start.is_some() {
                "Files"
            } else if menu.arguments.is_some() {
                "Options"
            } else {
                "Commands"
            };
            let title = format!("{title} ({}/{total})", menu.selected + 1);
            let hint = if theme.ascii() {
                "tab complete"
            } else {
                "↑↓ select · tab complete"
            };
            rows = super::chrome::framed_lines(
                theme,
                super::chrome::Section::Dial,
                &title,
                hint,
                rows,
                outer,
            );
        }
        rows
    }

    /// Usage and the full description of the selected command, drawn under the menu. Both rows are
    /// ellipsized to the width, so the detail never wraps into the composer.
    fn command_detail_rows(command: &CommandEntry, w: usize, theme: &Theme) -> Rows {
        let usage = match command.args_hint.as_deref() {
            Some(hint) if !hint.is_empty() => format!("/{} {hint}", command.name),
            _ => format!("/{}", command.name),
        };
        let room = w.saturating_sub(2);
        vec![
            finish_row(
                vec![Span::styled(
                    format!(" {}", text::ellipsize(&usage, room, theme.ascii())),
                    theme.fg(Tok::Accent),
                )],
                w,
            ),
            finish_row(
                vec![Span::styled(
                    format!(
                        " {}",
                        text::ellipsize(&command.description, room, theme.ascii())
                    ),
                    theme.fg(Tok::Muted),
                )],
                w,
            ),
        ]
    }

    /// The search prompt row, then the matching history entries (newest first). Returns the cursor
    /// column inside the prompt row, which is the first returned row.
    fn search_rows(&self, search: &Search, w: usize, ctx: &ComposerCtx) -> (Rows, usize) {
        let theme = ctx.theme;
        let label = "search";
        let prompt = theme.sym(Sym::Prompt);
        let head = vec![
            Span::styled(format!("{label} "), theme.fg(Tok::Muted)),
            Span::styled(format!("{prompt} "), theme.fg(Tok::Accent)),
            Span::raw(search.query.text.clone()),
        ];
        let col = text::width(label)
            + 1
            + text::width(prompt)
            + 1
            + text::width(&search.query.text[..search.query.cursor]);
        let mut rows = vec![finish_row(head, w)];

        let matches = self.search_matches();
        if matches.is_empty() {
            rows.push(finish_row(
                vec![Span::styled("  No matching history", theme.fg(Tok::Dim))],
                w,
            ));
            return (rows, col.min(w - 1));
        }
        let total = matches.len();
        let shown = list_rows(ctx.ui).min(total);
        let start = window(search.selected, search.scroll, shown, total);
        let pointer = theme.sym(Sym::Pointer);
        let blank_pointer = " ".repeat(text::width(pointer));
        let accent = theme.fg(Tok::Accent);
        let hit = accent.add_modifier(Modifier::BOLD);
        let query = search.query.text.to_lowercase();
        rows.extend(render_list(
            theme,
            w,
            total,
            shown,
            start,
            search.selected,
            |i, budget, selected| {
                let entry = text::one_line(&search.entries[matches[i]]);
                let entry = text::ellipsize(&entry, budget.saturating_sub(2), theme.ascii());
                let hits = substring_hits(&entry, &query);
                let mut spans = vec![
                    Span::styled(
                        if selected {
                            pointer
                        } else {
                            blank_pointer.as_str()
                        }
                        .to_owned(),
                        accent,
                    ),
                    Span::raw(" "),
                ];
                spans.extend(highlighted(&entry, &hits, theme.fg(Tok::Text), hit));
                spans
            },
        ));
        (rows, col.min(w - 1))
    }
}
fn clipboard_image_bytes() -> anyhow::Result<Vec<u8>> {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pngpaste", &["-"])];
    #[cfg(target_os = "linux")]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-paste", &["--no-newline", "--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let candidates: &[(&str, &[&str])] = &[];

    let mut attempted = Vec::new();
    for (program, args) in candidates {
        let output = match Command::new(program).args(*args).output() {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                attempted.push(*program);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if output.status.success() && !output.stdout.is_empty() {
            anyhow::ensure!(
                output.stdout.len() as u64 <= crate::llm::images::MAX_IMAGE_BYTES,
                "clipboard image exceeds the supported 5 MiB limit"
            );
            anyhow::ensure!(
                image::guess_format(&output.stdout).is_ok(),
                "clipboard data is not a supported image"
            );
            return Ok(output.stdout);
        }
        attempted.push(*program);
    }
    anyhow::bail!("no clipboard image found (tried {})", attempted.join(", "))
}

// ---------------------------------------------------------------------------------------------
// Layout and rendering helpers
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Ghost,
    Marker,
}

impl Kind {
    fn style(self, theme: &Theme) -> Style {
        match self {
            Kind::Text => theme.fg(Tok::Text),
            Kind::Ghost => theme.fg(Tok::Dim).add_modifier(Modifier::DIM),
            Kind::Marker => theme.fg(Tok::ShellMode),
        }
    }
}

struct Layout {
    rows: Vec<Vec<(String, Kind)>>,
    /// `(row, column)` of the text cursor; always inside `width` cells.
    cursor: (usize, usize),
}

/// Hard-wrap `text` followed by the dim `ghost` at `width` cells, tracking where the cursor (a byte
/// offset into `text`) lands. A wide grapheme that does not fit moves to the next row; one wider than
/// the row becomes `?`. A cursor that would sit past the last cell of a full row wraps to a new row.
fn layout(text: &str, ghost: &str, cursor: usize, width: usize, shell: bool) -> Layout {
    let width = width.max(1);
    let split = text.len();
    let graphemes = text
        .grapheme_indices(true)
        .map(|(i, g)| (i, g, false))
        .chain(
            ghost
                .grapheme_indices(true)
                .map(|(i, g)| (split + i, g, true)),
        );
    let mut rows: Vec<Vec<(String, Kind)>> = vec![Vec::new()];
    let mut col = 0;
    let mut at: Option<(usize, usize)> = None;
    for (offset, grapheme, is_ghost) in graphemes {
        let here = offset == cursor;
        if grapheme == "\n" {
            if here && col >= width {
                rows.push(Vec::new());
                col = 0;
            }
            if here {
                at = Some((rows.len() - 1, col));
            }
            rows.push(Vec::new());
            col = 0;
            continue;
        }
        let shown = if text::width(grapheme) > width {
            "?"
        } else {
            grapheme
        };
        let size = text::width(shown);
        if col + size > width {
            rows.push(Vec::new());
            col = 0;
        }
        if here {
            at = Some((rows.len() - 1, col));
        }
        let kind = if is_ghost {
            Kind::Ghost
        } else if shell && offset == 0 && shown == "!" {
            Kind::Marker
        } else {
            Kind::Text
        };
        let row = rows.last_mut().expect("rows is never empty");
        match row.last_mut() {
            Some((run, k)) if *k == kind => run.push_str(shown),
            _ => row.push((shown.to_owned(), kind)),
        }
        col += size;
    }
    let cursor = at.unwrap_or_else(|| {
        if col >= width {
            rows.push(Vec::new());
            col = 0;
        }
        (rows.len() - 1, col)
    });
    Layout { rows, cursor }
}

/// One bottom-border hint: an optional key (drawn amber bold) and its label (muted).
struct Hint {
    key: Option<String>,
    what: String,
}

impl Hint {
    fn text(&self) -> String {
        match &self.key {
            Some(key) => format!("{key} {}", self.what),
            None => self.what.clone(),
        }
    }
}

/// `╰─ hint ──────╯`, exactly `w` cells; lower-priority hint items are dropped whole.
fn bottom_border(
    w: usize,
    hints: &[Hint],
    left: &str,
    right: &str,
    bar: &str,
    border: Style,
    theme: &Theme,
) -> Line<'static> {
    let frame = text::width(left) + text::width(right) + 2 * text::width(bar);
    let room = w.saturating_sub(frame + 2);
    let sep = format!(" {} ", theme.sym(Sym::Dot));
    let joined = |items: &[Hint]| items.iter().map(Hint::text).collect::<Vec<_>>().join(&sep);
    let mut keep = hints.len();
    while keep > 0 && text::width(&joined(&hints[..keep])) > room {
        keep -= 1;
    }
    let kept = &hints[..keep];
    if kept.is_empty() {
        let corners = text::width(left) + text::width(right);
        return Line::from(Span::styled(
            format!("{left}{}{right}", bar.repeat(w.saturating_sub(corners))),
            border,
        ));
    }
    let fill = room - text::width(&joined(kept));
    let mut spans = vec![Span::styled(format!("{left}{bar} "), border)];
    for (i, hint) in kept.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(sep.clone(), theme.fg(Tok::Muted)));
        }
        match &hint.key {
            Some(key) => {
                spans.push(Span::styled(
                    key.clone(),
                    theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(" {}", hint.what),
                    theme.fg(Tok::Muted),
                ));
            }
            None => spans.push(Span::styled(hint.what.clone(), theme.fg(Tok::Muted))),
        }
    }
    spans.push(Span::styled(
        format!(" {}{bar}{right}", bar.repeat(fill)),
        border,
    ));
    Line::from(spans)
}

fn list_rows(ui: &UiConfig) -> usize {
    ui.autocomplete_items.max(1)
}

/// First visible item for a window of `rows` over `total` items that keeps `selected` in view,
/// moving from the previous `scroll` as little as possible.
fn window(selected: usize, scroll: usize, rows: usize, total: usize) -> usize {
    let rows = rows.max(1).min(total.max(1));
    let max_start = total.saturating_sub(rows);
    let mut start = scroll.min(max_start);
    if selected < start {
        start = selected;
    } else if selected >= start + rows {
        start = selected + 1 - rows;
    }
    start.min(max_start)
}

/// Render `rows` list rows starting at item `start`: selected row on `SelectedBg`, a scrollbar in
/// the last column and an `n/m` counter on the last row when the list scrolls. `build` produces a
/// row's spans for an item given its cell budget; the result is clipped to that budget.
fn render_list(
    theme: &Theme,
    w: usize,
    total: usize,
    rows: usize,
    start: usize,
    selected: usize,
    build: impl Fn(usize, usize, bool) -> Vec<Span<'static>>,
) -> Rows {
    let scrolls = total > rows;
    let usable = w.saturating_sub(usize::from(scrolls));
    let counter = if scrolls {
        format!("{}/{total}", selected + 1)
    } else {
        String::new()
    };
    let thumb_len = if scrolls {
        (rows * rows / total).max(1)
    } else {
        0
    };
    let max_start = total.saturating_sub(rows);
    let thumb_start = (start * (rows - thumb_len) + max_start / 2)
        .checked_div(max_start)
        .unwrap_or(0);
    (0..rows)
        .map(|r| {
            let index = start + r;
            let is_selected = index == selected;
            let reserve = if scrolls && r + 1 == rows {
                text::width(&counter) + 1
            } else {
                0
            };
            let budget = usable.saturating_sub(reserve);
            let mut spans = clip_spans(build(index, budget, is_selected), budget);
            if scrolls || is_selected {
                let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
                spans.push(Span::raw(" ".repeat(budget - used)));
                if reserve > 0 {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(counter.clone(), theme.fg(Tok::Dim)));
                }
            }
            if is_selected {
                super::chrome::select_spans(theme, &mut spans);
            }
            if scrolls {
                let on = r >= thumb_start && r < thumb_start + thumb_len;
                spans.push(if on {
                    Span::styled(theme.sym(Sym::ScrollThumb).to_owned(), theme.fg(Tok::Muted))
                } else {
                    Span::styled(theme.sym(Sym::ScrollTrack).to_owned(), theme.fg(Tok::Dim))
                });
            }
            Line::from(spans)
        })
        .collect()
}

fn finish_row(spans: Vec<Span<'static>>, w: usize) -> Line<'static> {
    Line::from(clip_spans(spans, w))
}

/// Cells before the input in the band: the prompt mark one cell in, then the text column the
/// transcript's answers and tool rows also start at.
const BAND_LEAD: usize = 3;

/// One row of the composer band: `spans` on the band's tint, padded to exactly `w` cells.
fn band_row(spans: Vec<Span<'static>>, w: usize, tint: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = clip_spans(spans, w)
        .into_iter()
        .map(|mut span| {
            span.style = tint.patch(span.style);
            span
        })
        .collect();
    let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
    if used < w {
        spans.push(Span::styled(" ".repeat(w - used), tint));
    }
    Line::from(spans)
}

/// Puts `rows` above the view's rows and moves its cursor down by the same amount.
fn prepend_rows(view: &mut ComposerView, rows: Rows) {
    view.cursor.0 += rows.len() as u16;
    let mut below = std::mem::take(&mut view.rows);
    view.rows = rows;
    view.rows.append(&mut below);
}

fn clip_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut left = max;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let cells = text::width(&span.content);
        if cells <= left {
            left -= cells;
            out.push(span);
            continue;
        }
        let kept = text::clip(&span.content, left);
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        break;
    }
    out
}

/// `value` as spans with the characters at `hits` (char indexes) emphasized.
fn highlighted(value: &str, hits: &[usize], normal: Style, hit: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_hit = false;
    for (index, ch) in value.chars().enumerate() {
        let is_hit = hits.contains(&index);
        if is_hit != run_hit && !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if run_hit { hit } else { normal },
            ));
        }
        run_hit = is_hit;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_hit { hit } else { normal }));
    }
    spans
}

/// Char indexes of the first case-insensitive occurrence of `query_lower` in `value`.
fn substring_hits(value: &str, query_lower: &str) -> Vec<usize> {
    if query_lower.is_empty() {
        return Vec::new();
    }
    let lower = value.to_lowercase();
    if lower.chars().count() != value.chars().count() {
        return Vec::new();
    }
    let Some(byte) = lower.find(query_lower) else {
        return Vec::new();
    };
    let first = lower[..byte].chars().count();
    (first..first + query_lower.chars().count()).collect()
}

/// Candidate count above which file ranking moves to a worker thread. Measured in release builds:
/// about 15 ms for 50k paths and under 1 ms for 2k, so anything at or below this rank inline.
const SYNC_RANK_LIMIT: usize = 2_000;

/// Commands listed first when `/` is typed with nothing after it. Defined with the other starter
/// lists in `command_surface`, so the menu and the palette cannot disagree.
const START_HERE: &[&str] = crate::ui::command_surface::START_HERE;

/// The unfiltered slash menu's groups, in order; the most used come first.
const MENU_GROUPS: [crate::ui::command_surface::Category; 10] = {
    use crate::ui::command_surface::Category as C;
    [
        C::Model,
        C::Settings,
        C::Session,
        C::Context,
        C::Review,
        C::Work,
        C::Modes,
        C::Integrations,
        C::Maintenance,
        C::Learning,
    ]
};

/// Where a command sits in the unfiltered menu: built-ins by category, then skills, then custom.
fn group_rank(command: &CommandEntry) -> usize {
    match command.kind {
        CommandKind::Builtin => {
            let category = crate::ui::command_surface::category(&command.name);
            MENU_GROUPS
                .iter()
                .position(|c| *c == category)
                .unwrap_or(MENU_GROUPS.len())
        }
        CommandKind::Skill => MENU_GROUPS.len() + 1,
        CommandKind::Custom => MENU_GROUPS.len() + 2,
        CommandKind::File => MENU_GROUPS.len() + 3,
    }
}

/// The group label for a command in the unfiltered menu.
fn group_of(command: &CommandEntry) -> &'static str {
    match command.kind {
        CommandKind::Builtin => crate::ui::command_surface::category(&command.name).tab(),
        CommandKind::Skill => "Skills",
        CommandKind::Custom => "Custom",
        CommandKind::File => "",
    }
}

/// Name matches lead description-only matches; within either field, prefixes lead word starts,
/// then subsequences. Fuzzy score breaks ties and supplies highlight positions.
fn rank(commands: &[CommandEntry], query: &str) -> Vec<Match> {
    let lower = query.to_lowercase();
    let tier = |value: &str| {
        if lower.is_empty() {
            return 0u8;
        }
        let value = value.to_lowercase();
        if value.starts_with(&lower) {
            2
        } else if value
            .char_indices()
            .any(|(i, c)| !c.is_alphanumeric() && value[i + c.len_utf8()..].starts_with(&lower))
        {
            1
        } else {
            0
        }
    };
    let mut scored: Vec<((bool, bool, u8, i32), Match)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let name = text::fuzzy(query, &command.name);
            let desc = text::fuzzy(query, &command.description);
            if name.is_none() && desc.is_none() {
                return None;
            }
            let exact = !query.is_empty() && command.name.eq_ignore_ascii_case(query);
            let score = match (&name, &desc) {
                (Some((s, _)), _) => (exact, true, tier(&command.name), *s),
                (None, Some((s, _))) => (false, false, tier(&command.description), *s),
                (None, None) => unreachable!(),
            };
            Some((
                score,
                Match {
                    index,
                    name_hits: name.map(|(_, p)| p).unwrap_or_default(),
                    desc_hits: desc.map(|(_, p)| p).unwrap_or_default(),
                },
            ))
        })
        .collect();
    if lower.is_empty() {
        // Nothing typed yet: everyday commands first, grouped (models, setup, sessions, …), each
        // group led by what people reach for first; then everything else, grouped the same way.
        let priority = |name: &str| {
            START_HERE
                .iter()
                .position(|p| p.eq_ignore_ascii_case(name))
                .unwrap_or(START_HERE.len())
        };
        scored.sort_by_key(|(_, m)| {
            let command = &commands[m.index];
            (
                !(command.kind == CommandKind::Builtin
                    && crate::ui::command_surface::everyday(&command.name)),
                group_rank(command),
                priority(&command.name),
            )
        });
    } else {
        scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    }
    scored.into_iter().map(|(_, m)| m).collect()
}

/// Stable per-session accent token (FNV-1a, so it does not change between runs or builds).
fn accent_for(session: &str) -> Tok {
    const ACCENTS: [Tok; 6] = [
        Tok::Accent,
        Tok::ThinkLow,
        Tok::ThinkMedium,
        Tok::ThinkHigh,
        Tok::StatusPath,
        Tok::StatusModel,
    ];
    let hash = session.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    ACCENTS[(hash % ACCENTS.len() as u64) as usize]
}

/// Replace each still-intact collapse placeholder with the pasted text it stands for, in one pass so
/// pasted text that looks like a placeholder is never expanded again.
fn expand_pastes(draft: &str, pastes: &[(String, String)]) -> String {
    let mut out = String::with_capacity(draft.len());
    let mut rest = draft;
    while let Some(at) = rest.find("[Pasted text #") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match pastes
            .iter()
            .find(|(placeholder, _)| tail.starts_with(placeholder.as_str()))
        {
            Some((placeholder, body)) => {
                out.push_str(body);
                rest = &tail[placeholder.len()..];
            }
            None => {
                out.push('[');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests;
