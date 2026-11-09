//! Read-only statistics tabs. The application supplies reports from the existing stats database.
use super::settings::{Axis, arrows, hint, hint_line, panel, scroll_cell, tab_strip};
use super::{DrawCtx, OverlayEvent, panel_keys};
use crate::ui::{
    chrome::Section,
    keys::{Action, Keymap},
    text,
    theme::Tok,
};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    text::{Line, Span},
};

pub struct StatsOverlay {
    tabs: Vec<(&'static str, String)>,
    active: usize,
    /// First visible wrapped row of the report.
    scroll: usize,
    query: String,
    searching: bool,
    /// Wrapped report rows and visible body rows from the last draw, so keys page and clamp by
    /// what is on screen.
    total: usize,
    page: usize,
    panel_keys: panel_keys::PanelKeys,
}

impl StatsOverlay {
    pub fn new(tabs: Vec<(&'static str, String)>) -> Self {
        Self {
            tabs,
            active: 0,
            scroll: 0,
            query: String::new(),
            searching: false,
            total: 0,
            page: 10,
            panel_keys: panel_keys::PanelKeys::default(),
        }
    }

    fn lines(&self) -> Vec<&str> {
        let Some((_, text)) = self.tabs.get(self.active) else {
            return Vec::new();
        };
        let needle = self.query.to_lowercase();
        text.lines()
            .filter(|line| needle.is_empty() || line.to_lowercase().contains(&needle))
            .collect()
    }

    fn max_scroll(&self) -> usize {
        self.total.saturating_sub(self.page)
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if self.panel_keys.handle_key(event, self.searching) {
            return OverlayEvent::None;
        }
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if key.kind == KeyEventKind::Release {
            return OverlayEvent::None;
        }
        if panel_keys::matches(keys, Action::SelectCancel, key) {
            if self.searching || !self.query.is_empty() {
                self.searching = false;
                self.query.clear();
                self.scroll = 0;
                return OverlayEvent::None;
            }
            return OverlayEvent::Close;
        }
        if panel_keys::matches(keys, Action::TabNext, key)
            || panel_keys::matches(keys, Action::TabPrev, key)
        {
            if !self.tabs.is_empty() {
                let delta = if panel_keys::matches(keys, Action::TabNext, key) {
                    1
                } else {
                    self.tabs.len() - 1
                };
                self.active = (self.active + delta) % self.tabs.len();
                self.scroll = 0;
                self.searching = false;
            }
        } else if self.searching {
            match key.code {
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                }
                _ => {
                    if let Some(c) = super::settings::typed_char(key) {
                        self.query.push(c);
                    }
                }
            }
            self.scroll = 0;
        } else if key.code == KeyCode::Char('/') {
            self.searching = true;
        } else if keys.matches(Action::SelectDown, key) {
            self.scroll = (self.scroll + 1).min(self.max_scroll());
        } else if keys.matches(Action::SelectUp, key) {
            self.scroll = self.scroll.saturating_sub(1);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.scroll = self.scroll.saturating_add(self.page).min(self.max_scroll());
        } else if keys.matches(Action::SelectPageUp, key) {
            self.scroll = self.scroll.saturating_sub(self.page);
        } else if key.code == KeyCode::Home {
            self.scroll = 0;
        } else if key.code == KeyCode::End {
            self.scroll = self.max_scroll();
        }
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        if self.panel_keys.draw(
            frame,
            ctx,
            "Stats",
            "Home/End jump to the top or bottom of the report.\nReports are a cross-session database snapshot, not live provider quotas. /usage shows quotas; /cost shows the current runtime session.",
            true,
            true,
        ) {
            return;
        }
        let extra = self
            .tabs
            .get(self.active)
            .map(|_| format!("{}/{}", self.active + 1, self.tabs.len()))
            .unwrap_or_default();
        panel(frame, ctx.theme, Section::Dial, "Stats", &extra, |w, h| {
            self.rows(usize::from(w), usize::from(h), ctx)
        });
    }

    /// Tab strip, the search line while one is typed, the wrapped report with a scroll indicator,
    /// and the key hints.
    fn rows(&mut self, w: usize, h: usize, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let theme = ctx.theme;
        let names: Vec<&str> = self.tabs.iter().map(|(name, _)| *name).collect();
        let mut out = vec![
            tab_strip(theme, &names, self.active, true, w),
            Line::default(),
        ];
        if self.searching || !self.query.is_empty() {
            let mut spans = vec![
                Span::styled(" Search  ", theme.fg(Tok::Accent)),
                Span::styled(text::one_line(&self.query), theme.fg(Tok::Text)),
            ];
            if self.searching {
                spans.push(Span::styled(
                    " ",
                    theme
                        .fg(Tok::Text)
                        .add_modifier(ratatui::style::Modifier::REVERSED),
                ));
            }
            out.push(Line::from(spans));
        }
        let text_w = w.saturating_sub(3).max(1);
        let report: Vec<String> = self
            .lines()
            .into_iter()
            .flat_map(|line| {
                if line.is_empty() {
                    vec![String::new()]
                } else {
                    text::wrap_words(line, text_w)
                }
            })
            .collect();
        let body_h = h.saturating_sub(out.len() + 1);
        self.total = report.len();
        self.page = body_h.max(1);
        self.scroll = self.scroll.min(self.max_scroll());
        if report.is_empty() {
            let message = if self.query.is_empty() {
                "Nothing recorded for this report yet."
            } else {
                "No lines match the search. Esc clears it."
            };
            out.push(Line::from(Span::styled(
                format!(" {message}"),
                theme.fg(Tok::Muted),
            )));
        }
        for (row, value) in report.iter().skip(self.scroll).take(body_h).enumerate() {
            let pad = text_w.saturating_sub(text::width(value));
            out.push(Line::from(vec![
                Span::styled(format!(" {value}{} ", " ".repeat(pad)), theme.fg(Tok::Text)),
                scroll_cell(theme, row, body_h, report.len(), self.scroll),
            ]));
        }
        out.truncate(h.saturating_sub(1));
        out.resize(h.saturating_sub(1), Line::default());
        let keys = ctx.keys;
        let hints = if self.searching {
            vec![
                "Enter keep results".to_owned(),
                hint(keys, Action::SelectCancel, "clear search"),
            ]
        } else {
            vec![
                format!("{} scroll", arrows(theme, Axis::Vertical)),
                "Tab tabs".to_owned(),
                "/ search".to_owned(),
                "? keys".to_owned(),
                hint(
                    keys,
                    Action::SelectCancel,
                    if self.query.is_empty() {
                        "close"
                    } else {
                        "clear search"
                    },
                ),
            ]
        };
        out.push(hint_line(theme, &hints, w));
        out
    }
}

#[cfg(test)]
mod tests;
