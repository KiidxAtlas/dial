//! Resume catalogue and terminal presentation. The controller alone opens journals or persists pins.
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

use super::{
    Rows, chrome, text,
    theme::{Sym, Theme, Tok},
};
use crate::storage::session::SessionInfo;

/// Wide layouts put the selected session's preview beside the list.
const PREVIEW_MIN_COLS: usize = 100;
const PREVIEW_MIN_ROWS: usize = 18;
/// Turns kept at each end of the preview.
const PREVIEW_TURNS: usize = 2;

pub enum Action {
    None,
    Cancel,
    Resume(usize),
    Pin(usize),
    /// Index of the session and its new title.
    Rename(usize, String),
    Fork(usize),
    Delete(usize),
    /// Toggle between this project and every project.
    Scope,
    /// Load every journal, not just the first page.
    LoadAll,
}

/// A list row: a non-selectable date group header, or a session by its position in `found`.
#[derive(Debug, PartialEq)]
enum Unit {
    Header(&'static str),
    Item(usize),
}

/// What a picker key asks of the selected session; [`Picker::blocked`] decides whether it may run.
#[derive(Clone, Copy)]
enum Need {
    Pin,
    Fork,
    Rename,
    Delete,
    Resume,
}

pub struct Picker {
    pub sessions: Vec<SessionInfo>,
    /// Fork depth of each session (tree mode), aligned with `sessions`.
    depth: Vec<usize>,
    pinned: HashSet<String>,
    current: String,
    tree: bool,
    show_empty: bool,
    all_projects: bool,
    /// Journals in scope; `sessions.len()` is how many are loaded ("showing N of M").
    total: usize,
    open_elsewhere: HashSet<String>,
    query: String,
    /// Session id and draft title while renaming.
    rename: Option<(String, String)>,
    notice: Option<String>,
    preview: HashMap<String, Vec<(String, String)>>,
    selected: usize,
    found: Vec<usize>,
    searchable: Vec<String>,
}

impl Picker {
    pub fn new(
        sessions: Vec<SessionInfo>,
        pinned: Vec<String>,
        current: &str,
        query: &str,
        tree: bool,
    ) -> Self {
        let (sessions, depth) = if tree {
            let order = tree_order(&sessions);
            let depth = order.iter().map(|&(_, depth)| depth).collect();
            let mut slots: Vec<Option<SessionInfo>> = sessions.into_iter().map(Some).collect();
            let sessions = order
                .iter()
                .map(|&(index, _)| {
                    slots[index]
                        .take()
                        .expect("tree order visits each session once")
                })
                .collect();
            (sessions, depth)
        } else {
            let depth = vec![0; sessions.len()];
            (sessions, depth)
        };
        let searchable = sessions.iter().map(search_text).collect();
        let mut picker = Self {
            total: sessions.len(),
            sessions,
            depth,
            pinned: pinned.into_iter().collect(),
            current: current.into(),
            tree,
            show_empty: tree,
            all_projects: false,
            open_elsewhere: HashSet::new(),
            query: text::one_line(query),
            rename: None,
            notice: None,
            preview: HashMap::new(),
            selected: 0,
            found: Vec::new(),
            searchable,
        };
        picker.refilter(None);
        picker
    }

    /// Sets the scope label and the journal count `total` behind "showing N of M".
    pub fn scope(mut self, all_projects: bool, total: usize) -> Self {
        self.all_projects = all_projects;
        self.total = total.max(self.sessions.len());
        self
    }

    /// Sessions another dial process holds open. They stay listed but cannot be resumed.
    pub fn with_open_elsewhere(mut self, ids: HashSet<String>) -> Self {
        self.open_elsewhere = ids;
        self
    }

    /// Keeps the empty-session filter across a reload.
    pub fn with_show_empty(mut self, show_empty: bool) -> Self {
        self.show_empty = show_empty || self.tree;
        self.refilter(None);
        self
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn tree(&self) -> bool {
        self.tree
    }

    pub fn show_empty(&self) -> bool {
        self.show_empty
    }

    pub fn all_projects(&self) -> bool {
        self.all_projects
    }

    /// Id of the highlighted session, used to keep the selection across a reload.
    pub fn selected_id(&self) -> Option<&str> {
        self.found
            .get(self.selected)
            .map(|&i| self.sessions[i].id.as_str())
    }

    /// Shows an error from the last action; it clears on the next key press.
    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(text::one_line(&notice.into()));
    }

    /// The selected session whose preview is still needed (wide layouts only).
    pub fn wanted_preview(&self, cols: usize, height: usize) -> Option<(String, PathBuf)> {
        if !wide(cols, height) {
            return None;
        }
        let &index = self.found.get(self.selected)?;
        let session = &self.sessions[index];
        (!self.preview.contains_key(&session.id))
            .then(|| (session.id.clone(), session.path.clone()))
    }

    pub fn set_preview(&mut self, id: String, turns: Vec<(String, String)>) {
        self.preview.insert(id, turns);
    }

    fn refilter(&mut self, keep: Option<&str>) {
        let query = self
            .query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let tokens: Vec<_> = query.split_whitespace().collect();
        let mut ranked = Vec::new();
        for (index, session) in self.sessions.iter().enumerate() {
            if !self.show_empty && !session.is_conversation() && !self.pinned.contains(&session.id)
            {
                continue;
            }
            let title = text::one_line(&session.title).to_lowercase();
            let hay = &self.searchable[index];
            let rank = if tokens.is_empty() || title == query {
                0
            } else if tokens.iter().all(|token| title.contains(token)) {
                1
            } else if tokens.iter().all(|token| hay.contains(token)) {
                2
            }
            // Fuzzy matching is restricted to the title: scattered letters across a long
            // transcript or filesystem path must not make nearly every session a hit.
            else if tokens
                .iter()
                .all(|token| text::fuzzy(token, &title).is_some())
            {
                3
            } else {
                continue;
            };
            ranked.push((index, rank));
        }
        // Stable ordering preserves the catalogue's activity/pin order for equal search ranks.
        if !tokens.is_empty() && !self.tree {
            ranked.sort_by_key(|(_, rank)| *rank);
        }
        self.found = ranked.into_iter().map(|(index, _)| index).collect();
        self.selected = keep
            .and_then(|id| self.found.iter().position(|&i| self.sessions[i].id == id))
            .unwrap_or(0)
            .min(self.found.len().saturating_sub(1));
    }

    /// The selected session when `need` may run on it; otherwise the notice says why it cannot.
    fn gated(&mut self, selected: Option<usize>, need: Need) -> Option<usize> {
        let Some(index) = selected else {
            self.notice = Some("Nothing selected to act on.".into());
            return None;
        };
        match self.blocked(index, need) {
            Some(reason) => {
                self.notice = Some(reason);
                None
            }
            None => Some(index),
        }
    }

    /// Why `need` cannot run on the session at `index`; `None` when it can. Sessions another dial
    /// process holds open are never resumed, renamed or deleted from here.
    fn blocked(&self, index: usize, need: Need) -> Option<String> {
        let s = &self.sessions[index];
        let open = self.open_elsewhere.contains(&s.id);
        let name = label(s);
        match need {
            Need::Pin | Need::Fork => None,
            Need::Delete if s.id == self.current => {
                Some("Use /delete to delete the current session.".into())
            }
            Need::Resume if open => Some(format!(
                "{name} is open in another dial process, so it cannot be resumed here."
            )),
            Need::Rename | Need::Delete if open => Some(format!(
                "{name} is open in another dial process; close it there first."
            )),
            _ => None,
        }
    }

    /// Re-filters after a query edit, keeping the highlighted session when it still matches.
    fn refilter_keeping_selection(&mut self) {
        let keep = self.selected_id().map(str::to_owned);
        self.refilter(keep.as_deref());
    }

    /// Moves the highlight to the listed session with `id`, if it is still listed.
    pub fn select_id(&mut self, id: &str) {
        if let Some(position) = self.found.iter().position(|&i| self.sessions[i].id == id) {
            self.selected = position;
        }
    }

    pub fn set_pinned(&mut self, index: usize, pinned: bool) {
        let id = self.sessions[index].id.clone();
        if pinned {
            self.pinned.insert(id.clone());
        } else {
            self.pinned.remove(&id);
        }
        if !self.tree {
            // Stable partition keeps recency within the two groups. The storage catalogue already
            // carries activity order, but removing a pin must restore it too.
            self.sessions.sort_by(|a, b| {
                self.pinned
                    .contains(&b.id)
                    .cmp(&self.pinned.contains(&a.id))
                    .then_with(|| b.updated.cmp(&a.updated))
                    .then_with(|| b.id.cmp(&a.id))
            });
            self.searchable = self.sessions.iter().map(search_text).collect();
        }
        self.refilter(Some(&id));
    }

    pub fn handle(&mut self, event: &Event, height: usize) -> Action {
        let page = self.page_size(height);
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if self.rename.is_some() {
                    return self.handle_rename(key);
                }
                self.notice = None;
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let selected = self.found.get(self.selected).copied();
                match key.code {
                    // Esc clears a query first; a second press on the empty search closes.
                    KeyCode::Esc => {
                        if self.query.is_empty() {
                            return Action::Cancel;
                        }
                        self.query.clear();
                        self.refilter_keeping_selection();
                    }
                    KeyCode::Char('c') if ctrl => return Action::Cancel,
                    KeyCode::Char('p') if ctrl => {
                        if let Some(index) = self.gated(selected, Need::Pin) {
                            return Action::Pin(index);
                        }
                    }
                    KeyCode::Char('r') if ctrl => {
                        if let Some(index) = self.gated(selected, Need::Rename) {
                            let session = &self.sessions[index];
                            self.rename =
                                Some((session.id.clone(), text::one_line(&session.title)));
                        }
                    }
                    KeyCode::Char('f') if ctrl => {
                        if let Some(index) = self.gated(selected, Need::Fork) {
                            return Action::Fork(index);
                        }
                    }
                    KeyCode::Char('d') if ctrl => {
                        if let Some(index) = self.gated(selected, Need::Delete) {
                            return Action::Delete(index);
                        }
                    }
                    KeyCode::Char('g') if ctrl => return Action::Scope,
                    KeyCode::Char('a') if ctrl => {
                        if self.sessions.len() < self.total {
                            return Action::LoadAll;
                        }
                    }
                    KeyCode::Tab => {
                        let id = self.selected_id().map(str::to_owned);
                        self.show_empty = !self.show_empty;
                        self.refilter(id.as_deref());
                    }
                    KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                    KeyCode::Down => {
                        self.selected = (self.selected + 1).min(self.found.len().saturating_sub(1))
                    }
                    KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
                    KeyCode::PageDown => {
                        self.selected =
                            (self.selected + page).min(self.found.len().saturating_sub(1))
                    }
                    KeyCode::Home => self.selected = 0,
                    KeyCode::End => self.selected = self.found.len().saturating_sub(1),
                    KeyCode::Enter => {
                        if let Some(index) = self.gated(selected, Need::Resume) {
                            return Action::Resume(index);
                        }
                    }
                    KeyCode::Char('u') if ctrl => {
                        self.query.clear();
                        self.refilter_keeping_selection();
                    }
                    KeyCode::Backspace => {
                        self.query.pop();
                        self.refilter_keeping_selection();
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.query.push(c);
                        self.refilter_keeping_selection();
                    }
                    _ => {}
                }
            }
            Event::Paste(value) => {
                if let Some((_, draft)) = self.rename.as_mut() {
                    draft.push_str(&text::one_line(value));
                } else {
                    self.query.push_str(&text::one_line(value));
                    self.refilter_keeping_selection();
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Keys while renaming: the draft replaces the search line until Enter or Esc.
    fn handle_rename(&mut self, key: &KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some((id, draft)) = self.rename.as_mut() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Esc => self.rename = None,
            KeyCode::Char('c') if ctrl => self.rename = None,
            KeyCode::Enter => {
                let title = draft.trim().to_owned();
                let id = id.clone();
                self.rename = None;
                if title.is_empty() {
                    self.notice = Some("Title is empty; rename cancelled.".into());
                } else if let Some(index) = self.sessions.iter().position(|s| s.id == id) {
                    return Action::Rename(index, title);
                }
            }
            KeyCode::Char('u') if ctrl => draft.clear(),
            KeyCode::Backspace => {
                draft.pop();
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                draft.push(c)
            }
            _ => {}
        }
        Action::None
    }

    fn page_size(&self, height: usize) -> usize {
        let compact = height < 18;
        let has_selection = self.found.get(self.selected).is_some();
        let availability = self.found.get(self.selected).is_some_and(|&index| {
            let session = &self.sessions[index];
            self.open_elsewhere.contains(&session.id) || session.id == self.current
        });
        let search = usize::from(!(height == 5 && self.notice.is_some()));
        let context = usize::from(height >= 7) + usize::from(self.found.is_empty());
        let details = if !compact && has_selection {
            4 + usize::from(availability)
        } else {
            0
        };
        let footer = if compact || self.rename.is_some() {
            1
        } else {
            2
        };
        let fixed = 2 + search + context + details + footer + usize::from(self.notice.is_some());
        (height.saturating_sub(fixed) / if compact { 1 } else { 2 }).max(1)
    }

    /// The list in display order: date headers (outside tree and search views) and sessions.
    fn units(&self, now: i64) -> Vec<Unit> {
        let headers = !self.tree && self.query.trim().is_empty();
        let mut units = Vec::with_capacity(self.found.len() * 2);
        let mut last = None;
        for (position, &index) in self.found.iter().enumerate() {
            if headers {
                let session = &self.sessions[index];
                let group = if self.pinned.contains(&session.id) {
                    "Pinned"
                } else {
                    day_group(&session.updated, now)
                };
                if last != Some(group) {
                    units.push(Unit::Header(group));
                    last = Some(group);
                }
            }
            units.push(Unit::Item(position));
        }
        units
    }

    pub fn rows(&self, cols: usize, height: usize, theme: &Theme) -> Rows {
        if height == 0 || cols == 0 {
            return Vec::new();
        }
        if height < 5 || cols < 12 {
            return vec![Line::from(Span::styled(
                text::ellipsize(
                    "Resize to browse sessions · Esc cancel",
                    cols,
                    theme.ascii(),
                ),
                theme.fg(Tok::Muted),
            ))];
        }
        // Nothing to preview: the message and hints get the full width instead of a split.
        let wide = wide(cols, height) && !self.found.is_empty();
        let inner = cols.saturating_sub(4);
        // Wide layouts split the inner width: the list on the left, the preview pane on the right.
        let width = if wide { inner * 3 / 5 } else { inner };
        let pane_width = inner.saturating_sub(width + 3);
        let compact = height < 18;
        let clip = |value: &str| text::ellipsize(&text::one_line(value), width, theme.ascii());
        let line = |value: String, tok| Line::from(Span::styled(clip(&value), theme.fg(tok)));
        let (prompt, draft) = match &self.rename {
            Some((_, draft)) => ("Rename  ", draft.as_str()),
            None => ("Search  ", self.query.as_str()),
        };
        let mut body = vec![Line::from(vec![
            Span::styled(prompt, theme.fg(Tok::Accent)),
            Span::styled(
                text::ellipsize(
                    if draft.is_empty() && self.rename.is_none() {
                        "type to filter titles and messages"
                    } else {
                        draft
                    },
                    width.saturating_sub(9),
                    theme.ascii(),
                ),
                theme.fg(if draft.is_empty() && self.rename.is_none() {
                    Tok::Dim
                } else {
                    Tok::Text
                }),
            ),
            Span::styled(" ", theme.fg(Tok::Text).add_modifier(Modifier::REVERSED)),
        ])];
        // At five rows, a refusal takes priority over search: keep the selected journal,
        // its explanation, the exit hint and both frame edges visible.
        if height == 5 && self.notice.is_some() {
            body.clear();
        }
        let empty = self
            .sessions
            .iter()
            .filter(|s| !s.is_conversation() && !self.pinned.contains(&s.id))
            .count();
        let scope = if self.all_projects {
            "all projects"
        } else {
            "this project"
        };
        if height >= 7 {
            let status = if self.show_empty {
                format!("All sessions · {scope}")
            } else if empty > 0 {
                format!("Conversations · {empty} empty hidden · Tab includes them · {scope}")
            } else {
                format!("Conversations · {scope}")
            };
            body.push(line(status, Tok::Muted));
        }
        let loaded = self.sessions.len();
        if self.found.is_empty() {
            let message = if !self.query.trim().is_empty() && loaded < self.total {
                format!(
                    "No matches in the {loaded} loaded sessions. Ctrl+A searches all {}.",
                    self.total
                )
            } else if !self.query.trim().is_empty() {
                "No matches. Ctrl+U clears search; Tab includes empty sessions.".into()
            } else if empty > 0 && !self.show_empty {
                "No conversations yet. Tab shows empty sessions.".into()
            } else if !self.all_projects {
                "No sessions in this project yet. Ctrl+G lists every project.".into()
            } else {
                "No sessions yet.".into()
            };
            body.push(line(message, Tok::Muted));
        }
        let now = now_secs();
        let units = self.units(now);
        let visible = self.page_size(height);
        let selected_unit = units
            .iter()
            .position(|unit| *unit == Unit::Item(self.selected))
            .unwrap_or(0);
        let start = selected_unit.saturating_sub(visible.saturating_sub(1));
        for unit in units.iter().skip(start).take(visible) {
            match unit {
                Unit::Header(group) => body.push(Line::from(Span::styled(
                    clip(group),
                    theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                ))),
                Unit::Item(position) => self.item_rows(*position, width, compact, theme, &mut body),
            }
        }
        if !compact && let Some(&index) = self.found.get(self.selected) {
            let s = &self.sessions[index];
            body.push(line(theme.sym(Sym::BoxH).repeat(width), Tok::Border));
            let branch = s
                .parent_id
                .as_ref()
                .map(|id| format!(" · fork of {}", short(id)))
                .unwrap_or_default();
            let project = if self.all_projects {
                format!(" · {}", s.project)
            } else {
                String::new()
            };
            body.push(line(
                format!(
                    "{} request{} · {} message{} · {} · {}{}{}",
                    s.user_messages,
                    if s.user_messages == 1 { "" } else { "s" },
                    s.messages,
                    if s.messages == 1 { "" } else { "s" },
                    s.model.rsplit('/').next().unwrap_or(&s.model),
                    short(&s.id),
                    branch,
                    project
                ),
                Tok::Muted,
            ));
            let state = if self.open_elsewhere.contains(&s.id) {
                Some((
                    "Open in another dial process: resume, rename and delete are unavailable here",
                    Tok::Warning,
                ))
            } else if s.id == self.current {
                Some((
                    "Current session: Ctrl+D is unavailable; use /delete",
                    Tok::Muted,
                ))
            } else {
                None
            };
            if let Some((note, tok)) = state {
                body.push(line(note.into(), tok));
            }
            body.push(line(
                format!(
                    "You  {}",
                    if s.last_message.trim().is_empty() {
                        "No prompt yet"
                    } else {
                        &s.last_message
                    }
                ),
                Tok::Text,
            ));
            body.push(line(
                format!(
                    "Dial {}",
                    if s.last_answer.trim().is_empty() {
                        "No answer recorded"
                    } else {
                        &s.last_answer
                    }
                ),
                Tok::Muted,
            ));
        }
        if let Some(notice) = &self.notice {
            body.push(line(notice.clone(), Tok::Warning));
        }
        let position = if self.found.is_empty() {
            0
        } else {
            self.selected + 1
        };
        if self.rename.is_some() {
            body.push(line(
                "Enter saves the title · Esc cancels rename".into(),
                Tok::Muted,
            ));
        } else if self.found.is_empty() {
            let hint = if loaded < self.total {
                "Esc back · Ctrl+A load all"
            } else if self.query.is_empty() {
                "Esc close · Ctrl+G projects"
            } else {
                "Esc clear search · Ctrl+G projects"
            };
            body.push(line(hint.into(), Tok::Muted));
        } else if compact {
            body.push(line(
                format!(
                    "{position}/{} · Enter resume · Ctrl+P pin · Esc clear or close",
                    self.found.len()
                ),
                Tok::Muted,
            ));
        } else {
            let load = if loaded < self.total {
                " · Ctrl+A load all"
            } else {
                ""
            };
            body.push(line(
                format!(
                    "{position}/{} · ↑↓ move · Enter resume · Esc clear or close{load}",
                    self.found.len()
                ),
                Tok::Muted,
            ));
            body.push(line(
                "Ctrl+P pin · Ctrl+R rename · Ctrl+F fork · Ctrl+D delete · Ctrl+G scope · Tab empty · Ctrl+U clear"
                    .into(),
                Tok::Muted,
            ));
        }
        let rows = if wide {
            let pane = self.pane(pane_width, theme);
            body.into_iter()
                .enumerate()
                .map(|(i, left)| beside(left, pane.get(i), width, theme))
                .collect()
        } else {
            body
        };
        let extra = if loaded < self.total {
            format!(
                "showing {loaded} of {} · {} found",
                self.total,
                self.found.len()
            )
        } else {
            format!("{} found", self.found.len())
        };
        chrome::framed_lines(
            theme,
            chrome::Section::Dial,
            if self.tree { "Session tree" } else { "Resume" },
            &extra,
            rows,
            cols,
        )
    }

    /// One session row (and its preview line outside compact layouts).
    fn item_rows(
        &self,
        position: usize,
        width: usize,
        compact: bool,
        theme: &Theme,
        body: &mut Vec<Line<'static>>,
    ) {
        let index = self.found[position];
        let s = &self.sessions[index];
        let selected = position == self.selected;
        let clip = |value: &str, tok| {
            Line::from(Span::styled(
                text::ellipsize(&text::one_line(value), width, theme.ascii()),
                theme.fg(tok),
            ))
        };
        let pinned = self.pinned.contains(&s.id);
        let current = s.id == self.current;
        let open = self.open_elsewhere.contains(&s.id);
        let marker = if selected {
            theme.sym(Sym::Pointer)
        } else {
            " "
        };
        // Wide rows spell the markers out; narrow rows keep one-character glyphs visible.
        let flags = if width >= 44 {
            let mut flags = String::new();
            for (on, word) in [
                (pinned, "[pin] "),
                (current, "[current] "),
                (open, "[open elsewhere] "),
            ] {
                if on {
                    flags.push_str(word);
                }
            }
            flags
        } else {
            let glyphs: String = [(pinned, '*'), (current, '@'), (open, '~')]
                .into_iter()
                .filter_map(|(on, glyph)| on.then_some(glyph))
                .collect();
            if glyphs.is_empty() {
                glyphs
            } else {
                format!("{glyphs} ")
            }
        };
        let age = if width >= 28 {
            age(&s.updated)
        } else {
            String::new()
        };
        let indent = if self.tree {
            "  ".repeat(self.depth[index].min(6))
        } else {
            String::new()
        };
        let title = format!("{indent}{}", label(s));
        let room = width.saturating_sub(text::width(&flags) + text::width(&age) + 4);
        let title = text::ellipsize(&title, room, theme.ascii());
        let gap =
            width.saturating_sub(text::width(&title) + text::width(&flags) + text::width(&age) + 2);
        let fill = if selected {
            theme.bg(Tok::SelectedBg)
        } else {
            Default::default()
        };
        body.push(
            Line::from(vec![
                Span::styled(
                    format!("{marker} {flags}"),
                    theme.fg(Tok::Accent).patch(fill),
                ),
                Span::styled(
                    title,
                    theme
                        .fg(if selected { Tok::Accent } else { Tok::Text })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        })
                        .patch(fill),
                ),
                Span::styled(" ".repeat(gap), fill),
                Span::styled(age, theme.fg(Tok::Muted).patch(fill)),
            ])
            .style(fill),
        );
        if !compact {
            let preview = if !s.last_message.trim().is_empty() {
                &s.last_message
            } else {
                &s.first_message
            };
            body.push(clip(
                &format!(
                    "  {}",
                    if preview.trim().is_empty() {
                        "No conversation messages"
                    } else {
                        preview
                    }
                ),
                Tok::Muted,
            ));
        }
    }

    /// Preview pane for the selected session: its first and last turns, loaded on demand.
    fn pane(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(&index) = self.found.get(self.selected) else {
            return Vec::new();
        };
        let s = &self.sessions[index];
        let clip = |value: String, tok| {
            Line::from(Span::styled(
                text::ellipsize(&text::one_line(&value), width, theme.ascii()),
                theme.fg(tok),
            ))
        };
        let mut lines = vec![
            clip(label(s), Tok::Accent),
            clip(
                format!(
                    "{} · {}{}",
                    s.model.rsplit('/').next().unwrap_or(&s.model),
                    age(&s.updated),
                    if self.all_projects {
                        format!(" · {}", s.project)
                    } else {
                        String::new()
                    }
                ),
                Tok::Muted,
            ),
        ];
        match self.preview.get(&s.id) {
            None => lines.push(clip("Loading preview".into(), Tok::Muted)),
            Some(turns) if turns.is_empty() => {
                lines.push(clip("No conversation messages".into(), Tok::Muted))
            }
            Some(turns) => {
                let count = turns.len();
                let mut shown: Vec<Option<&(String, String)>> =
                    turns.iter().take(PREVIEW_TURNS).map(Some).collect();
                if count > 2 * PREVIEW_TURNS {
                    shown.push(None);
                }
                let tail = count.saturating_sub(PREVIEW_TURNS).max(PREVIEW_TURNS);
                shown.extend(turns.iter().skip(tail).map(Some));
                for turn in shown {
                    match turn {
                        None => lines.push(clip("…".into(), Tok::Muted)),
                        Some((role, body)) => {
                            let user = role == "user";
                            lines.push(clip(
                                format!("{}  {body}", if user { "You" } else { "Dial" }),
                                if user { Tok::Text } else { Tok::Muted },
                            ))
                        }
                    }
                }
            }
        }
        lines
    }
}

/// Whether the preview pane has room beside the list.
fn wide(cols: usize, height: usize) -> bool {
    cols >= PREVIEW_MIN_COLS && height >= PREVIEW_MIN_ROWS
}

/// Joins a list row with the matching preview row, keeping a separator column in every row.
fn beside(
    left: Line<'static>,
    right: Option<&Line<'static>>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let used: usize = left.spans.iter().map(|s| text::width(&s.content)).sum();
    let mut spans = left.spans;
    spans.push(Span::styled(
        " ".repeat(width.saturating_sub(used)),
        left.style,
    ));
    spans.push(Span::styled(
        format!(" {} ", theme.sym(Sym::BoxV)),
        theme.fg(Tok::BorderMuted),
    ));
    if let Some(right) = right {
        spans.extend(right.spans.iter().cloned());
    }
    Line::from(spans)
}

/// Tree order: each root (no parent in the list) followed by its descendants depth-first, with
/// siblings most recently active first. Returns `(index, depth)` for every session exactly once.
pub fn tree_order(sessions: &[SessionInfo]) -> Vec<(usize, usize)> {
    let index: HashMap<&str, usize> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); sessions.len()];
    let mut roots = Vec::new();
    for (i, session) in sessions.iter().enumerate() {
        match session.parent_id.as_deref().and_then(|p| index.get(p)) {
            Some(&parent) if parent != i => children[parent].push(i),
            _ => roots.push(i),
        }
    }
    let recent_first = |a: &usize, b: &usize| {
        sessions[*b]
            .updated
            .cmp(&sessions[*a].updated)
            .then_with(|| sessions[*b].id.cmp(&sessions[*a].id))
    };
    roots.sort_by(recent_first);
    for siblings in &mut children {
        siblings.sort_by(recent_first);
    }
    let mut out = Vec::with_capacity(sessions.len());
    let mut seen = vec![false; sessions.len()];
    // Parent cycles reach no root; they are started from their own index so nothing is dropped.
    for start in roots.into_iter().chain(0..sessions.len()) {
        let mut stack = vec![(start, 0usize)];
        while let Some((node, depth)) = stack.pop() {
            if std::mem::replace(&mut seen[node], true) {
                continue;
            }
            out.push((node, depth));
            for &child in children[node].iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }
    out
}

/// Date group of an activity stamp relative to `now` (both Unix seconds, UTC days).
fn day_group(stamp: &str, now: i64) -> &'static str {
    let Some(then) = stamp.get(..19).and_then(crate::platform::time::parse_utc) else {
        return "Older";
    };
    match now.div_euclid(86400) - then.div_euclid(86400) {
        ..=0 => "Today",
        1 => "Yesterday",
        2..=6 => "This week",
        _ => "Older",
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn search_text(s: &SessionInfo) -> String {
    text::one_line(&format!(
        "{} {} {} {} {} {}",
        s.title, s.first_message, s.last_message, s.last_answer, s.model, s.id
    ))
    .to_lowercase()
}

fn label(s: &SessionInfo) -> String {
    [s.title.trim(), s.first_message.trim()]
        .into_iter()
        .find(|s| !s.is_empty())
        .map(text::one_line)
        .unwrap_or_else(|| "(empty session)".into())
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub fn age(stamp: &str) -> String {
    let Some(then) = stamp.get(..19).and_then(crate::platform::time::parse_utc) else {
        return text::one_line(stamp);
    };
    match now_secs().saturating_sub(then).max(0) {
        0..=59 => "now".into(),
        s @ 60..=3599 => format!("{}m ago", s / 60),
        s @ 3600..=86399 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests;
