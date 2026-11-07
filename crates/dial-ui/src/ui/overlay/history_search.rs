//! Read-only full-text history hits. Opening a conversation is a controller action, never rewind.
use super::settings::{filled, panel, pressed};
use super::{DrawCtx, OverlayEvent};
use crate::storage::session::SearchHit;
use crate::ui::{
    chrome::Section,
    keys::{Action, Keymap},
    text,
    theme::Tok,
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    style::Style,
    text::{Line, Span},
};

pub struct HistorySearchOverlay {
    query: String,
    hits: Vec<SearchHit>,
    cursor: usize,
    preview_scroll: usize,
    return_available: bool,
}

impl HistorySearchOverlay {
    pub fn new(query: String, hits: Vec<SearchHit>, return_available: bool) -> Self {
        Self {
            query,
            hits,
            cursor: 0,
            preview_scroll: 0,
            return_available,
        }
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if !pressed(key) {
            return OverlayEvent::None;
        }
        if super::panel_keys::matches(keys, Action::SelectCancel, key) {
            return OverlayEvent::Close;
        }
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
            self.preview_scroll = 0;
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(self.hits.len().saturating_sub(1));
            self.preview_scroll = 0;
        } else if keys.matches(Action::SelectPageUp, key) {
            self.preview_scroll = self.preview_scroll.saturating_sub(8);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.preview_scroll = self.preview_scroll.saturating_add(8);
        } else if keys.matches(Action::SelectConfirm, key) {
            if let Some(hit) = self.hits.get(self.cursor) {
                return OverlayEvent::Action(format!("history-open:{}", hit.session_id));
            }
        } else if key.code == KeyCode::Char('b') && self.return_available {
            return OverlayEvent::Action("history-return".into());
        }
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        panel(
            frame,
            ctx.theme,
            Section::Dial,
            "Search history",
            "read-only preview",
            |w, h| self.rows(w, h, ctx),
        );
    }

    fn rows(&mut self, width: u16, height: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(width), usize::from(height));
        let line = |value: String, tok| {
            filled(
                vec![Span::styled(value, ctx.theme.fg(tok))],
                w,
                Style::default(),
            )
        };
        let mut rows = vec![line(
            format!(
                " {} · {} matches",
                text::one_line(&self.query),
                self.hits.len()
            ),
            Tok::Accent,
        )];
        if h < 3 {
            rows.truncate(h);
            return rows;
        }
        let footer = text::wrap_words(
            &format!(
                "Enter open · Esc close{} · ↑↓ select · PgUp/PgDn preview",
                if self.return_available {
                    " · b return"
                } else {
                    ""
                }
            ),
            w.saturating_sub(1).max(1),
        );
        let list_h = (h / 3).max(1).min(self.hits.len());
        let start = self.cursor.saturating_sub(list_h.saturating_sub(1));
        for (index, hit) in self.hits.iter().enumerate().skip(start).take(list_h) {
            rows.push(line(
                format!(
                    "{} {} [{}] {}",
                    if index == self.cursor { ">" } else { " " },
                    text::one_line(&hit.session_id),
                    text::one_line(&hit.role),
                    text::one_line(&hit.snippet)
                ),
                if index == self.cursor {
                    Tok::Accent
                } else {
                    Tok::Text
                },
            ));
        }
        if let Some(hit) = self.hits.get(self.cursor) {
            rows.push(line(
                format!(
                    " Match {} · {} · {}",
                    text::one_line(&hit.entry_id),
                    text::one_line(&hit.when),
                    "no branch changes"
                ),
                Tok::Muted,
            ));
            let body = text::wrap_words(&text::one_line(&hit.preview), w.saturating_sub(2).max(1));
            let preview_h = h.saturating_sub(rows.len() + footer.len());
            self.preview_scroll = self
                .preview_scroll
                .min(body.len().saturating_sub(preview_h.max(1)));
            rows.extend(
                body.into_iter()
                    .skip(self.preview_scroll)
                    .take(preview_h)
                    .map(|value| line(format!(" {value}"), Tok::Text)),
            );
        } else {
            rows.push(line(
                " No matching messages. Esc returns to your draft.".into(),
                Tok::Muted,
            ));
        }
        rows.truncate(h.saturating_sub(footer.len()));
        rows.extend(
            footer
                .into_iter()
                .take(h)
                .map(|value| line(format!(" {value}"), Tok::Muted)),
        );
        rows
    }
}

#[cfg(test)]
mod tests;
