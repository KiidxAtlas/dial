//! Keep pasted line breaks out of the prompt's submit-key path.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

const PASTE_SETTLE: Duration = Duration::from_millis(150);

#[derive(Default)]
pub struct PasteGuard {
    until: Option<Instant>,
    raw: bool,
}

impl PasteGuard {
    pub fn pasted(&mut self, now: Instant) {
        self.until = Some(now + PASTE_SETTLE);
        self.raw = false;
    }

    /// Buffered printable keys can be an unbracketed paste. Preserve their line breaks;
    /// discard an immediate extra Enter following a bracketed paste. Explicit Enter after
    /// the input settles still submits, and editing/navigation keys end the protection.
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
        match key.code {
            KeyCode::Char(_)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                if buffered || (protected && self.raw) {
                    self.until = Some(now + PASTE_SETTLE);
                    self.raw = true;
                }
                None
            }
            KeyCode::Enter if key.modifiers.is_empty() && (protected || buffered) => {
                if !protected {
                    self.raw = true;
                }
                self.until = Some(now + PASTE_SETTLE);
                Some(if self.raw { "\n" } else { "" })
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
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn raw_paste_keeps_all_newlines_until_a_separate_enter() {
        let now = Instant::now();
        let mut guard = PasteGuard::default();
        assert_eq!(guard.text_for_key(key(KeyCode::Char('a')), true, now), None);
        assert_eq!(
            guard.text_for_key(key(KeyCode::Enter), true, now),
            Some("\n")
        );
        assert_eq!(guard.text_for_key(key(KeyCode::Char('b')), true, now), None);
        assert_eq!(
            guard.text_for_key(key(KeyCode::Enter), false, now),
            Some("\n")
        );
        assert_eq!(
            guard.text_for_key(key(KeyCode::Enter), false, now + PASTE_SETTLE),
            None
        );
    }
    #[test]
    fn bracketed_paste_discards_trailing_enter_without_queuing_a_turn() {
        let now = Instant::now();
        let mut guard = PasteGuard::default();
        guard.pasted(now);
        assert_eq!(
            guard.text_for_key(key(KeyCode::Enter), false, now),
            Some("")
        );
        assert_eq!(
            guard.text_for_key(key(KeyCode::Enter), false, now + PASTE_SETTLE),
            None
        );
    }
    #[test]
    fn raw_paste_can_begin_with_a_blank_line() {
        let now = Instant::now();
        let mut guard = PasteGuard::default();
        assert_eq!(
            guard.text_for_key(key(KeyCode::Enter), true, now),
            Some("\n")
        );
        assert_eq!(
            guard.text_for_key(key(KeyCode::Enter), false, now),
            Some("\n")
        );
    }
    #[test]
    fn ordinary_typing_navigation_and_explicit_shortcuts_are_preserved() {
        let now = Instant::now();
        let mut guard = PasteGuard::default();
        assert_eq!(
            guard.text_for_key(key(KeyCode::Char('a')), false, now),
            None
        );
        assert_eq!(guard.text_for_key(key(KeyCode::Enter), false, now), None);
        guard.pasted(now);
        assert_eq!(guard.text_for_key(key(KeyCode::Left), false, now), None);
        assert_eq!(guard.text_for_key(key(KeyCode::Enter), false, now), None);
        guard.pasted(now);
        assert_eq!(
            guard.text_for_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), false, now),
            None
        );
    }
}
