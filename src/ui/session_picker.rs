//! Resume catalogue and terminal presentation. The controller alone opens journals or persists pins.
use std::collections::HashSet;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

use super::{
    Rows, chrome, text,
    theme::{Sym, Theme, Tok},
};
use crate::storage::session::SessionInfo;

pub enum Action {
    None,
    Cancel,
    Resume(usize),
    Pin(usize),
}

pub struct Picker {
    pub sessions: Vec<SessionInfo>,
    pinned: HashSet<String>,
    current: String,
    tree: bool,
    show_empty: bool,
    query: String,
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
        let searchable = sessions.iter().map(search_text).collect();
        let mut picker = Self {
            sessions,
            pinned: pinned.into_iter().collect(),
            current: current.into(),
            tree,
            show_empty: tree,
            query: text::one_line(query),
            selected: 0,
            found: Vec::new(),
            searchable,
        };
        picker.refilter(None);
        picker
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
            if !self.show_empty && session.user_messages == 0 && !self.pinned.contains(&session.id)
            {
                continue;
            }
            let title = text::one_line(&session.title).to_lowercase();
            let hay = &self.searchable[index];
            let rank = if tokens.is_empty() {
                0
            } else if title == query {
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
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => return Action::Cancel,
                    KeyCode::Char('c') if ctrl => return Action::Cancel,
                    KeyCode::Char('p') if ctrl => {
                        if let Some(&index) = self.found.get(self.selected) {
                            return Action::Pin(index);
                        }
                    }
                    KeyCode::Tab => {
                        let id = self
                            .found
                            .get(self.selected)
                            .map(|&i| self.sessions[i].id.clone());
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
                        if let Some(&index) = self.found.get(self.selected) {
                            return Action::Resume(index);
                        }
                    }
                    KeyCode::Char('u') if ctrl => {
                        self.query.clear();
                        self.refilter(None);
                    }
                    KeyCode::Backspace => {
                        self.query.pop();
                        self.refilter(None);
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.query.push(c);
                        self.refilter(None);
                    }
                    _ => {}
                }
            }
            Event::Paste(value) => {
                self.query.push_str(&text::one_line(value));
                self.refilter(None);
            }
            _ => {}
        }
        Action::None
    }

    fn page_size(&self, height: usize) -> usize {
        if height < 18 {
            height.saturating_sub(7).max(1)
        } else {
            height.saturating_sub(14).checked_div(2).unwrap_or(1).max(1)
        }
    }

    pub fn rows(&self, cols: usize, height: usize, theme: &Theme) -> Rows {
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
        let width = cols.saturating_sub(4);
        let compact = height < 18;
        let clip = |value: &str| text::ellipsize(&text::one_line(value), width, theme.ascii());
        let line = |value: String, tok| Line::from(Span::styled(clip(&value), theme.fg(tok)));
        let mut body = vec![Line::from(vec![
            Span::styled("Search  ", theme.fg(Tok::Accent)),
            Span::styled(
                text::ellipsize(&self.query, width.saturating_sub(9), theme.ascii()),
                theme.fg(Tok::Text),
            ),
            Span::styled(theme.sym(Sym::BoxV).to_owned(), theme.fg(Tok::Accent)),
        ])];
        let empty = self
            .sessions
            .iter()
            .filter(|s| s.user_messages == 0 && !self.pinned.contains(&s.id))
            .count();
        if height >= 7 {
            body.push(line(
                format!(
                    "{} · Tab {} · {} empty hidden · this project",
                    if self.show_empty {
                        "All sessions"
                    } else {
                        "Conversations"
                    },
                    if self.show_empty {
                        "conversations"
                    } else {
                        "show all"
                    },
                    if self.show_empty { 0 } else { empty }
                ),
                Tok::Muted,
            ));
        }
        let visible = self.page_size(height);
        let start = self.selected.saturating_sub(visible.saturating_sub(1));
        if self.found.is_empty() {
            body.push(line(
                if self.query.trim().is_empty() {
                    "No conversations yet. Tab shows empty sessions.".into()
                } else {
                    "No matches. Ctrl+U clears search; Tab includes empty sessions.".into()
                },
                Tok::Muted,
            ));
        }
        for (position, &index) in self.found.iter().enumerate().skip(start).take(visible) {
            let s = &self.sessions[index];
            let selected = position == self.selected;
            let title = label(s);
            let marker = if selected {
                theme.sym(Sym::Pointer)
            } else {
                " "
            };
            let flags = if width >= 44 {
                format!(
                    "{}{}",
                    if self.pinned.contains(&s.id) {
                        "[pin] "
                    } else {
                        ""
                    },
                    if s.id == self.current {
                        "[current] "
                    } else {
                        ""
                    }
                )
            } else {
                String::new()
            };
            let age = if width >= 28 {
                age(&s.updated)
            } else {
                String::new()
            };
            let room = width.saturating_sub(text::width(&flags) + text::width(&age) + 4);
            let title = text::ellipsize(&title, room, theme.ascii());
            let gap = width
                .saturating_sub(text::width(&title) + text::width(&flags) + text::width(&age) + 2);
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
                body.push(line(
                    format!(
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
        if !compact {
            if let Some(&index) = self.found.get(self.selected) {
                let s = &self.sessions[index];
                body.push(line(theme.sym(Sym::BoxH).repeat(width), Tok::Border));
                let branch = s
                    .parent_id
                    .as_ref()
                    .map(|id| format!(" · fork of {}", short(id)))
                    .unwrap_or_default();
                body.push(line(
                    format!(
                        "{} request{} · {} message{} · {} · {}{}",
                        s.user_messages,
                        if s.user_messages == 1 { "" } else { "s" },
                        s.messages,
                        if s.messages == 1 { "" } else { "s" },
                        s.model.rsplit('/').next().unwrap_or(&s.model),
                        short(&s.id),
                        branch
                    ),
                    Tok::Muted,
                ));
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
        }
        body.push(line(
            format!(
                "{}/{} · ↑↓ move · Enter resume · Ctrl+P pin · Esc cancel",
                if self.found.is_empty() {
                    0
                } else {
                    self.selected + 1
                },
                self.found.len()
            ),
            Tok::Muted,
        ));
        chrome::framed_lines(
            theme,
            chrome::Section::Dial,
            if self.tree { "Session tree" } else { "Resume" },
            &format!("{} found", self.found.len()),
            body,
            cols,
        )
    }
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

fn age(stamp: &str) -> String {
    let Some(then) = stamp.get(..19).and_then(crate::platform::time::parse_utc) else {
        return text::one_line(stamp);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    match now.saturating_sub(then).max(0) {
        0..=59 => "now".into(),
        s @ 60..=3599 => format!("{}m ago", s / 60),
        s @ 3600..=86399 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    fn session(id: &str, title: &str, requests: usize) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            path: format!("/{id}.jsonl").into(),
            started: "2026-01-01T00:00:00Z".into(),
            updated: "2026-01-02T00:00:00Z".into(),
            title: title.into(),
            messages: requests * 2,
            user_messages: requests,
            model: "provider/model".into(),
            parent_id: None,
            first_message: title.into(),
            last_message: "Fix callback retry 日本語".into(),
            last_answer: "Preserve the useful error.".into(),
            project: "project".into(),
        }
    }
    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }
    #[test]
    fn conversations_hide_empty_but_pins_and_all_view_keep_them_accessible() {
        let mut p = Picker::new(
            vec![
                session("empty", "", 0),
                session("pin", "Saved empty", 0),
                session("chat", "Fix", 1),
            ],
            vec!["pin".into()],
            "chat",
            "",
            false,
        );
        assert_eq!(p.found, [1, 2]);
        p.handle(&key(KeyCode::Tab, KeyModifiers::NONE), 24);
        assert_eq!(p.found, [0, 1, 2]);
        p.handle(&key(KeyCode::Home, KeyModifiers::NONE), 24);
        assert!(matches!(
            p.handle(&key(KeyCode::Enter, KeyModifiers::NONE), 24),
            Action::Resume(0)
        ));
    }
    #[test]
    fn pinning_keeps_selection_and_unpinning_restores_activity_order() {
        let mut older = session("older", "Old", 1);
        older.updated = "2025-01-01T00:00:00Z".into();
        let mut p = Picker::new(
            vec![session("newer", "New", 1), older],
            vec![],
            "",
            "",
            false,
        );
        p.handle(&key(KeyCode::Down, KeyModifiers::NONE), 24);
        assert!(matches!(
            p.handle(&key(KeyCode::Char('p'), KeyModifiers::CONTROL), 24),
            Action::Pin(1)
        ));
        p.set_pinned(1, true);
        assert_eq!(p.sessions[p.found[p.selected]].id, "older");
        assert_eq!(p.found[0], 0);
        p.set_pinned(0, false);
        assert_eq!(p.sessions[0].id, "newer");
        assert_eq!(p.sessions[p.found[p.selected]].id, "older");
    }

    #[test]
    fn tree_includes_empty_sessions_and_never_reorders_ancestry_for_search() {
        let p = Picker::new(
            vec![
                session("parent", "Callback changes", 0),
                session("child", "Callback", 1),
            ],
            vec![],
            "",
            "callback",
            true,
        );
        assert_eq!(p.found, [0, 1]);
    }

    #[test]
    fn search_ranks_titles_and_matches_words_in_any_order_without_path_noise() {
        let mut a = session("one", "Callback retry", 1);
        a.last_message = "Other topic".into();
        let p = Picker::new(
            vec![session("two", "Other topic", 1), a],
            vec![],
            "",
            "callback retry",
            false,
        );
        assert_eq!(p.found, [1, 0]);
        let p = Picker::new(p.sessions, vec![], "", "日本語 callback", false);
        assert_eq!(p.found, [0]);
    }
    #[test]
    fn no_matches_is_safe_and_paste_cannot_inject_terminal_controls() {
        let mut p = Picker::new(
            vec![session("one", "Title", 1)],
            vec![],
            "",
            "no-match-123",
            false,
        );
        assert!(matches!(
            p.handle(&key(KeyCode::Enter, KeyModifiers::NONE), 24),
            Action::None
        ));
        p.handle(&key(KeyCode::Char('u'), KeyModifiers::CONTROL), 24);
        assert_eq!(p.found, [0]);
        p.handle(&Event::Paste("\x1b[31m日本語\ncallback".into()), 24);
        assert!(!p.query.contains('\x1b'));
        assert!(!p.query.contains('\n'));
    }
    #[test]
    fn layout_fits_small_and_wide_terminals_without_losing_selection() {
        let theme = Theme::dark();
        let mut p = Picker::new(
            (0..30)
                .map(|i| {
                    session(
                        &i.to_string(),
                        "A long Unicode title 日本語 about retrying callbacks",
                        1,
                    )
                })
                .collect(),
            vec!["1".into()],
            "2",
            "",
            false,
        );
        for (width, height) in [(110, 38), (80, 24), (44, 18), (30, 12), (12, 5), (8, 3)] {
            p.handle(&key(KeyCode::End, KeyModifiers::NONE), height);
            let rows = p.rows(width, height, &theme);
            assert!(
                rows.len() <= height,
                "{width}x{height}: {} rows",
                rows.len()
            );
            for row in rows {
                let value: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
                assert!(text::width(&value) <= width, "{width}x{height}: {value:?}");
            }
            assert!(matches!(
                p.handle(&key(KeyCode::Enter, KeyModifiers::NONE), height),
                Action::Resume(29)
            ));
        }
    }
}
