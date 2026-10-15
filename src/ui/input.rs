//! Keep pasted line breaks out of the prompt's submit-key path.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

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
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn a_typed_command_line_is_submitted_not_split_by_a_buffered_enter() {
        assert!(enter_submits_command("/settings"));
        assert!(enter_submits_command("/sett"));
        assert!(
            !enter_submits_command("/settings\n"),
            "a real multi-line paste keeps its breaks"
        );
        assert!(!enter_submits_command("plain prompt"));
        assert!(!enter_submits_command(""));
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

    #[test]
    fn a_stalled_burst_ending_in_enter_submits_but_an_inner_line_break_stays() {
        // Keys queued behind a stalled loop: `o` sees the Enter queued after it, the Enter sees nothing after it.
        let now = Instant::now();
        let mut guard = PasteGuard::default();
        assert_eq!(guard.text_for_key(key(KeyCode::Char('o')), true, now), None);
        assert_eq!(
            guard.text_for_key(key(KeyCode::Enter), false, now),
            None,
            "a plain Enter with nothing queued after it must submit"
        );
        // A real unbracketed paste with a break before more text still keeps its newline.
        let mut paste = PasteGuard::default();
        assert_eq!(paste.text_for_key(key(KeyCode::Char('a')), true, now), None);
        assert_eq!(
            paste.text_for_key(key(KeyCode::Enter), true, now),
            Some("\n")
        );
        assert_eq!(
            paste.text_for_key(key(KeyCode::Char('b')), false, now),
            None
        );
        assert_eq!(
            paste.text_for_key(key(KeyCode::Enter), false, now),
            Some("\n")
        );
    }
}
