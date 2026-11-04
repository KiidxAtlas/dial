//! Shared panel navigation and contextual help; text editors retain punctuation input.
use super::DrawCtx;
use crate::ui::keys::{Action, Keymap};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    widgets::{Block, Borders, Paragraph, Wrap},
};

#[derive(Default)]
pub struct PanelKeys {
    help: bool,
    search_focus: bool,
}

/// Fixed panel keys remain available alongside customized legacy bindings.
pub fn matches(keys: &Keymap, action: Action, key: &KeyEvent) -> bool {
    keys.matches(action, key)
        || match action {
            Action::SelectCancel => key.code == KeyCode::Esc,
            Action::SelectConfirm => key.code == KeyCode::Enter,
            Action::TabNext => key.code == KeyCode::Tab,
            Action::TabPrev => key.code == KeyCode::BackTab,
            _ => false,
        }
}

impl PanelKeys {
    /// Returns true when the help layer or search-focus gesture consumed the event.
    /// F1 also opens help when the panel is a permanently focused search editor.
    pub fn handle(&mut self, event: &Event, text_focused: bool, search: bool) -> bool {
        let Event::Key(key) = event else {
            return self.help;
        };
        if key.kind == KeyEventKind::Release {
            return self.help;
        }
        if self.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::Char('?')
            ) {
                self.help = false;
            }
            return true;
        }
        if key.code == KeyCode::F(1)
            || (key.code == KeyCode::Char('?') && !text_focused && !self.search_focus)
        {
            self.help = true;
            return true;
        }
        if search && !text_focused && !self.search_focus && key.code == KeyCode::Char('/') {
            self.search_focus = true;
            return true;
        }
        if key.code == KeyCode::Esc {
            self.search_focus = false;
        }
        false
    }

    /// Render the modal key list instead of the underlying panel while help is open.
    pub fn draw(
        &self,
        frame: &mut Frame,
        ctx: &DrawCtx,
        title: &str,
        extra: &str,
        tabs: bool,
        search: bool,
    ) -> bool {
        if !self.help {
            return false;
        }
        let mut text = format!(
            "{}  select\n{}  open / apply\n{}  back (clear search first)\n? / F1  panel keys\n",
            ctx.keys.label(Action::SelectDown),
            ctx.keys.label(Action::SelectConfirm),
            ctx.keys.label(Action::SelectCancel)
        );
        if tabs {
            text.push_str("Tab / Shift+Tab  switch tabs\n");
        }
        if search {
            text.push_str("/  focus search (type normally when search is focused)\n");
        }
        text.push_str(extra);
        text.push_str("\nEsc / Enter  return to panel");
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("{title} · keys")),
            ),
            frame.area(),
        );
        true
    }
}

#[cfg(test)]
mod tests;
