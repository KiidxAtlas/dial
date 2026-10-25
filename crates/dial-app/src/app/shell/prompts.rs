//! Asking the user a question from inside a turn, with optional cancellation.

use crate::app::panel::{
    RECOVERY_PANEL, panel_body_limit, panel_has_room, panel_inner, panel_rows_window, panel_scroll,
};
use crate::app::shell::{Shell, TICK};
use crate::ui::keys::Action;
use crate::ui::text;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, poll, read};

impl Shell {
    // ----- asking the user ------------------------------------------------------------------

    /// Keep draft-only consent across bridges and turns, but not across app launches.
    pub(in crate::app) fn review_turn_draft(
        &mut self,
        cancel: &std::sync::atomic::AtomicBool,
        ask: impl FnOnce(&mut Self) -> Option<usize>,
    ) -> bool {
        use std::sync::atomic::Ordering;

        if cancel.load(Ordering::Acquire) {
            return false;
        }
        if self.accept_all_turn_drafts {
            return true;
        }
        let choice = ask(self);
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        match choice {
            Some(0) => true,
            Some(1) => {
                self.accept_all_turn_drafts = true;
                true
            }
            _ => false,
        }
    }

    /// A modal choice drawn in place of the composer. `None` means the user cancelled.
    pub(in crate::app) fn ask(
        &mut self,
        title: &str,
        body: &str,
        options: &[(&str, char)],
    ) -> Option<usize> {
        self.ask_with_cancel(title, body, options, None)
    }

    pub(in crate::app) fn ask_with_cancel(
        &mut self,
        title: &str,
        body: &str,
        options: &[(&str, char)],
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<usize> {
        if options.is_empty() {
            return None;
        }
        // The terminal tab shows that Dial waits for an answer while the panel is up.
        let was_waiting = std::mem::replace(&mut self.awaiting_user, true);
        let answer = self.ask_loop(title, body, options, cancel);
        self.awaiting_user = was_waiting;
        answer
    }

    fn ask_loop(
        &mut self,
        title: &str,
        body: &str,
        options: &[(&str, char)],
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<usize> {
        let mut body_offset = 0usize;
        let mut selected = 0usize;
        let mut show_raw = false;
        loop {
            if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
                return None;
            }
            let shown = if title == RECOVERY_PANEL {
                format!(
                    "{}\n\n{} {}",
                    crate::ui::recovery::panel_text(body, show_raw),
                    self.keys.label(Action::ToolsExpand),
                    if show_raw {
                        "to hide details"
                    } else {
                        "for details"
                    }
                )
            } else {
                body.to_owned()
            };
            let (cols, height) = self.term.size();
            let rows = panel_rows_window(
                self.themes.current(),
                cols,
                title,
                &shown,
                options,
                selected,
                body_offset,
                height.saturating_sub(6) as usize,
            );
            if self.paint(Some(rows)).is_err() {
                return None;
            }
            if !poll(TICK).ok()? {
                continue;
            }
            match read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    if !panel_has_room(cols, height.saturating_sub(6) as usize) {
                        // A tiny viewport cannot show what a confirmation would authorize. Keep
                        // cancellation available, but never act on Enter or an approval hotkey.
                        if self.keys.matches(Action::Dismiss, &key)
                            || key.code == KeyCode::Esc
                            || (key.code == KeyCode::Char('c') && ctrl)
                        {
                            return None;
                        }
                        continue;
                    }
                    // Page by the body rows actually visible, keeping one row of overlap, so a short
                    // terminal never skips details the user has not seen.
                    let down = self.keys.matches(Action::SelectPageDown, &key);
                    if down || self.keys.matches(Action::SelectPageUp, &key) {
                        let (cols, height) = self.term.size();
                        let lines =
                            text::wrap_words(&text::sanitize(&shown), panel_inner(cols)).len();
                        let visible =
                            panel_body_limit(options.len(), height.saturating_sub(6) as usize);
                        body_offset = panel_scroll(body_offset, lines, visible, down);
                        continue;
                    }
                    if title == RECOVERY_PANEL && self.keys.matches(Action::ToolsExpand, &key) {
                        show_raw = !show_raw;
                        body_offset = 0;
                        continue;
                    }
                    if self.keys.matches(Action::Dismiss, &key) {
                        return None;
                    }
                    if self.keys.matches(Action::SelectUp, &key) {
                        selected = (selected + options.len() - 1) % options.len();
                        continue;
                    }
                    if self.keys.matches(Action::SelectDown, &key) {
                        selected = (selected + 1) % options.len();
                        continue;
                    }
                    match key.code {
                        KeyCode::Up => selected = (selected + options.len() - 1) % options.len(),
                        KeyCode::Down | KeyCode::Tab => selected = (selected + 1) % options.len(),
                        KeyCode::Enter => return Some(selected),
                        KeyCode::Esc => return None,
                        KeyCode::Char('c') if ctrl => return None,
                        KeyCode::Char(c) if !ctrl => {
                            if let Some(index) = options.iter().position(|(_, hotkey)| {
                                *hotkey != ' ' && hotkey.eq_ignore_ascii_case(&c)
                            }) {
                                return Some(index);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Resize(..)) => self.term.on_resize(),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests;
