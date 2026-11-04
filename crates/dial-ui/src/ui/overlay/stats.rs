//! Read-only statistics tabs. The application supplies reports from the existing stats database.
use super::{DrawCtx, OverlayEvent, panel_keys};
use crate::ui::keys::{Action, Keymap};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    widgets::{Block, Borders, Paragraph, Wrap},
};

pub struct StatsOverlay {
    tabs: Vec<(&'static str, String)>,
    active: usize,
    scroll: usize,
    query: String,
    searching: bool,
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

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if self.panel_keys.handle(event, self.searching, false) {
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
            self.scroll = (self.scroll + 1).min(self.lines().len().saturating_sub(1));
        } else if keys.matches(Action::SelectUp, key) {
            self.scroll = self.scroll.saturating_sub(1);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.scroll = self
                .scroll
                .saturating_add(10)
                .min(self.lines().len().saturating_sub(1));
        } else if keys.matches(Action::SelectPageUp, key) {
            self.scroll = self.scroll.saturating_sub(10);
        }
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        if self.panel_keys.draw(frame, ctx, "Stats", "↑/↓ scroll · PgUp/PgDn page\nReports are a cross-session database snapshot, not live provider quotas. /usage shows quotas; /cost shows the current runtime session.", true, true) { return; }
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, (label, _))| {
                if i == self.active {
                    format!("[{label}]")
                } else {
                    (*label).to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let title = format!("Stats · {tabs}");
        let footer = if self.searching {
            format!("Search: {}_ · Enter browse · Esc clear", self.query)
        } else {
            "Tab tabs · / search · ↑/↓ scroll · ? keys · Esc close".into()
        };
        let text = self
            .lines()
            .into_iter()
            .skip(self.scroll)
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_bottom(footer),
            ),
            frame.area(),
        );
    }
}

#[cfg(test)]
mod tests;
