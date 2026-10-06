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
const MAX_ARGS_COLUMN: usize = 20;
/// App-level shortcuts the composer reports instead of handling.
const APP_ACTIONS: [Action; 10] = [
    Action::ModelSelect,
    Action::ModelRoles,
    Action::ProviderSetup,
    Action::Settings,
    Action::ToolsExpand,
    Action::ToolsToggle,
    Action::ThinkingToggle,
    Action::ApprovalCycle,
    Action::DisplayReset,
    Action::EditExternal,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    Builtin,
    Custom,
    Skill,
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

struct Menu {
    query: String,
    file_start: Option<usize>,
    matches: Vec<Match>,
    selected: usize,
    scroll: usize,
}

struct Search {
    query: Editor,
    selected: usize,
    scroll: usize,
}

pub struct Composer {
    editor: Editor,
    history: Vec<String>,
    history_limit: usize,
    history_index: Option<usize>,
    saved_draft: Option<Editor>,
    commands: Vec<CommandEntry>,
    argument_completions: Vec<String>,
    files: Vec<CommandEntry>,
    menu: Option<Menu>,
    /// The exact draft the user dismissed the menu/ghost on; editing the text ends the dismissal.
    dismissed: Option<String>,
    search: Option<Search>,
    busy: bool,
    queued: std::collections::VecDeque<(String, bool)>,
    editing_queue: Option<bool>,
    exit_armed: bool,
    /// `(placeholder, full text)` for collapsed pastes still referenced by the draft.
    pastes: Vec<(String, String)>,
    paste_count: usize,
}

impl Composer {
    pub fn new(history: Vec<String>, history_limit: usize) -> Self {
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
            exit_armed: false,
            pastes: Vec::new(),
            paste_count: 0,
            queued: std::collections::VecDeque::new(),
            editing_queue: None,
            files: Vec::new(),
            argument_completions: Vec::new(),
        };
        for entry in history {
            composer.push_history(entry);
        }
        composer
    }

    pub fn draft(&self) -> String {
        self.editor.text.clone()
    }

    pub fn set_draft(&mut self, text: &str) {
        self.editor.set(text);
        self.history_index = None;
        self.saved_draft = None;
        self.exit_armed = false;
        self.dismissed = None;
        self.pastes.clear();
        self.editing_queue = None;
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
        if self.history_limit == 0 || entry.trim().is_empty() || self.history.last() == Some(&entry)
        {
            return;
        }
        self.history.push(entry);
        let excess = self.history.len().saturating_sub(self.history_limit);
        self.history.drain(..excess);
    }

    /// Commands come from the controller so the menu matches real dispatch. Names that could not be
    /// typed as a command (empty, whitespace, slashes, controls) and duplicates are dropped.
    pub fn set_commands(&mut self, commands: Vec<CommandEntry>) {
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
    }

    /// Valid argument continuations supplied by the controller, in preference order. Unlike history,
    /// these are available before a command has ever been run. Never insert display placeholders.
    pub fn set_argument_completions(&mut self, candidates: Vec<String>) {
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
    }

    /// While busy Enter steers, Alt-Enter schedules a follow-up, and Esc interrupts.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        self.exit_armed = false;
        self.refresh();
    }

    pub fn set_files(&mut self, paths: Vec<String>) {
        self.files = paths
            .into_iter()
            .map(|name| CommandEntry {
                name,
                description: "project file".into(),
                kind: CommandKind::Custom,
                args_hint: None,
            })
            .collect();
        self.menu = None;
        self.refresh();
    }

    fn menu_entries(&self, menu: &Menu) -> &[CommandEntry] {
        if menu.file_start.is_some() {
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

    // -----------------------------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------------------------

    /// Bracketed paste never submits. Long pastes collapse to `[Pasted text #n +X lines]` and are
    /// expanded when the draft is submitted.
    pub fn handle_paste(&mut self, pasted: &str) {
        let clean = text::sanitize(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
        self.exit_armed = false;
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

        if self.editing_queue.is_some() && key.code == KeyCode::Esc {
            self.editing_queue = None;
            self.clear();
            return ComposerEvent::None;
        }
        if self.editor.text.is_empty() && key.code == KeyCode::Up {
            if let Some((text, follow_up)) = self.queued.pop_back() {
                self.set_draft(&text);
                self.editing_queue = Some(follow_up);
                return ComposerEvent::None;
            }
        }
        if self.busy && key.code == KeyCode::Enter && key.modifiers == KeyModifiers::ALT {
            return if self.editor.text.trim_start().starts_with('/') {
                self.submit()
            } else {
                self.submit_queued(true)
            };
        }
        if let Some(action) = APP_ACTIONS.into_iter().find(|a| keys.matches(*a, &key)) {
            return ComposerEvent::Action(action);
        }
        if keys.matches(Action::HistorySearch, &key) {
            self.search = Some(Search {
                query: Editor::default(),
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
        if let Some(menu) = self.menu.as_ref().filter(|m| !m.matches.is_empty()) {
            let selected = &self.menu_entries(menu)[menu.matches[menu.selected].index];
            // A partial name is completed (so arguments can be added); only a complete name runs.
            if menu.file_start.is_some()
                || !selected.name.eq_ignore_ascii_case(&self.editor.text[1..])
            {
                self.complete_menu();
                return ComposerEvent::None;
            }
        }
        if self.busy && !self.editor.text.trim_start().starts_with('/') {
            return self.submit_queued(false);
        }
        if self.editor.text.trim().is_empty() {
            return ComposerEvent::None;
        }
        let draft = std::mem::take(&mut self.editor.text);
        self.editor.cursor = 0;
        let sent = expand_pastes(&draft, &self.pastes);
        self.pastes.clear();
        self.history_index = None;
        self.saved_draft = None;
        self.editing_queue = None;
        self.push_history(sent.clone());
        ComposerEvent::Submit(sent)
    }

    fn submit_queued(&mut self, follow_up: bool) -> ComposerEvent {
        if self.editor.text.trim().is_empty() {
            return ComposerEvent::None;
        }
        let sent = expand_pastes(&self.editor.text, &self.pastes);
        self.push_history(sent.clone());
        self.queued
            .push_back((sent, follow_up || self.editing_queue.unwrap_or(false)));
        self.clear();
        self.editing_queue = None;
        ComposerEvent::None
    }

    fn complete_menu(&mut self) -> bool {
        let Some(menu) = self.menu.as_ref().filter(|m| !m.matches.is_empty()) else {
            return false;
        };
        let entry = &self.menu_entries(menu)[menu.matches[menu.selected].index];
        let completed = if let Some(start) = menu.file_start {
            format!("{}@{} ", &self.editor.text[..start], entry.name)
        } else {
            format!("/{} ", entry.name)
        };
        self.editor.set(&completed);
        self.dismissed = None;
        true
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
        let query = self
            .search
            .as_ref()
            .map(|s| s.query.text.to_lowercase())
            .unwrap_or_default();
        self.history
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, entry)| entry.to_lowercase().contains(&query))
            .map(|(i, _)| i)
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
                .map(|&i| self.history[i].clone());
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
        (!rest.chars().any(char::is_whitespace)).then_some((rest, None))
    }

    fn refresh(&mut self) {
        if self
            .dismissed
            .as_deref()
            .is_some_and(|d| d != self.editor.text)
        {
            self.dismissed = None;
        }
        match self.menu_query() {
            None => self.menu = None,
            Some((query, start))
                if self
                    .menu
                    .as_ref()
                    .is_some_and(|m| m.query == query && m.file_start == start) => {}
            Some((query, file_start)) => {
                let query = query.to_owned();
                self.menu = Some(Menu {
                    matches: rank(
                        if file_start.is_some() {
                            &self.files
                        } else {
                            &self.commands
                        },
                        &query,
                    ),
                    query,
                    file_start,
                    selected: 0,
                    scroll: 0,
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
            || self.menu_open()
            || draft.is_empty()
            || self.editor.cursor != draft.len()
            || self.dismissed.as_deref() == Some(draft)
        {
            return None;
        }
        let extends = |entry: &str| {
            entry.starts_with(draft)
                && entry.len() > draft.len()
                && entry
                    .grapheme_indices(true)
                    .any(|(offset, _)| offset == draft.len())
        };
        // Names still use the command menu; argument ghosts start after its first space.
        if draft.starts_with('/') && draft.contains(' ') {
            if let Some(entry) = self
                .argument_completions
                .iter()
                .find(|entry| extends(entry))
            {
                return Some(&entry[draft.len()..]);
            }
        }
        self.history
            .iter()
            .rev()
            .find(|entry| extends(entry))
            .map(|entry| &entry[draft.len()..])
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
            return self.bare_view(w, ghost, theme);
        }

        let boxed = ctx.ui.composer_shape == ComposerShape::Box;
        let inner = if boxed { w - 4 } else { w };
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
        let bar = theme.sym(Sym::BoxH);
        let side = theme.sym(Sym::BoxV);
        let (top_l, top_r, bottom_l, bottom_r) = if boxed {
            (
                theme.sym(Sym::BoxTl),
                theme.sym(Sym::BoxTr),
                theme.sym(Sym::BoxBl),
                theme.sym(Sym::BoxBr),
            )
        } else {
            (bar, bar, bar, bar)
        };

        let mut rows: Rows = vec![statusline::border_line(
            ctx.ui, ctx.status, theme, width, border, top_l, top_r,
        )];
        for row in laid.rows.iter().skip(start).take(MAX_INPUT_ROWS) {
            let mut spans = Vec::new();
            if boxed {
                spans.push(Span::styled(format!("{side} "), border));
            }
            let used: usize = row.iter().map(|(t, _)| text::width(t)).sum();
            spans.extend(
                row.iter()
                    .map(|(t, kind)| Span::styled(t.clone(), kind.style(theme))),
            );
            if boxed {
                spans.push(Span::raw(" ".repeat(inner - used + 1)));
                spans.push(Span::styled(side.to_owned(), border));
            }
            rows.push(Line::from(spans));
        }
        let cursor = (
            (1 + laid.cursor.0 - start) as u16,
            (laid.cursor.1 + if boxed { 2 } else { 0 }) as u16,
        );
        rows.push(bottom_border(
            w,
            &self.hint(ctx),
            bottom_l,
            bottom_r,
            bar,
            border,
            theme,
        ));

        let mut cursor = cursor;
        if let Some(search) = &self.search {
            let (list, col) = self.search_rows(search, w, ctx);
            cursor = (rows.len() as u16, col as u16);
            rows.extend(list);
        } else if let Some(menu) = &self.menu {
            rows.extend(self.menu_rows(menu, w, ctx));
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
            queued.push(Line::from(Span::styled(
                text::ellipsize(
                    &format!(
                        "{} queued · Up edit last · Esc cancels edited item",
                        self.queued.len()
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

    /// Border color by state: shell mode, busy, the optional per-session accent, else the plain border.
    fn border_style(&self, ctx: &ComposerCtx) -> Style {
        let theme = ctx.theme;
        let token = if self.mode() == InputMode::Shell {
            Tok::ShellMode
        } else if self.busy {
            Tok::Success
        } else if ctx.ui.session_accent && !ctx.status.session.is_empty() {
            accent_for(&ctx.status.session)
        } else {
            Tok::Border
        };
        theme.fg(token)
    }

    /// Priority-ordered, indivisible hints; disabled or shadowed bindings are not advertised.
    fn hint(&self, ctx: &ComposerCtx) -> Vec<String> {
        if !ctx.ui.hints {
            return Vec::new();
        }
        let keys = ctx.keys;
        let label = |action: Action| {
            keys.chords(action)
                .iter()
                .find(|chord| {
                    let key = KeyEvent::new(chord.code, chord.mods);
                    // These queue controls are literal keys in edit_key, not remappable actions.
                    !(self.search.is_none()
                        && ((self.editing_queue.is_some() && key.code == KeyCode::Esc)
                            || (self.busy
                                && key.code == KeyCode::Enter
                                && key.modifiers == KeyModifiers::ALT)
                            || (action == Action::Interrupt
                                && keys.matches(Action::Dismiss, &key)
                                && (self.menu_open() || self.ghost().is_some()))))
                })
                .map(|chord| chord.display())
                .unwrap_or_default()
        };
        let hint = |action: Action, what: &str| {
            let label = label(action);
            (!label.is_empty()).then(|| format!("{label} {what}"))
        };
        let select = || {
            let labels: Vec<_> = [Action::SelectUp, Action::SelectDown]
                .into_iter()
                .map(label)
                .filter(|s| !s.is_empty())
                .collect();
            (!labels.is_empty()).then(|| format!("{} select", labels.join("/")))
        };
        let mut pieces = Vec::new();
        if self.search.is_some() {
            pieces.extend([
                hint(Action::Submit, "accept"),
                select(),
                hint(Action::Dismiss, "cancel"),
            ]);
        } else {
            if self.busy {
                pieces.push(hint(Action::Interrupt, "interrupt"));
            }
            if let Some(follow_up) = self.editing_queue {
                pieces.push(Some(format!(
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
                pieces.push(Some("Esc cancel edit".into()));
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
                pieces.push(hint(
                    Action::Submit,
                    if command { "run command" } else { "steer" },
                ));
                if !command {
                    pieces.push(Some("Alt+Enter follow-up".into()));
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
                if self.mode() == InputMode::Shell {
                    pieces.push(Some("shell mode".into()));
                }
                if self.ghost().is_some() {
                    pieces.extend([
                        hint(Action::Complete, "accept suggestion"),
                        hint(Action::Dismiss, "dismiss"),
                    ]);
                }
            } else {
                pieces.extend([
                    Some("/ commands".into()),
                    Some("/help help".into()),
                    hint(Action::ModelSelect, "model"),
                    hint(Action::Settings, "settings"),
                    hint(Action::HistorySearch, "history"),
                ]);
            }
        }
        pieces.into_iter().flatten().collect()
    }

    fn menu_rows(&self, menu: &Menu, w: usize, ctx: &ComposerCtx) -> Rows {
        let theme = ctx.theme;
        let total = menu.matches.len();
        if total == 0 {
            let message = if menu.file_start.is_some() {
                "  No matching files"
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
            .min(if menu.file_start.is_some() {
                w.saturating_sub(6)
            } else {
                MAX_NAME_COLUMN
            });
        let args_col = visible
            .iter()
            .filter_map(|m| entries[m.index].args_hint.as_deref())
            .map(text::width)
            .max()
            .unwrap_or(0)
            .min(MAX_ARGS_COLUMN);
        let pointer = theme.sym(Sym::Pointer);
        let blank_pointer = " ".repeat(text::width(pointer));
        let accent = theme.fg(Tok::Accent);
        let hit = accent.add_modifier(Modifier::BOLD);
        render_list(
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
                let mut used = prefix + name_col + 2;
                if args_col > 0 {
                    let args = text::ellipsize(
                        command.args_hint.as_deref().unwrap_or(""),
                        args_col,
                        theme.ascii(),
                    );
                    let cells = text::width(&args);
                    spans.push(Span::styled(args, theme.fg(Tok::Dim)));
                    spans.push(Span::raw(" ".repeat(args_col - cells + 2)));
                    used += args_col + 2;
                }
                let room = budget.saturating_sub(used);
                if room > 0 {
                    let description = text::ellipsize(&command.description, room, theme.ascii());
                    spans.extend(highlighted(
                        &description,
                        &found.desc_hits,
                        theme.fg(Tok::Muted),
                        hit,
                    ));
                }
                spans
            },
        )
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
                let entry = text::one_line(&self.history[matches[i]]);
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

/// `╰─ hint ──────╯`, exactly `w` cells; lower-priority hint items are dropped whole.
fn bottom_border(
    w: usize,
    hints: &[String],
    left: &str,
    right: &str,
    bar: &str,
    border: Style,
    theme: &Theme,
) -> Line<'static> {
    let frame = text::width(left) + text::width(right) + 2 * text::width(bar);
    let room = w.saturating_sub(frame + 2);
    let sep = format!(" {} ", theme.sym(Sym::Dot));
    let mut keep = hints.len();
    while keep > 0 && text::width(&hints[..keep].join(&sep)) > room {
        keep -= 1;
    }
    let hint = hints[..keep].join(&sep);
    if hint.is_empty() {
        let corners = text::width(left) + text::width(right);
        return Line::from(Span::styled(
            format!("{left}{}{right}", bar.repeat(w.saturating_sub(corners))),
            border,
        ));
    }
    let fill = room - text::width(&hint);
    Line::from(vec![
        Span::styled(format!("{left}{bar} "), border),
        Span::styled(hint, theme.fg(Tok::Muted)),
        Span::styled(format!(" {}{bar}{right}", bar.repeat(fill)), border),
    ])
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
    let thumb_start = if max_start == 0 {
        0
    } else {
        (start * (rows - thumb_len) + max_start / 2) / max_start
    };
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
                let bg = theme.bg(Tok::SelectedBg);
                for span in &mut spans {
                    span.style = span.style.patch(bg);
                }
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
    scored.sort_by(|a, b| b.0.cmp(&a.0));
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
mod tests {
    use super::*;
    use crate::ui::status::Phase;

    struct Env {
        theme: Theme,
        keys: Keymap,
        ui: UiConfig,
        status: StatusData,
    }
    impl Env {
        fn new() -> Env {
            let ui = UiConfig {
                session_accent: false,
                ..UiConfig::default()
            };
            let status = StatusData {
                provider: "mlx".into(),
                model: "qwen3-9b".into(),
                project_path: "/work/projects/dial".into(),
                ..StatusData::default()
            };
            Env {
                theme: Theme::dark(),
                keys: Keymap::defaults(),
                ui,
                status,
            }
        }
        fn ctx(&self) -> ComposerCtx<'_> {
            ComposerCtx {
                theme: &self.theme,
                keys: &self.keys,
                ui: &self.ui,
                status: &self.status,
            }
        }
    }

    fn event(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    fn key(c: &mut Composer, env: &Env, code: KeyCode, mods: KeyModifiers) -> ComposerEvent {
        c.handle_key(event(code, mods), &env.ctx())
    }
    fn press(c: &mut Composer, env: &Env, code: KeyCode) -> ComposerEvent {
        key(c, env, code, KeyModifiers::NONE)
    }
    fn ctrl(c: &mut Composer, env: &Env, ch: char) -> ComposerEvent {
        key(c, env, KeyCode::Char(ch), KeyModifiers::CONTROL)
    }
    fn typed(c: &mut Composer, env: &Env, text: &str) {
        for ch in text.chars() {
            press(c, env, KeyCode::Char(ch));
        }
    }
    fn lines(rows: &Rows) -> Vec<String> {
        rows.iter()
            .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }
    fn screen(c: &Composer, env: &Env, width: u16) -> Vec<String> {
        lines(&c.view(width, &env.ctx()).rows)
    }
    fn composer() -> Composer {
        Composer::new(vec!["older prompt".into(), "recent prompt".into()], 100)
    }

    fn cmd(name: &str, desc: &str, kind: CommandKind) -> CommandEntry {
        CommandEntry {
            name: name.into(),
            description: desc.into(),
            kind,
            args_hint: None,
        }
    }
    fn commands() -> Vec<CommandEntry> {
        let b = |n: &str, d: &str| cmd(n, d, CommandKind::Builtin);
        vec![
            b("help", "Show help"),
            b("model", "Switch model"),
            b("modelpreset", "Model presets"),
            b("memory", "Memory tools"),
            b("settings", "Open settings"),
            b("sessions", "Browse sessions"),
            b("search", "Search files"),
            b("remove-dir", "Remove a directory"),
            cmd("skill:review", "Review a diff", CommandKind::Skill),
            cmd("deploy", "Run the deploy script", CommandKind::Custom),
        ]
    }
    fn with_commands() -> Composer {
        let mut c = composer();
        c.set_commands(commands());
        c
    }

    #[test]
    fn every_row_fits_the_width_and_the_box_has_its_structure_at_all_sizes() {
        let env = Env::new();
        let mut c = composer();
        c.set_commands(commands());
        c.set_draft("界e\u{301}👩‍💻 some fairly long text that must wrap around the narrow box several times\nsecond line");
        for width in [1u16, 2, 7, 8, 9, 20, 60, 120] {
            let view = c.view(width, &env.ctx());
            let rows = lines(&view.rows);
            for (row, line) in view.rows.iter().zip(&rows) {
                assert!(
                    text::line_width(row) <= width as usize,
                    "width {width}: {line:?}"
                );
            }
            assert!(
                (view.cursor.0 as usize) < rows.len()
                    && (view.cursor.1 as usize) < width.max(1) as usize,
                "width {width}: cursor {:?} in {rows:?}",
                view.cursor
            );
            if width >= 8 {
                let (top, bottom) = (&rows[0], rows.last().unwrap());
                assert!(
                    top.starts_with('╭')
                        && top.ends_with('╮')
                        && text::width(top) == width as usize,
                    "{top:?}"
                );
                assert!(
                    bottom.starts_with('╰')
                        && bottom.ends_with('╯')
                        && text::width(bottom) == width as usize,
                    "{bottom:?}"
                );
                for middle in &rows[1..rows.len() - 1] {
                    assert!(
                        middle.starts_with('│') && middle.ends_with('│'),
                        "{middle:?}"
                    );
                }
            } else {
                assert_eq!(
                    rows.len(),
                    1,
                    "a bare input row below {MIN_BOX_WIDTH} cells"
                );
            }
        }
        assert_eq!(
            c.view(0, &env.ctx()).rows.len(),
            1,
            "width 0 must not panic"
        );
        let empty = Composer::new(vec![], 10);
        for width in [1u16, 8, 60] {
            assert!(empty.view(width, &env.ctx()).rows.len() >= 1);
        }
    }

    #[test]
    fn cursor_follows_text_cells_including_wide_and_wrapped_text() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        assert_eq!(
            c.view(30, &env.ctx()).cursor,
            (1, 2),
            "empty draft: inside the left border and padding"
        );
        typed(&mut c, &env, "ab");
        assert_eq!(c.view(30, &env.ctx()).cursor, (1, 4));
        c.set_draft("日本語");
        assert_eq!(
            c.view(30, &env.ctx()).cursor,
            (1, 2 + 6),
            "wide characters count two cells each"
        );
        press(&mut c, &env, KeyCode::Left);
        assert_eq!(c.view(30, &env.ctx()).cursor, (1, 2 + 4));
        c.set_draft("e\u{301}👩‍💻x");
        assert_eq!(
            c.view(30, &env.ctx()).cursor,
            (1, 2 + 1 + 2 + 1),
            "combining marks and ZWJ emoji are single graphemes"
        );
        // Inner width at 12 cells is 8: eight narrow chars fill the row, the cursor wraps to the next.
        c.set_draft("12345678");
        let view = c.view(12, &env.ctx());
        assert_eq!(view.cursor, (2, 2), "{:?}", lines(&view.rows));
        assert_eq!(lines(&view.rows).len(), 4);
        c.set_draft("1234567日");
        let rows = screen(&c, &env, 12);
        assert_eq!(
            rows[1], "│ 1234567  │",
            "a wide character that does not fit moves to the next row: {rows:?}"
        );
        assert!(rows[2].starts_with("│ 日"));
        c.set_draft("a\nb");
        assert_eq!(c.view(30, &env.ctx()).cursor, (2, 3));
    }

    #[test]
    fn long_drafts_scroll_to_keep_the_cursor_visible() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        c.set_draft(
            &(0..30)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let view = c.view(40, &env.ctx());
        let rows = lines(&view.rows);
        assert_eq!(rows.len(), MAX_INPUT_ROWS + 2);
        assert!(
            rows[MAX_INPUT_ROWS].contains("line 29") && !rows.iter().any(|r| r.contains("line 5 ")),
            "{rows:?}"
        );
        assert_eq!(
            view.cursor.0 as usize, MAX_INPUT_ROWS,
            "cursor on the last visible row"
        );
        for _ in 0..29 {
            press(&mut c, &env, KeyCode::Up);
        }
        let view = c.view(40, &env.ctx());
        assert!(lines(&view.rows)[1].contains("line 0") && view.cursor.0 == 1);
    }

    #[test]
    fn line_shape_has_rules_without_side_borders() {
        let mut env = Env::new();
        env.ui.composer_shape = ComposerShape::Line;
        let mut c = Composer::new(vec![], 10);
        c.set_draft("hello");
        let view = c.view(40, &env.ctx());
        let rows = lines(&view.rows);
        assert_eq!(rows.len(), 3);
        assert!(
            rows[0].starts_with("──") && rows[0].ends_with("──") && text::width(&rows[0]) == 40,
            "{:?}",
            rows[0]
        );
        assert_eq!(rows[1], "hello");
        assert!(
            rows[2].starts_with("──") && rows[2].ends_with("──") && text::width(&rows[2]) == 40,
            "{:?}",
            rows[2]
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.contains('│') || r.contains('╭') || r.contains('╰')),
            "{rows:?}"
        );
        assert_eq!(view.cursor, (1, 5));
        env.ui.composer_shape = ComposerShape::Box;
        assert!(screen(&c, &env, 40)[0].starts_with('╭'));
    }

    #[test]
    fn the_status_line_sits_in_the_top_border_and_follows_the_ui_config() {
        let mut env = Env::new();
        env.status.prompt_tokens = Some(3900);
        env.status.context_window = 32_000;
        let c = Composer::new(vec![], 10);
        let top = screen(&c, &env, 100)[0].clone();
        assert!(
            top.contains("mlx/qwen3-9b") && top.contains("3.9k/32k") && text::width(&top) == 100,
            "{top:?}"
        );
        env.ui.status = crate::llm::model_config::StatusPreset::Minimal;
        let top = screen(&c, &env, 100)[0].clone();
        assert!(
            top.contains("mlx/qwen3-9b") && !top.contains("projects/dial"),
            "{top:?}"
        );
    }

    #[test]
    fn transparent_status_has_no_background_in_the_composer_border() {
        let mut env = Env::new();
        let c = Composer::new(vec![], 10);
        let has_bg = |env: &Env| {
            c.view(100, &env.ctx()).rows[0]
                .spans
                .iter()
                .any(|s| s.style.bg.is_some())
        };
        assert!(has_bg(&env), "the default status line is tinted");
        env.ui.status_transparent = true;
        assert!(!has_bg(&env));
    }

    #[test]
    fn border_color_follows_state_and_the_session_accent_is_stable() {
        let mut env = Env::new();
        let mut c = Composer::new(vec![], 10);
        let border = |c: &Composer, env: &Env| c.view(60, &env.ctx()).rows[1].spans[0].style.fg;
        assert_eq!(border(&c, &env), env.theme.fg(Tok::Border).fg);
        c.set_busy(true);
        assert_eq!(border(&c, &env), env.theme.fg(Tok::Success).fg);
        c.set_busy(false);
        c.set_draft("!ls");
        assert_eq!(border(&c, &env), env.theme.fg(Tok::ShellMode).fg);
        c.clear();
        env.ui.session_accent = true;
        env.status.session = "sess-1".into();
        let first = border(&c, &env);
        assert_eq!(first, border(&c, &env), "same session, same accent");
        let accents: Vec<_> = (0..40)
            .map(|n| {
                env.status.session = format!("session-{n}");
                border(&c, &env)
            })
            .collect();
        assert!(
            accents
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1,
            "different sessions get different accents"
        );
        env.status.session.clear();
        assert_eq!(
            border(&c, &env),
            env.theme.fg(Tok::Border).fg,
            "no session, no accent"
        );
    }

    #[test]
    fn shell_mode_starts_with_bang_and_colors_the_marker() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        assert_eq!(c.mode(), InputMode::Normal);
        typed(&mut c, &env, "!ls -la");
        assert_eq!(c.mode(), InputMode::Shell);
        let rows = c.view(60, &env.ctx()).rows;
        let marker = rows[1]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "!")
            .expect("marker span");
        assert_eq!(marker.style.fg, env.theme.fg(Tok::ShellMode).fg);
        assert!(lines(&rows)[2].contains("shell mode"));
        assert!(
            matches!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::Submit(t) if t == "!ls -la")
        );
        assert_eq!(c.mode(), InputMode::Normal);
        c.set_draft("not !shell");
        assert_eq!(c.mode(), InputMode::Normal);
    }

    // ---- hints and keymap -------------------------------------------------------------------

    #[test]
    fn primary_hints_follow_context_and_effective_bindings() {
        let mut env = Env::new();
        let mut c = with_commands();
        assert_eq!(&c.hint(&env.ctx())[..2], ["/ commands", "/help help"]);
        env.keys.rebind(Action::Submit, &["F2"]).unwrap();
        env.keys.rebind(Action::Newline, &["F3"]).unwrap();
        c.set_draft("a draft");
        assert_eq!(&c.hint(&env.ctx())[..2], ["F2 send", "F3 newline"]);
        c.set_busy(true);
        assert_eq!(c.hint(&env.ctx())[0], "Esc interrupt");
        env.keys.rebind(Action::Interrupt, &["F4"]).unwrap();
        assert_eq!(&c.hint(&env.ctx())[..2], ["F4 interrupt", "F2 steer"]);
        env.keys.rebind(Action::Interrupt, &[]).unwrap();
        assert!(!c.hint(&env.ctx()).iter().any(|h| h.contains("interrupt")));
        c.set_busy(false);
        env.keys.rebind(Action::Submit, &[]).unwrap();
        env.keys.rebind(Action::Newline, &[]).unwrap();
        assert!(c.hint(&env.ctx()).is_empty());
        c.set_draft("/mo");
        env.keys.rebind(Action::SelectUp, &[]).unwrap();
        env.keys.rebind(Action::SelectDown, &["F5"]).unwrap();
        env.keys.rebind(Action::Complete, &[]).unwrap();
        env.keys.rebind(Action::Dismiss, &[]).unwrap();
        assert_eq!(c.hint(&env.ctx()), ["F5 select"]);
        env.keys.rebind(Action::SelectDown, &[]).unwrap();
        assert!(c.hint(&env.ctx()).is_empty());
        ctrl(&mut c, &env, 'r');
        assert!(
            c.hint(&env.ctx()).is_empty(),
            "no orphan select slash in search either"
        );
    }

    #[test]
    fn busy_and_queue_edit_hints_respect_literal_control_precedence() {
        let mut env = Env::new();
        let mut c = with_commands();
        c.set_busy(true);
        c.set_draft("/mo");
        assert_eq!(
            c.hint(&env.ctx())[0],
            "Ctrl+C interrupt",
            "Esc dismisses a menu first"
        );
        for follow_up in [false, true] {
            c.set_draft("queued draft");
            let mods = if follow_up {
                KeyModifiers::ALT
            } else {
                KeyModifiers::NONE
            };
            key(&mut c, &env, KeyCode::Enter, mods);
            press(&mut c, &env, KeyCode::Up);
            let hints = c.hint(&env.ctx());
            assert_eq!(
                hints[0], "Ctrl+C interrupt",
                "Esc cancels an edited queue item first"
            );
            assert_eq!(
                hints[1],
                format!(
                    "editing queued {}",
                    if follow_up { "follow-up" } else { "steering" }
                )
            );
            assert!(hints.iter().any(|h| h == "Esc cancel edit"));
            press(&mut c, &env, KeyCode::Enter);
            assert_eq!(
                c.queued.pop_front(),
                Some(("queued draft".into(), follow_up)),
                "rendering must not change queue routing"
            );
        }
        env.keys
            .rebind(Action::Submit, &["Alt+Enter", "F2"])
            .unwrap();
        c.set_draft("draft");
        assert!(
            c.hint(&env.ctx()).iter().any(|h| h == "F2 steer"),
            "Alt+Enter is reserved for follow-up while busy"
        );
    }

    #[test]
    fn footer_drops_whole_items_at_exact_width_boundaries() {
        for ascii in [false, true] {
            let mut env = Env::new();
            if ascii {
                env.theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
            }
            env.keys
                .rebind(Action::Interrupt, &["Ctrl+Shift+F12"])
                .unwrap();
            let mut c = composer();
            c.set_busy(true);
            let hints = c.hint(&env.ctx());
            let sep = format!(" {} ", env.theme.sym(Sym::Dot));
            for shape in [ComposerShape::Box, ComposerShape::Line] {
                env.ui.composer_shape = shape;
                for width in 0..=120u16 {
                    let view = c.view(width, &env.ctx());
                    assert!(
                        view.rows
                            .iter()
                            .all(|r| text::line_width(r) <= width as usize)
                    );
                    if width < MIN_BOX_WIDTH as u16 {
                        continue;
                    }
                    let footer = view.rows.last().unwrap();
                    let shown: String = footer
                        .spans
                        .iter()
                        .filter(|s| s.style == env.theme.fg(Tok::Muted))
                        .map(|s| s.content.as_ref())
                        .collect();
                    let keep = (0..=hints.len())
                        .rev()
                        .find(|&n| text::width(&hints[..n].join(&sep)) <= width as usize - 6)
                        .unwrap();
                    assert_eq!(shown, hints[..keep].join(&sep), "width {width}");
                    assert_eq!(text::line_width(footer), width as usize);
                    if ascii {
                        assert!(lines(&view.rows).iter().all(|r| r.is_ascii()));
                    }
                }
            }
        }
    }

    #[test]
    fn unmatched_completion_rows_are_informational_and_width_safe() {
        for ascii in [false, true] {
            let mut env = Env::new();
            if ascii {
                env.theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
            }
            for (draft, message) in [
                ("/zzzz", "No matching commands"),
                ("@zzzz", "No matching files"),
            ] {
                let mut c = with_commands();
                c.set_files(vec!["src/main.rs".into()]);
                c.set_draft(draft);
                assert!(!c.menu_open());
                assert!(screen(&c, &env, 80).last().unwrap().contains(message));
                let row = c.view(80, &env.ctx()).rows.pop().unwrap();
                assert!(
                    row.spans
                        .iter()
                        .any(|s| s.content.contains(message) && s.style == env.theme.fg(Tok::Dim))
                );
                for width in 0..=80u16 {
                    let view = c.view(width, &env.ctx());
                    assert!(
                        view.rows
                            .iter()
                            .all(|r| text::line_width(r) <= width as usize)
                    );
                    assert!((view.cursor.0 as usize) < view.rows.len());
                    assert!(view.cursor.1 < width.max(1));
                    if ascii {
                        assert!(lines(&view.rows).iter().all(|r| r.is_ascii()));
                    }
                }
                assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
                assert_eq!(c.draft(), draft, "no completion is selected");
                assert_eq!(
                    press(&mut c, &env, KeyCode::Enter),
                    ComposerEvent::Submit(draft.into())
                );
                c.set_draft(draft);
                press(&mut c, &env, KeyCode::Left);
                assert!(
                    !screen(&c, &env, 80).iter().any(|r| r.contains(message)),
                    "inactive query has no empty row"
                );
            }
        }
    }

    #[test]
    fn hints_come_from_the_keymap_and_change_with_state() {
        let env = Env::new();
        let mut c = Composer::new(vec!["hello world".into()], 10);
        let hint = |c: &Composer| {
            screen(c, &env, 110)
                .into_iter()
                .find(|r| r.starts_with('╰'))
                .unwrap()
        };
        let idle = hint(&c);
        assert!(
            idle.contains(&format!("{} model", env.keys.label(Action::ModelSelect)))
                && idle.contains(&format!("{} settings", env.keys.label(Action::Settings))),
            "{idle:?}"
        );
        c.set_busy(true);
        assert!(
            hint(&c).contains(&format!("{} interrupt", env.keys.label(Action::Interrupt))),
            "{:?}",
            hint(&c)
        );
        c.set_busy(false);
        typed(&mut c, &env, "hel");
        assert!(
            hint(&c).contains(&format!(
                "{} accept suggestion",
                env.keys.label(Action::Complete)
            )),
            "{:?}",
            hint(&c)
        );
        c.clear();
        c.set_commands(commands());
        typed(&mut c, &env, "/m");
        assert!(
            hint(&c).contains(&format!("{} complete", env.keys.label(Action::Complete))),
            "{:?}",
            hint(&c)
        );
        c.clear();
        ctrl(&mut c, &env, 'c');
        assert!(
            hint(&c).contains(&format!(
                "{} again to exit",
                env.keys.label(Action::ClearDraft)
            )),
            "{:?}",
            hint(&c)
        );
        ctrl(&mut c, &env, 'r');
        assert!(hint(&c).contains("accept") && hint(&c).contains("cancel"));
        let mut quiet = Env::new();
        quiet.ui.hints = false;
        let bottom = screen(&Composer::new(vec![], 10), &quiet, 80)
            .last()
            .unwrap()
            .clone();
        assert!(
            bottom.chars().all(|ch| ch == '─' || ch == '╰' || ch == '╯'),
            "{bottom:?}"
        );
    }

    #[test]
    fn app_shortcuts_come_from_the_keymap_and_remapping_is_honored() {
        let mut env = Env::new();
        let mut c = Composer::new(vec![], 10);
        let cases = [
            (Action::ModelSelect, KeyCode::Char('m'), KeyModifiers::ALT),
            (Action::ModelRoles, KeyCode::Char('r'), KeyModifiers::ALT),
            (Action::ProviderSetup, KeyCode::Char('p'), KeyModifiers::ALT),
            (Action::Settings, KeyCode::Char(','), KeyModifiers::CONTROL),
            (
                Action::ToolsExpand,
                KeyCode::Char('o'),
                KeyModifiers::CONTROL,
            ),
            (Action::ToolsToggle, KeyCode::Char('o'), KeyModifiers::ALT),
            (
                Action::ThinkingToggle,
                KeyCode::Char('t'),
                KeyModifiers::CONTROL,
            ),
            (Action::ApprovalCycle, KeyCode::BackTab, KeyModifiers::SHIFT),
            (Action::DisplayReset, KeyCode::Char('l'), KeyModifiers::ALT),
            (
                Action::EditExternal,
                KeyCode::Char('g'),
                KeyModifiers::CONTROL,
            ),
        ];
        for (action, code, mods) in cases {
            assert_eq!(
                key(&mut c, &env, code, mods),
                ComposerEvent::Action(action),
                "{action:?}"
            );
        }
        assert_eq!(c.draft(), "", "shortcuts never type text");

        env.keys.rebind(Action::ModelSelect, &["Ctrl+K"]).unwrap();
        assert_eq!(
            ctrl(&mut c, &env, 'k'),
            ComposerEvent::Action(Action::ModelSelect),
            "the remapped chord fires, ahead of the editor's kill-line"
        );
        assert_eq!(
            key(&mut c, &env, KeyCode::Char('m'), KeyModifiers::ALT),
            ComposerEvent::None,
            "the old chord no longer does"
        );
        assert!(
            screen(&c, &env, 110)
                .last()
                .unwrap()
                .contains("Ctrl+K model"),
            "{:?}",
            screen(&c, &env, 110).last()
        );
    }

    // ---- editing, keys, submit --------------------------------------------------------------

    #[test]
    fn grapheme_editing_and_line_keys_work_through_the_editor() {
        let env = Env::new();
        let mut c = composer();
        c.set_draft("a e\u{301}👩‍💻界");
        press(&mut c, &env, KeyCode::Left);
        press(&mut c, &env, KeyCode::Backspace);
        assert_eq!(c.draft(), "a e\u{301}界");
        press(&mut c, &env, KeyCode::Backspace);
        assert_eq!(c.draft(), "a 界");
        press(&mut c, &env, KeyCode::Delete);
        assert_eq!(c.draft(), "a ");
        c.set_draft("界a\n1234");
        press(&mut c, &env, KeyCode::Left);
        press(&mut c, &env, KeyCode::Up);
        press(&mut c, &env, KeyCode::Char('!'));
        assert_eq!(
            c.draft(),
            "界a!\n1234",
            "Up moves between lines before it browses history"
        );
        ctrl(&mut c, &env, 'a');
        ctrl(&mut c, &env, 'k');
        assert_eq!(c.draft(), "\n1234");
        c.set_draft("one two  ");
        ctrl(&mut c, &env, 'w');
        assert_eq!(c.draft(), "one ");
        c.set_draft("abc");
        ctrl(&mut c, &env, 'a');
        ctrl(&mut c, &env, 'd');
        assert_eq!(
            c.draft(),
            "bc",
            "Ctrl-D in a non-empty draft deletes forward"
        );
    }

    #[test]
    fn word_movement_jumps_over_words() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        c.set_draft("alpha  beta gamma");
        key(&mut c, &env, KeyCode::Left, KeyModifiers::ALT);
        typed(&mut c, &env, "|");
        assert_eq!(c.draft(), "alpha  beta |gamma");
        key(&mut c, &env, KeyCode::Left, KeyModifiers::CONTROL);
        key(&mut c, &env, KeyCode::Left, KeyModifiers::CONTROL);
        typed(&mut c, &env, "<");
        assert_eq!(c.draft(), "alpha  <beta |gamma");
        key(&mut c, &env, KeyCode::Right, KeyModifiers::ALT);
        typed(&mut c, &env, ">");
        assert_eq!(c.draft(), "alpha  <beta> |gamma");
    }

    #[test]
    fn key_release_events_are_ignored() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        let mut release = event(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(c.handle_key(release, &env.ctx()), ComposerEvent::None);
        assert_eq!(c.draft(), "");
        c.set_draft("text");
        let mut release = event(KeyCode::Enter, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(c.handle_key(release, &env.ctx()), ComposerEvent::None);
        assert_eq!(c.draft(), "text", "a release of Enter must not submit");
    }

    #[test]
    fn newline_keys_insert_and_enter_submits_without_trimming_or_sending_blanks() {
        let env = Env::new();
        let mut c = composer();
        typed(&mut c, &env, "one");
        key(&mut c, &env, KeyCode::Enter, KeyModifiers::SHIFT);
        typed(&mut c, &env, "two");
        key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
        ctrl(&mut c, &env, 'j');
        typed(&mut c, &env, "three");
        assert_eq!(c.draft(), "one\ntwo\n\nthree");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("one\ntwo\n\nthree".into())
        );
        assert_eq!(c.draft(), "");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::None,
            "an empty draft submits nothing"
        );
        typed(&mut c, &env, "   ");
        assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
        assert_eq!(c.draft(), "   ", "whitespace is not discarded");
        assert_eq!(c.history().last().unwrap(), "one\ntwo\n\nthree");
    }

    #[test]
    fn busy_queue_distinguishes_steering_followups_and_survives_interrupt() {
        let env = Env::new();
        let mut c = composer();
        c.set_busy(true);
        typed(&mut c, &env, "steer");
        press(&mut c, &env, KeyCode::Enter);
        typed(&mut c, &env, "later");
        key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::Interrupt);
        assert_eq!(c.take_steering(), vec!["steer"]);
        assert_eq!(c.take_follow_up().as_deref(), Some("later"));
        assert_eq!(c.take_follow_up(), None);
        typed(&mut c, &env, "cancel this");
        press(&mut c, &env, KeyCode::Enter);
        press(&mut c, &env, KeyCode::Up);
        assert_eq!(c.draft(), "cancel this");
        press(&mut c, &env, KeyCode::Esc);
        assert!(c.take_steering().is_empty());
        assert_eq!(c.draft(), "");
    }

    #[test]
    fn busy_slash_commands_bypass_both_message_queues() {
        let env = Env::new();
        for modifiers in [KeyModifiers::NONE, KeyModifiers::ALT] {
            for command in ["/help", "/model example", "  /unknown"] {
                let mut c = composer();
                c.set_busy(true);
                c.set_draft("existing follow-up");
                key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
                c.set_draft(command);
                assert_eq!(
                    key(&mut c, &env, KeyCode::Enter, modifiers),
                    ComposerEvent::Submit(command.into())
                );
                assert_eq!(c.draft(), "");
                assert!(c.take_steering().is_empty());
                assert_eq!(c.take_follow_up().as_deref(), Some("existing follow-up"));
                assert_eq!(c.take_follow_up(), None);
            }
        }
    }

    #[test]
    fn ctrl_c_preserves_busy_draft_and_arms_exit_only_when_idle() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        typed(&mut c, &env, "draft");
        assert_eq!(ctrl(&mut c, &env, 'c'), ComposerEvent::None);
        assert_eq!(c.draft(), "", "Ctrl-C clears a non-empty draft");
        assert!(
            !screen(&c, &env, 100)
                .last()
                .unwrap()
                .contains("again to exit")
        );
        assert_eq!(
            ctrl(&mut c, &env, 'c'),
            ComposerEvent::None,
            "first Ctrl-C on an empty idle draft only arms exit"
        );
        assert!(
            screen(&c, &env, 100)
                .last()
                .unwrap()
                .contains("again to exit")
        );
        assert_eq!(ctrl(&mut c, &env, 'c'), ComposerEvent::Exit);

        typed(&mut c, &env, "x");
        ctrl(&mut c, &env, 'c');
        ctrl(&mut c, &env, 'c');
        typed(&mut c, &env, "y");
        assert_eq!(
            ctrl(&mut c, &env, 'c'),
            ComposerEvent::None,
            "typing disarms: this Ctrl-C clears the draft"
        );
        assert_eq!(
            ctrl(&mut c, &env, 'c'),
            ComposerEvent::None,
            "and the next one arms again rather than exiting"
        );

        c.set_busy(true);
        typed(&mut c, &env, "busy draft");
        assert_eq!(ctrl(&mut c, &env, 'c'), ComposerEvent::Interrupt);
        assert_eq!(c.draft(), "busy draft");
    }

    #[test]
    fn ctrl_d_exits_only_on_an_empty_idle_draft_and_esc_is_inert_when_idle() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::None);
        assert_eq!(ctrl(&mut c, &env, 'd'), ComposerEvent::Exit);
        c.set_busy(true);
        assert_eq!(
            ctrl(&mut c, &env, 'd'),
            ComposerEvent::None,
            "never exits in the middle of a turn"
        );
        assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::Interrupt);
        c.set_busy(false);
        c.set_draft("keep");
        assert_ne!(ctrl(&mut c, &env, 'd'), ComposerEvent::Exit);
    }

    // ---- history ----------------------------------------------------------------------------

    #[test]
    fn history_browsing_restores_the_unsent_draft_and_discards_browsed_edits() {
        let env = Env::new();
        let mut c = composer();
        c.set_draft("unsent draft");
        press(&mut c, &env, KeyCode::Left);
        press(&mut c, &env, KeyCode::Up);
        assert_eq!(c.draft(), "recent prompt");
        press(&mut c, &env, KeyCode::Char('!'));
        press(&mut c, &env, KeyCode::Up);
        assert_eq!(c.draft(), "older prompt");
        press(&mut c, &env, KeyCode::Up);
        assert_eq!(c.draft(), "older prompt", "stops at the oldest entry");
        press(&mut c, &env, KeyCode::Down);
        assert_eq!(
            c.draft(),
            "recent prompt",
            "the edit made while browsing is not kept"
        );
        press(&mut c, &env, KeyCode::Down);
        press(&mut c, &env, KeyCode::Char('!'));
        assert_eq!(c.draft(), "unsent draf!t", "draft and cursor come back");
        assert_eq!(c.history(), ["older prompt", "recent prompt"]);
        c.clear();
        assert_eq!(press(&mut c, &env, KeyCode::Down), ComposerEvent::None);
        assert_eq!(c.draft(), "", "Down with nothing to browse does nothing");
    }

    #[test]
    fn browsing_a_multiline_entry_moves_within_it_before_leaving_it() {
        let env = Env::new();
        let mut c = Composer::new(vec!["first".into(), "a\nb".into()], 10);
        press(&mut c, &env, KeyCode::Up);
        assert_eq!(c.draft(), "a\nb");
        press(&mut c, &env, KeyCode::Up);
        assert_eq!(c.draft(), "a\nb", "moved to line 1 of the entry");
        press(&mut c, &env, KeyCode::Up);
        assert_eq!(c.draft(), "first");
    }

    #[test]
    fn history_is_bounded_deduplicated_sanitized_and_only_holds_submissions() {
        let env = Env::new();
        let mut c = Composer::new((0..10).map(|n| format!("entry {n}")).collect(), 3);
        assert_eq!(c.history(), ["entry 7", "entry 8", "entry 9"]);
        c.push_history("entry 9".into());
        c.push_history("   ".into());
        c.push_history("bad\x1b[31m!".into());
        assert_eq!(c.history(), ["entry 8", "entry 9", "bad!"]);
        c.handle_paste("pasted");
        assert_eq!(c.history().len(), 3, "paste never records history");
        let mut off = Composer::new(vec!["x".into()], 0);
        typed(&mut off, &env, "y");
        press(&mut off, &env, KeyCode::Enter);
        assert!(off.history().is_empty(), "limit 0 disables history");
    }

    #[test]
    fn ctrl_r_searches_history_incrementally_below_the_box() {
        let env = Env::new();
        let mut c = Composer::new(
            vec![
                "git status".into(),
                "cargo test".into(),
                "git commit".into(),
                "ls".into(),
            ],
            10,
        );
        c.set_draft("do not lose me");
        ctrl(&mut c, &env, 'r');
        let view = c.view(60, &env.ctx());
        let rows = lines(&view.rows);
        let search_row = rows
            .iter()
            .position(|r| r.starts_with("search"))
            .expect("prompt row below the box");
        assert!(
            search_row > 1
                && rows[search_row + 1].contains("ls")
                && rows[search_row + 2].contains("git commit"),
            "newest first: {rows:?}"
        );
        assert_eq!(
            view.cursor,
            (search_row as u16, 9),
            "the cursor sits in the search prompt"
        );
        typed(&mut c, &env, "git");
        let rows = screen(&c, &env, 60);
        assert!(
            rows.iter().any(|r| r.contains("git commit"))
                && rows.iter().any(|r| r.contains("git status"))
                && !rows.iter().any(|r| r.contains("cargo test")),
            "{rows:?}"
        );
        press(&mut c, &env, KeyCode::Down);
        assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
        assert_eq!(
            c.draft(),
            "git status",
            "Enter loads the selected entry without sending it"
        );
        assert_eq!(c.history().len(), 4);
        assert!(
            !screen(&c, &env, 60).iter().any(|r| r.starts_with("search")),
            "search closes"
        );
    }

    #[test]
    fn search_cancel_keeps_the_draft_and_ctrl_r_again_selects_the_next_match() {
        let env = Env::new();
        let mut c = Composer::new(vec!["alpha one".into(), "alpha two".into()], 10);
        c.set_draft("my draft");
        ctrl(&mut c, &env, 'r');
        typed(&mut c, &env, "alpha");
        ctrl(&mut c, &env, 'r');
        press(&mut c, &env, KeyCode::Enter);
        assert_eq!(
            c.draft(),
            "alpha one",
            "Ctrl-R again moved to the older match"
        );
        c.set_draft("my draft");
        ctrl(&mut c, &env, 'r');
        c.handle_paste("alpha\nx");
        let rows = screen(&c, &env, 60);
        assert!(
            rows.iter()
                .any(|r| r.starts_with("search") && r.contains("alpha x")),
            "paste is a single line in the query: {rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains("No matching history")));
        assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
        assert_eq!(c.draft(), "my draft", "Enter with no match changes nothing");
        ctrl(&mut c, &env, 'r');
        typed(&mut c, &env, "zzz");
        press(&mut c, &env, KeyCode::Esc);
        assert_eq!(c.draft(), "my draft");
        assert!(!screen(&c, &env, 60).iter().any(|r| r.starts_with("search")));
        assert_eq!(
            press(&mut c, &env, KeyCode::Esc),
            ComposerEvent::None,
            "closing the search consumed Esc"
        );
    }

    // ---- ghost suggestions ------------------------------------------------------------------

    #[test]
    fn command_arguments_are_dim_ghosts_without_history_and_complete_without_submitting() {
        let env = Env::new();
        let mut c = Composer::new(Vec::new(), 100);
        c.set_commands(vec![
            cmd("skills", "Manage skills", CommandKind::Builtin),
            cmd("understanding", "Knowledge", CommandKind::Builtin),
        ]);
        c.set_argument_completions(
            vec![
                "/skills sources",
                "/skills enable",
                "/skills enable review",
                "/skills enable review --user",
                "/understanding architecture",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        );
        typed(&mut c, &env, "/understanding ");
        assert_eq!(c.ghost(), Some("architecture"));
        let view = c.view(60, &env.ctx());
        let ghost = view
            .rows
            .iter()
            .flat_map(|row| &row.spans)
            .find(|span| span.content == "architecture")
            .unwrap();
        assert!(ghost.style.add_modifier.contains(Modifier::DIM));
        assert_eq!(c.draft(), "/understanding ");
        assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
        assert_eq!(c.draft(), "/understanding architecture");
        c.set_draft("/skills so");
        assert_eq!(c.ghost(), Some("urces"));
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("/skills so".into())
        );
        c.set_draft("/skills enable ");
        assert_eq!(c.ghost(), Some("review"));
        press(&mut c, &env, KeyCode::Right);
        assert_eq!(c.draft(), "/skills enable review");
        typed(&mut c, &env, " --u");
        assert_eq!(c.ghost(), Some("ser"));
        press(&mut c, &env, KeyCode::Tab);
        assert_eq!(c.draft(), "/skills enable review --user");
        for width in 1..50 {
            c.set_draft("/understanding ");
            assert!(
                screen(&c, &env, width)
                    .iter()
                    .all(|line| text::width(line) <= width as usize)
            );
        }
    }

    #[test]
    fn command_ghosts_prefer_metadata_and_respect_dismissal_cursor_and_busy_state() {
        let env = Env::new();
        let mut c = Composer::new(
            vec!["/skills stale-history".into(), "/custom remembered".into()],
            100,
        );
        c.set_argument_completions(vec![
            "/skills sources".into(),
            "/skills sources".into(),
            "/skills bad\ncommand".into(),
        ]);
        assert_eq!(c.argument_completions, vec!["/skills sources"]);
        c.set_draft("/skills ");
        assert_eq!(c.ghost(), Some("sources"));
        press(&mut c, &env, KeyCode::Esc);
        assert_eq!(c.ghost(), None);
        press(&mut c, &env, KeyCode::Tab);
        assert_eq!(c.draft(), "/skills ");
        typed(&mut c, &env, "s");
        assert_eq!(c.ghost(), Some("ources"));
        press(&mut c, &env, KeyCode::Left);
        assert_eq!(c.ghost(), None);
        press(&mut c, &env, KeyCode::End);
        c.set_busy(true);
        assert_eq!(c.ghost(), None);
        c.set_busy(false);
        assert_eq!(c.ghost(), Some("ources"));
        c.set_draft("/custom ");
        assert_eq!(
            c.ghost(),
            Some("remembered"),
            "unregistered argument choices still use history"
        );
        c.set_draft("/skills sources");
        assert_eq!(c.ghost(), None);
    }

    #[test]
    fn ghost_suggestion_is_dim_and_only_changes_the_draft_when_accepted() {
        let env = Env::new();
        let mut c = composer();
        typed(&mut c, &env, "rec");
        let view = c.view(60, &env.ctx());
        let row = &view.rows[1];
        assert!(
            lines(&view.rows)[1].contains("recent prompt"),
            "{:?}",
            lines(&view.rows)
        );
        let ghost = row
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "ent prompt")
            .expect("ghost span");
        assert!(
            ghost.style.add_modifier.contains(Modifier::DIM)
                && ghost.style.fg == env.theme.fg(Tok::Dim).fg,
            "ghost is dim"
        );
        let typed_span = row
            .spans
            .iter()
            .find(|s| s.content.contains("rec") && !s.content.contains("│"))
            .unwrap();
        assert!(!typed_span.style.add_modifier.contains(Modifier::DIM));
        assert_eq!(
            view.cursor,
            (1, 5),
            "the cursor stays after the typed text, before the ghost"
        );
        assert_eq!(c.draft(), "rec");
        press(&mut c, &env, KeyCode::Tab);
        assert_eq!(c.draft(), "recent prompt");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("recent prompt".into())
        );
    }

    #[test]
    fn right_accepts_the_ghost_enter_sends_only_typed_text_and_esc_hides_it_until_editing() {
        let env = Env::new();
        let mut c = composer();
        typed(&mut c, &env, "old");
        press(&mut c, &env, KeyCode::Right);
        assert_eq!(c.draft(), "older prompt");
        c.clear();
        typed(&mut c, &env, "old");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("old".into()),
            "the suggestion is not sent"
        );
        typed(&mut c, &env, "rec");
        assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::None);
        assert!(
            !lines(&c.view(60, &env.ctx()).rows)[1].contains("recent prompt"),
            "Esc hides the suggestion"
        );
        assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
        assert_eq!(c.draft(), "rec", "and Tab no longer accepts it");
        for _ in 0..3 {
            press(&mut c, &env, KeyCode::Backspace);
        }
        typed(&mut c, &env, "rec");
        assert!(
            lines(&c.view(60, &env.ctx()).rows)[1].contains("recent prompt"),
            "editing away and back brings it back"
        );
    }

    #[test]
    fn ghost_appears_only_at_the_end_of_an_idle_draft_and_prefers_the_newest_entry() {
        let env = Env::new();
        let mut c = Composer::new(
            vec![
                "e\u{301}clair old".into(),
                "ex newer".into(),
                "e\u{301}clair new".into(),
            ],
            10,
        );
        typed(&mut c, &env, "e");
        assert_eq!(
            c.ghost(),
            Some("x newer"),
            "never offer half of a combining sequence"
        );
        c.set_draft("e\u{301}");
        assert_eq!(c.ghost(), Some("clair new"));
        press(&mut c, &env, KeyCode::Left);
        assert_eq!(c.ghost(), None, "no suggestion mid-line");
        press(&mut c, &env, KeyCode::End);
        assert!(c.ghost().is_some());
        c.set_busy(true);
        assert_eq!(c.ghost(), None, "no suggestion while a turn runs");
        assert!(!lines(&c.view(60, &env.ctx()).rows)[1].contains("clair"));
        assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
        assert_eq!(c.draft(), "e\u{301}");
        c.set_busy(false);
        c.clear();
        assert_eq!(c.ghost(), None, "an empty draft has no suggestion");
        c.set_draft("ex newer");
        assert_eq!(c.ghost(), None, "an exact match leaves nothing to suggest");
    }

    #[test]
    fn multiline_ghost_wraps_inside_the_box_and_fits_the_width() {
        let env = Env::new();
        let mut c = Composer::new(
            vec!["start of a long multi\nline entry that goes on".into()],
            10,
        );
        typed(&mut c, &env, "start");
        for width in [8u16, 12, 30] {
            let view = c.view(width, &env.ctx());
            assert!(
                view.rows
                    .iter()
                    .all(|r| text::line_width(r) <= width as usize)
            );
            assert!((view.cursor.0 as usize) < view.rows.len());
        }
    }

    // ---- paste ------------------------------------------------------------------------------

    #[test]
    fn long_pastes_collapse_and_expand_on_submit_short_pastes_stay_literal() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        let long: String = (1..=25).map(|n| format!("line {n}\n")).collect();
        c.handle_paste("small\npaste");
        assert_eq!(
            c.draft(),
            "small\npaste",
            "short pastes are inserted as typed"
        );
        c.clear();
        typed(&mut c, &env, "before ");
        c.handle_paste(&long);
        assert_eq!(c.draft(), "before [Pasted text #1 +25 lines]");
        assert!(lines(&c.view(60, &env.ctx()).rows)[1].contains("[Pasted text #1 +25 lines]"));
        typed(&mut c, &env, " after");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit(format!("before {long} after"))
        );
        assert!(
            c.history().last().unwrap().contains("line 25"),
            "history keeps what was actually sent"
        );
        c.handle_paste(&"x".repeat(2001));
        assert_eq!(
            c.draft(),
            "[Pasted text #2 +1 lines]",
            "very long single lines collapse as well"
        );
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("x".repeat(2001))
        );
        c.handle_paste(&"y".repeat(2000));
        assert!(
            c.draft().starts_with("yyy"),
            "exactly at the limit stays literal"
        );
        c.handle_paste(&"z\n".repeat(20));
        assert!(
            !c.draft().contains("Pasted"),
            "20 lines is not over the limit"
        );
    }

    #[test]
    fn an_edited_placeholder_stays_literal_and_pasted_lookalikes_are_not_expanded() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        let long = "a\n".repeat(30);
        c.handle_paste(&long);
        press(&mut c, &env, KeyCode::Backspace);
        typed(&mut c, &env, " ");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("[Pasted text #1 +30 lines ".into()),
            "edited placeholder is sent as typed"
        );

        c.handle_paste(&long);
        let first = c.draft();
        c.handle_paste(&format!("{}\n{first}", "b\n".repeat(25)));
        assert!(c.draft().contains("#2"), "{}", c.draft());
        let sent = match press(&mut c, &env, KeyCode::Enter) {
            ComposerEvent::Submit(t) => t,
            other => panic!("{other:?}"),
        };
        assert!(
            sent.starts_with(&long) && sent.contains(&"b\n".repeat(25)),
            "both placeholders expand"
        );
        assert!(
            sent.ends_with(&first),
            "text inside a paste that looks like a placeholder stays literal: {sent:?}"
        );
        // Placeholders do not leak into the next draft.
        typed(&mut c, &env, "[Pasted text #1 +30 lines]");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("[Pasted text #1 +30 lines]".into())
        );
    }

    #[test]
    fn paste_never_submits_and_is_sanitized() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        c.handle_paste("one\r\ntwo\rthree\x1b[31m\x07");
        assert_eq!(c.draft(), "one\ntwo\nthree");
        key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(c.history().len(), 0);
    }

    // ---- slash menu -------------------------------------------------------------------------

    #[test]
    fn slash_opens_a_menu_below_the_box_that_filters_fuzzily_as_you_type() {
        let env = Env::new();
        let mut c = with_commands();
        assert!(!c.menu_open());
        typed(&mut c, &env, "/");
        assert!(c.menu_open());
        let rows = screen(&c, &env, 80);
        let bottom = rows.iter().position(|r| r.starts_with('╰')).unwrap();
        assert!(
            rows.len() > bottom + 1,
            "menu rows come after the box: {rows:?}"
        );
        for name in ["help", "model", "settings"] {
            assert!(
                rows[bottom + 1..].iter().any(|r| r.contains(name)),
                "{name}: {rows:?}"
            );
        }
        assert!(
            rows[bottom + 1].contains("help") && rows[bottom + 1].contains("Show help"),
            "name and description on a row: {:?}",
            rows[bottom + 1]
        );
        typed(&mut c, &env, "mo");
        let names: Vec<_> = screen(&c, &env, 80)[bottom + 1..]
            .iter()
            .map(|r| r.trim().to_owned())
            .collect();
        assert!(
            names[0].contains("model") && !names[0].contains("modelpreset"),
            "the exact-prefix match leads: {names:?}"
        );
        for expected in ["modelpreset", "memory", "remove-dir"] {
            assert!(
                names.iter().any(|r| r.contains(expected)),
                "/mo finds {expected}: {names:?}"
            );
        }
        assert!(
            !names
                .iter()
                .any(|r| r.contains("help") || r.contains("settings")),
            "{names:?}"
        );
        press(&mut c, &env, KeyCode::Backspace);
        press(&mut c, &env, KeyCode::Backspace);
        press(&mut c, &env, KeyCode::Backspace);
        assert!(!c.menu_open(), "deleting the slash closes the menu");
        typed(&mut c, &env, "/zzz");
        assert!(!c.menu_open(), "no match, no selectable menu");
        assert_eq!(screen(&c, &env, 80).len(), 4);
        assert!(screen(&c, &env, 80)[3].contains("No matching commands"));
    }

    #[test]
    fn palette_prioritizes_prefix_then_word_start_and_matches_descriptions() {
        let entries = vec![
            cmd("memory", "", CommandKind::Builtin),
            cmd("set-model", "", CommandKind::Builtin),
            cmd("model", "", CommandKind::Builtin),
            cmd("choose", "Switch provider", CommandKind::Builtin),
        ];
        let names: Vec<_> = rank(&entries, "mo")
            .iter()
            .map(|m| entries[m.index].name.as_str())
            .collect();
        assert_eq!(names, ["model", "set-model", "memory"]);
        assert_eq!(rank(&entries, "provider")[0].index, 3);
    }

    #[test]
    fn file_completion_fuzzy_matches_and_preserves_surrounding_draft() {
        let env = Env::new();
        let mut c = composer();
        c.set_files(vec!["src/storage/session.rs".into(), "src/app.rs".into()]);
        typed(&mut c, &env, "inspect @ssess");
        assert!(c.menu_open());
        press(&mut c, &env, KeyCode::Tab);
        assert_eq!(c.draft(), "inspect @src/storage/session.rs ");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("inspect @src/storage/session.rs ".into())
        );
    }

    #[test]
    fn menu_rows_have_icons_by_kind_a_pointer_and_a_selected_background() {
        let mut env = Env::new();
        env.ui.autocomplete_items = 12;
        let mut c = with_commands();
        typed(&mut c, &env, "/");
        let view = c.view(80, &env.ctx());
        let rows = lines(&view.rows);
        let menu_start = 3;
        let row_of = |name: &str| {
            rows[menu_start..]
                .iter()
                .position(|r| r.contains(name))
                .map(|i| i + menu_start)
                .unwrap()
        };
        let t = &env.theme;
        assert!(rows[row_of("help")].contains(&format!("{} help", t.sym(Sym::IconCommand))));
        assert!(
            rows[row_of("skill:review")]
                .contains(&format!("{} skill:review", t.sym(Sym::IconSkill)))
        );
        assert!(rows[row_of("deploy")].contains(&format!("{} deploy", t.sym(Sym::IconSettings))));
        assert!(
            rows[menu_start].starts_with(t.sym(Sym::Pointer))
                && !rows[menu_start + 1].starts_with(t.sym(Sym::Pointer)),
            "{rows:?}"
        );
        let sel_bg = t.bg(Tok::SelectedBg).bg;
        assert!(sel_bg.is_some());
        assert!(
            view.rows[menu_start]
                .spans
                .iter()
                .all(|s| s.style.bg == sel_bg),
            "the whole selected row is highlighted"
        );
        assert!(
            view.rows[menu_start + 1]
                .spans
                .iter()
                .all(|s| s.style.bg.is_none())
        );
        assert_eq!(
            text::line_width(&view.rows[menu_start]),
            80,
            "the selected row spans the width"
        );
        let matched = {
            typed(&mut c, &env, "se");
            c.view(80, &env.ctx()).rows[menu_start].clone()
        };
        let hit = matched
            .spans
            .iter()
            .find(|s| s.style.add_modifier.contains(Modifier::BOLD) && s.content.contains('s'))
            .expect("matched characters are emphasized");
        assert_eq!(hit.style.fg, env.theme.fg(Tok::Accent).fg);
        let mut ascii = Env::new();
        ascii.theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
        let mut c = with_commands();
        typed(&mut c, &ascii, "/");
        let rows = screen(&c, &ascii, 80);
        assert!(
            rows[3].starts_with("> help") && rows.iter().all(|r| r.is_ascii()),
            "ascii pointer and no icons: {rows:?}"
        );
    }

    #[test]
    fn args_hints_show_after_the_name_and_long_descriptions_are_ellipsized() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        c.set_commands(vec![
            CommandEntry {
                name: "model".into(),
                description: "Switch the active model to something else entirely, now".into(),
                kind: CommandKind::Builtin,
                args_hint: Some("<name>".into()),
            },
            cmd("help", "Show help", CommandKind::Builtin),
        ]);
        typed(&mut c, &env, "/");
        let rows = screen(&c, &env, 40);
        assert!(
            rows[3].contains("model  <name>  Switch") && rows[3].contains('…'),
            "{:?}",
            rows[3]
        );
        assert!(rows.iter().all(|r| text::width(r) <= 40));
        for width in [8u16, 9, 14, 20] {
            let view = c.view(width, &env.ctx());
            assert!(
                view.rows
                    .iter()
                    .all(|r| text::line_width(r) <= width as usize),
                "width {width}: {:?}",
                lines(&view.rows)
            );
        }
    }

    #[test]
    fn enter_completes_a_partial_name_and_submits_a_complete_one() {
        let env = Env::new();
        let mut c = with_commands();
        typed(&mut c, &env, "/sett");
        assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
        assert_eq!(
            c.draft(),
            "/settings ",
            "a partial name is completed so arguments can follow"
        );
        assert!(!c.menu_open());
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("/settings ".into())
        );
        typed(&mut c, &env, "/help");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("/help".into()),
            "an exact name runs"
        );
        typed(&mut c, &env, "/model");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("/model".into()),
            "exact beats the longer modelpreset"
        );
        typed(&mut c, &env, "/nope");
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("/nope".into()),
            "unknown commands go to the controller"
        );
    }

    #[test]
    fn menu_navigation_wraps_and_tab_or_right_completes_the_selection() {
        let env = Env::new();
        let mut c = with_commands();
        typed(&mut c, &env, "/se");
        press(&mut c, &env, KeyCode::Down);
        press(&mut c, &env, KeyCode::Tab);
        let second = c.draft();
        assert!(
            second.starts_with('/') && second.ends_with(' ') && second != "/se ",
            "{second:?}"
        );
        c.set_draft("/se");
        let first = {
            press(&mut c, &env, KeyCode::Tab);
            c.draft()
        };
        c.set_draft("/se");
        press(&mut c, &env, KeyCode::Up);
        press(&mut c, &env, KeyCode::Right);
        let last = c.draft();
        assert_ne!(first, last, "Up from the first row wraps to the last");
        assert_ne!(first, second);
        c.set_draft("/mod");
        ctrl(&mut c, &env, 'n');
        press(&mut c, &env, KeyCode::Tab);
        assert_eq!(
            c.draft(),
            "/modelpreset ",
            "Ctrl-N is the keymap's SelectDown"
        );
    }

    #[test]
    fn long_menus_show_autocomplete_items_rows_with_a_scrollbar_and_follow_the_selection() {
        let mut env = Env::new();
        env.ui.autocomplete_items = 5;
        let mut c = Composer::new(vec![], 10);
        c.set_commands(
            (0..20)
                .map(|n| {
                    cmd(
                        &format!("cmd{n:02}"),
                        &format!("number {n}"),
                        CommandKind::Builtin,
                    )
                })
                .collect(),
        );
        typed(&mut c, &env, "/");
        let t = &env.theme;
        let menu = |c: &Composer| screen(c, &env, 60)[3..].to_vec();
        let rows = menu(&c);
        assert_eq!(rows.len(), 5);
        assert!(
            rows[0].contains("cmd00")
                && rows[4].contains("cmd04")
                && !rows.iter().any(|r| r.contains("cmd05"))
        );
        assert!(
            rows[4].contains("1/20"),
            "counter on the last row: {rows:?}"
        );
        assert!(rows.iter().all(|r| text::width(r) == 60), "{rows:?}");
        assert!(
            rows[0].ends_with(t.sym(Sym::ScrollThumb))
                && rows[1..]
                    .iter()
                    .all(|r| r.ends_with(t.sym(Sym::ScrollTrack))),
            "thumb at the top, track below: {rows:?}"
        );
        for _ in 0..5 {
            press(&mut c, &env, KeyCode::Down);
        }
        let rows = menu(&c);
        assert!(
            rows[0].contains("cmd01") && rows[4].contains("cmd05") && rows[4].contains("6/20"),
            "window follows the selection: {rows:?}"
        );
        assert!(
            rows[4].starts_with(t.sym(Sym::Pointer)),
            "the selection is on the last visible row: {rows:?}"
        );
        press(&mut c, &env, KeyCode::Up);
        assert!(
            menu(&c)[3].starts_with(t.sym(Sym::Pointer)) && menu(&c)[0].contains("cmd01"),
            "moving up inside the window does not scroll"
        );
        for _ in 0..4 {
            press(&mut c, &env, KeyCode::Up);
        }
        assert!(menu(&c)[0].starts_with(t.sym(Sym::Pointer)) && menu(&c)[0].contains("cmd00"));
        press(&mut c, &env, KeyCode::Up);
        let rows = menu(&c);
        assert!(
            rows[4].contains("cmd19")
                && rows[4].contains("20/20")
                && rows[4].starts_with(t.sym(Sym::Pointer)),
            "wrapping to the end scrolls it into view: {rows:?}"
        );
        assert!(
            rows[4].ends_with(t.sym(Sym::ScrollThumb)),
            "thumb at the bottom: {rows:?}"
        );
        key(&mut c, &env, KeyCode::PageUp, KeyModifiers::NONE);
        assert!(
            menu(&c)
                .iter()
                .any(|r| r.starts_with(t.sym(Sym::Pointer)) && r.contains("cmd14")),
            "PageUp moves by a page: {:?}",
            menu(&c)
        );
        typed(&mut c, &env, "1");
        let rows = menu(&c);
        assert_eq!(
            rows.len(),
            5,
            "cmd10..cmd19 and more: still limited to five rows"
        );
        assert!(
            rows[0].contains("cmd1") && rows[0].starts_with(t.sym(Sym::Pointer)),
            "typing resets the selection to the top: {rows:?}"
        );
    }

    #[test]
    fn short_menus_have_no_scrollbar_or_counter() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        c.set_commands(vec![
            cmd("a", "first", CommandKind::Builtin),
            cmd("b", "second", CommandKind::Builtin),
        ]);
        typed(&mut c, &env, "/");
        let rows = screen(&c, &env, 40);
        assert_eq!(rows.len(), 5);
        assert!(
            !rows[3..]
                .iter()
                .any(|r| r.contains("/2") || r.contains(env.theme.sym(Sym::ScrollTrack))),
            "{rows:?}"
        );
    }

    #[test]
    fn esc_dismisses_the_menu_until_the_text_changes() {
        let env = Env::new();
        let mut c = with_commands();
        typed(&mut c, &env, "/se");
        assert!(c.menu_open());
        assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::None);
        assert!(!c.menu_open() && c.draft() == "/se");
        assert_eq!(screen(&c, &env, 80).len(), 3, "no menu rows");
        typed(&mut c, &env, "t");
        assert!(c.menu_open(), "typing reopens the menu");
        press(&mut c, &env, KeyCode::Esc);
        press(&mut c, &env, KeyCode::Backspace);
        assert!(
            c.menu_open(),
            "any change reopens it, even back to the dismissed prefix"
        );
        press(&mut c, &env, KeyCode::Esc);
        typed(&mut c, &env, "t");
        assert!(c.menu_open());
        c.set_busy(true);
        assert_eq!(
            press(&mut c, &env, KeyCode::Esc),
            ComposerEvent::None,
            "Esc closes the menu before it interrupts"
        );
        assert!(!c.menu_open());
        assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::Interrupt);
    }

    #[test]
    fn the_menu_applies_only_to_a_single_slash_word_at_the_end_with_commands_registered() {
        let env = Env::new();
        let mut plain = composer();
        typed(&mut plain, &env, "/mo");
        assert!(!plain.menu_open(), "no commands registered, no menu");
        let mut c = with_commands();
        typed(&mut c, &env, "/model ");
        assert!(!c.menu_open(), "a space ends command completion");
        typed(&mut c, &env, "gpt");
        assert!(!c.menu_open());
        c.set_draft("not /a command");
        assert!(!c.menu_open());
        c.set_draft("/mo\nmore");
        assert!(!c.menu_open(), "a newline counts as whitespace");
        c.set_draft("/mo");
        assert!(c.menu_open());
        press(&mut c, &env, KeyCode::Left);
        assert!(!c.menu_open(), "the cursor must be at the end");
        press(&mut c, &env, KeyCode::End);
        assert!(c.menu_open());
        ctrl(&mut c, &env, 'r');
        assert!(!c.menu_open(), "history search replaces the menu");
        press(&mut c, &env, KeyCode::Esc);
        c.set_commands(vec![]);
        assert!(!c.menu_open(), "clearing the commands closes it");
    }

    #[test]
    fn registered_commands_are_sanitized_and_untrusted_names_rejected() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        c.set_commands(vec![
            cmd(
                "review",
                "check\x1b]0;owned\x07 this\ndiff",
                CommandKind::Custom,
            ),
            cmd("review", "duplicate", CommandKind::Custom),
            cmd("has space", "bad", CommandKind::Custom),
            cmd("/slash", "bad", CommandKind::Custom),
            cmd("", "bad", CommandKind::Custom),
            cmd("tab\tname", "bad", CommandKind::Custom),
        ]);
        typed(&mut c, &env, "/");
        let rows = screen(&c, &env, 80);
        assert_eq!(rows.len(), 4, "only one command survives: {rows:?}");
        assert!(
            rows[3].contains("review")
                && rows[3].contains("check this diff")
                && !rows[3].contains("owned")
                && !rows[3].contains('\x1b'),
            "{:?}",
            rows[3]
        );
    }

    #[test]
    fn menu_and_search_rows_never_exceed_any_width() {
        let env = Env::new();
        let mut c = with_commands();
        typed(&mut c, &env, "/");
        for width in 0..=60u16 {
            let view = c.view(width, &env.ctx());
            for row in &view.rows {
                assert!(
                    text::line_width(row) <= width as usize,
                    "menu, width {width}: {:?}",
                    lines(&view.rows)
                );
            }
        }
        c.clear();
        c.push_history("a long history entry 日本語日本語 with wide characters in it".into());
        ctrl(&mut c, &env, 'r');
        typed(&mut c, &env, "wide");
        for width in 0..=60u16 {
            let view = c.view(width, &env.ctx());
            for row in &view.rows {
                assert!(
                    text::line_width(row) <= width as usize,
                    "search, width {width}: {:?}",
                    lines(&view.rows)
                );
            }
            assert!(view.rows.is_empty() || (view.cursor.0 as usize) < view.rows.len());
        }
    }

    #[test]
    fn untrusted_draft_text_cannot_inject_terminal_sequences() {
        let env = Env::new();
        let mut c = Composer::new(vec![], 10);
        c.set_draft("safe\x1b]52;c;x\x07\x1b[31mred\u{202e}end");
        for row in screen(&c, &env, 60) {
            assert!(
                !row.contains('\x1b') && !row.contains('\u{202e}') && !row.contains('\x07'),
                "{row:?}"
            );
        }
        assert_eq!(c.draft(), "saferedend");
    }

    #[test]
    fn spinner_in_the_status_border_animates_while_busy() {
        let mut env = Env::new();
        let mut c = Composer::new(vec![], 10);
        c.set_busy(true);
        env.status.phase = Phase::Working;
        env.status.elapsed_secs = 7.0;
        env.status.tick = 0;
        let a = screen(&c, &env, 100)[0].clone();
        env.status.tick = 1;
        let b = screen(&c, &env, 100)[0].clone();
        assert!(a.contains("7s") && a != b, "{a:?} {b:?}");
        assert!(a.contains(env.theme.spinner(0)) && b.contains(env.theme.spinner(1)));
    }
}
