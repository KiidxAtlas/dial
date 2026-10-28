//! Keep pasted line breaks out of the prompt's submit-key path.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

mod pump;

pub use pump::{halt_for_panic, poll, read, start, stop};

const PASTE_SETTLE: Duration = Duration::from_millis(150);

#[derive(Default)]
pub struct PasteGuard {
    until: Option<Instant>,
    raw: bool,
    bracketed: bool,
}

/// A single-line slash command is submitted by Enter even inside a burst of buffered keys. Only
/// an unbracketed paste that already contains a line break keeps Enter as a newline.
pub fn enter_submits_command(draft: &str) -> bool {
    draft.starts_with('/') && !draft.contains('\n')
}

impl PasteGuard {
    pub fn pasted(&mut self, now: Instant) {
        self.until = Some(now + PASTE_SETTLE);
        self.raw = false;
        self.bracketed = true;
    }

    /// Applies the command Enter shortcut without bypassing bracketed-paste protection.
    /// Buffered typed commands submit immediately; a pasted command's trailing Enter does not.
    pub fn text_for_draft_key(
        &mut self,
        key: KeyEvent,
        buffered: bool,
        now: Instant,
        draft: &str,
    ) -> Option<&'static str> {
        let bracketed = self.bracketed && self.until.is_some_and(|until| now < until);
        if key.kind != KeyEventKind::Release
            && key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && enter_submits_command(draft)
            && !bracketed
        {
            *self = Self::default();
            return None;
        }
        self.text_for_key(key, buffered, now)
    }

    /// Buffered printable keys can be an unbracketed paste. A line break followed by more
    /// buffered input is a paste's newline; an Enter with nothing queued after it submits, even
    /// when the keys before it were queued behind a stalled loop. A bracketed paste's trailing
    /// Enter is discarded. Editing/navigation keys end the protection.
    pub fn text_for_key(
        &mut self,
        key: KeyEvent,
        buffered: bool,
        now: Instant,
    ) -> Option<&'static str> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let protected = self.until.is_some_and(|until| now < until);
        if !protected {
            self.raw = false;
        }
        match key.code {
            KeyCode::Char(_)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                if buffered || (protected && self.raw) {
                    self.until = Some(now + PASTE_SETTLE);
                }
                self.bracketed = false;
                None
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                if protected && self.bracketed {
                    // A bracketed paste's trailing Enter is discarded, queued keys or not.
                    self.until = Some(now + PASTE_SETTLE);
                    Some("")
                } else if buffered {
                    // More input follows in the same burst: this Enter is a line break inside a paste.
                    self.raw = true;
                    self.until = Some(now + PASTE_SETTLE);
                    Some("\n")
                } else if protected && self.raw {
                    self.until = Some(now + PASTE_SETTLE);
                    Some("\n")
                } else {
                    self.until = None;
                    None
                }
            }
            KeyCode::Tab if key.modifiers.is_empty() && protected && self.raw => {
                self.until = Some(now + PASTE_SETTLE);
                Some("\t")
            }
            _ => {
                self.until = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
