//! Read-only full-text history hits. Opening a conversation is a controller action, never rewind.
use super::settings::{Axis, arrows, hint_line, list_row, panel, pressed, scroll_cell, typed_char};
use super::{DrawCtx, OverlayEvent};
use crate::storage::session::SearchHit;
use crate::ui::{
    chrome::{self, Section},
    keys::{Action, Keymap},
    text,
    theme::{Sym, Tok},
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    style::Modifier,
    text::{Line, Span},
};
use std::collections::HashMap;

pub struct HistorySearchOverlay {
    panel_keys: super::panel_keys::PanelKeys,
    query: String,
    hits: Vec<SearchHit>,
    /// Session titles by id, shown instead of raw ids when the controller supplies them.
    titles: HashMap<String, String>,
    /// Narrows the hits by session, role and text; `/` starts typing it.
    filter: String,
    filtering: bool,
    /// Indexes into `hits` that match `filter`, in order.
    shown: Vec<usize>,
    /// Position in `shown`.
    cursor: usize,
    preview_scroll: usize,
    return_available: bool,
}

impl HistorySearchOverlay {
    pub fn new(query: String, hits: Vec<SearchHit>, return_available: bool) -> Self {
        let shown = (0..hits.len()).collect();
        Self {
            panel_keys: Default::default(),
            query,
            hits,
            titles: HashMap::new(),
            filter: String::new(),
            filtering: false,
            shown,
            cursor: 0,
            preview_scroll: 0,
            return_available,
        }
    }

    /// Session titles by session id; rows show a title instead of the raw id when one is known.
    pub fn with_titles(mut self, titles: HashMap<String, String>) -> Self {
        self.titles = titles;
        self
    }

    fn selected(&self) -> Option<&SearchHit> {
        self.shown.get(self.cursor).map(|&i| &self.hits[i])
    }

    /// The session's title, or its short id when no title is known.
    fn session_label(&self, hit: &SearchHit) -> String {
        self.titles
            .get(&hit.session_id)
            .map(|t| text::one_line(t))
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| text::one_line(hit.session_id.get(..8).unwrap_or(&hit.session_id)))
    }

    fn refilter(&mut self) {
        let terms: Vec<String> = self
            .filter
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.shown = (0..self.hits.len())
            .filter(|&i| {
                let hit = &self.hits[i];
                let hay = format!(
                    "{} {} {} {} {}",
                    self.session_label(hit),
                    hit.session_id,
                    hit.role,
                    hit.snippet,
                    hit.preview
                )
                .to_lowercase();
                terms.iter().all(|term| hay.contains(term))
            })
            .collect();
        self.cursor = self.cursor.min(self.shown.len().saturating_sub(1));
        self.preview_scroll = 0;
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if self.panel_keys.handle_key(event, self.filtering) {
            return OverlayEvent::None;
        }
        if let Event::Paste(value) = event {
            if self.filtering {
                self.filter.push_str(&text::one_line(value));
                self.refilter();
            }
            return OverlayEvent::None;
        }
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if !pressed(key) {
            return OverlayEvent::None;
        }
        if super::panel_keys::matches(keys, Action::SelectCancel, key) {
            if self.filtering || !self.filter.is_empty() {
                self.filtering = false;
                self.filter.clear();
                self.refilter();
                return OverlayEvent::None;
            }
            return OverlayEvent::Close;
        }
        let last = self.shown.len().saturating_sub(1);
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
            self.preview_scroll = 0;
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(last);
            self.preview_scroll = 0;
        } else if keys.matches(Action::SelectPageUp, key) {
            self.preview_scroll = self.preview_scroll.saturating_sub(8);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.preview_scroll = self.preview_scroll.saturating_add(8);
        } else if keys.matches(Action::SelectConfirm, key) {
            if self.filtering {
                // Enter keeps the filter and returns to the list.
                self.filtering = false;
            } else if let Some(hit) = self.selected() {
                return OverlayEvent::Action(format!("history-open:{}", hit.session_id));
            }
        } else if self.filtering {
            if key.code == KeyCode::Backspace {
                self.filter.pop();
                self.refilter();
            } else if let Some(c) = typed_char(key) {
                self.filter.push(c);
                self.refilter();
            }
        } else if key.code == KeyCode::Char('/') {
            self.filtering = true;
        } else if key.code == KeyCode::Char('b') && self.return_available {
            return OverlayEvent::Action("history-return".into());
        }
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        if self.panel_keys.draw(frame, ctx, "Search history", "Enter opens the conversation (read-only preview here; nothing is rewound). PgUp/PgDn scroll the preview.\nb returns to the session you came from when offered.", false, true) {
            return;
        }
        panel(
            frame,
            ctx.theme,
            Section::Dial,
            "Search history",
            "read-only preview",
            |w, h| self.rows(w, h, ctx),
        );
    }

    /// Footer hints packed into one row, or two when `w` is narrow, most useful first and the way
    /// out last, so a narrow panel keeps every control instead of dropping them.
    fn footer(&self, ctx: &DrawCtx, w: usize) -> Vec<Line<'static>> {
        let theme = ctx.theme;
        let mut hints: Vec<String> = Vec::new();
        if self.filtering {
            hints.push("Enter keep filter".into());
        } else if self.selected().is_some() {
            hints.push("Enter open".into());
        }
        if self.return_available && !self.filtering {
            hints.push("b return".into());
        }
        if self.selected().is_some() {
            hints.push("PgUp/PgDn preview".into());
            hints.push(format!("{} select", arrows(theme, Axis::Vertical)));
        }
        if !self.filtering {
            hints.push("/ filter".into());
        }
        hints.push(
            if self.filtering || !self.filter.is_empty() {
                "Esc clear filter"
            } else {
                "Esc close"
            }
            .into(),
        );
        let cells = |set: &[String]| {
            let pairs: Vec<(&str, &str)> = set
                .iter()
                .map(|s| s.split_once(' ').unwrap_or((s, "")))
                .collect();
            chrome::key_hint_spans(theme, &pairs)
                .iter()
                .map(|s| text::width(&s.content))
                .sum::<usize>()
        };
        let room = w.saturating_sub(1);
        if cells(&hints) <= room || hints.len() < 2 {
            return vec![hint_line(theme, &hints, w)];
        }
        // Fill the first row greedily; the rest, way out included, go on the second.
        let mut split = 0;
        while split + 1 < hints.len() && cells(&hints[..=split]) <= room {
            split += 1;
        }
        let split = split.max(1);
        vec![
            hint_line(theme, &hints[..split], w),
            hint_line(theme, &hints[split..], w),
        ]
    }

    fn rows(&mut self, width: u16, height: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(width), usize::from(height));
        let theme = ctx.theme;
        let muted = |value: String| Line::from(Span::styled(value, theme.fg(Tok::Muted)));
        let count = if self.filter.is_empty() {
            format!("{} matches", self.hits.len())
        } else {
            format!("{} of {} matches", self.shown.len(), self.hits.len())
        };
        let mut rows = vec![Line::from(vec![
            Span::styled(
                format!(" {}", text::one_line(&self.query)),
                theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" · {count}"), theme.fg(Tok::Muted)),
        ])];
        if h < 3 {
            rows.truncate(h);
            return rows;
        }
        if self.filtering || !self.filter.is_empty() {
            let mut spans = vec![
                Span::styled(" Filter  ", theme.fg(Tok::Accent)),
                Span::styled(text::one_line(&self.filter), theme.fg(Tok::Text)),
            ];
            if self.filtering {
                spans.push(Span::styled(
                    " ",
                    theme.fg(Tok::Text).add_modifier(Modifier::REVERSED),
                ));
            }
            rows.push(Line::from(spans));
        }
        let footer = self.footer(ctx, w);
        let list_h = (h / 3).max(1).min(self.shown.len());
        let start = self.cursor.saturating_sub(list_h.saturating_sub(1));
        let row_w = w.saturating_sub(1);
        for (row, (position, &index)) in self
            .shown
            .iter()
            .enumerate()
            .skip(start)
            .take(list_h)
            .enumerate()
        {
            let hit = &self.hits[index];
            let selected = position == self.cursor;
            let pointer = if selected {
                theme.sym(Sym::Pointer)
            } else {
                " "
            };
            let spans = vec![
                Span::styled(format!(" {pointer} "), theme.fg(Tok::Accent)),
                Span::styled(self.session_label(hit), theme.fg(Tok::Text)),
                Span::styled(
                    format!("  {}  ", text::one_line(&hit.role)),
                    theme.fg(Tok::Muted),
                ),
                Span::styled(text::one_line(&hit.snippet), theme.fg(Tok::Text)),
            ];
            let mut line = list_row(theme, spans, row_w, selected);
            line.spans
                .push(scroll_cell(theme, row, list_h, self.shown.len(), start));
            rows.push(line);
        }
        if let Some(hit) = self.selected().cloned() {
            let mut detail = vec![format!("Match {}", text::one_line(&hit.entry_id))];
            if !hit.when.trim().is_empty() {
                detail.push(text::one_line(&hit.when));
            }
            if self.titles.contains_key(&hit.session_id) {
                detail.push(text::one_line(&hit.session_id));
            }
            rows.push(muted(format!(" {}", detail.join(" · "))));
            let body = text::wrap_words(&text::one_line(&hit.preview), w.saturating_sub(2).max(1));
            let preview_h = h.saturating_sub(rows.len() + footer.len());
            self.preview_scroll = self
                .preview_scroll
                .min(body.len().saturating_sub(preview_h.max(1)));
            rows.extend(
                body.into_iter()
                    .skip(self.preview_scroll)
                    .take(preview_h)
                    .map(|value| {
                        Line::from(Span::styled(format!(" {value}"), theme.fg(Tok::Text)))
                    }),
            );
        } else if self.hits.is_empty() {
            rows.push(muted(
                " No matching messages. Esc returns to your draft.".into(),
            ));
        } else {
            rows.push(muted(" No matches for the filter. Esc clears it.".into()));
        }
        let body_h = h.saturating_sub(footer.len());
        rows.truncate(body_h);
        rows.resize(body_h, Line::default());
        rows.extend(footer.into_iter().take(h));
        rows
    }
}

#[cfg(test)]
mod tests;
