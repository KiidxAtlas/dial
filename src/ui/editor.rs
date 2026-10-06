use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::text::sanitize;

/// The cursor is a byte offset, always at an extended-grapheme boundary.
#[derive(Default, Clone)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
}

impl Editor {
    pub fn set(&mut self, text: &str) {
        self.text = sanitize(text);
        self.cursor = self.text.len();
    }

    pub fn insert(&mut self, text: &str) {
        let clean = sanitize(text);
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        // Inserting a combining mark or ZWJ can join the neighboring graphemes.
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= self.cursor)
            .unwrap_or(self.text.len());
    }

    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    /// Move between logical lines, preserving display-cell column where possible.
    /// False means the caller can browse history at this edge of the draft.
    pub fn vertical(&mut self, up: bool) -> bool {
        let start = self.line_start();
        let column = UnicodeWidthStr::width(&self.text[start..self.cursor]);
        let (target_start, target_end) = if up {
            if start == 0 {
                return false;
            }
            let end = start - 1;
            (self.text[..end].rfind('\n').map_or(0, |i| i + 1), end)
        } else {
            let end = self.line_end();
            if end == self.text.len() {
                return false;
            }
            let start = end + 1;
            (
                start,
                self.text[start..]
                    .find('\n')
                    .map_or(self.text.len(), |i| start + i),
            )
        };
        let mut cells = 0;
        self.cursor = target_start;
        for g in self.text[target_start..target_end].graphemes(true) {
            let width = UnicodeWidthStr::width(g);
            if cells + width > column {
                break;
            }
            cells += width;
            self.cursor += g.len();
        }
        true
    }

    pub fn key(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Left => self.cursor = self.previous(),
            KeyCode::Right => self.cursor = self.next(),
            KeyCode::Home | KeyCode::Char('a') if key.code == KeyCode::Home || control => {
                self.cursor = self.line_start()
            }
            KeyCode::End | KeyCode::Char('e') if key.code == KeyCode::End || control => {
                self.cursor = self.line_end()
            }
            KeyCode::Backspace => {
                let previous = self.previous();
                self.text.replace_range(previous..self.cursor, "");
                self.cursor = previous;
            }
            KeyCode::Delete => {
                self.text.replace_range(self.cursor..self.next(), "");
            }
            KeyCode::Char('u') if control => {
                let start = self.line_start();
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Char('k') if control => {
                let end = self.line_end();
                let end = if end == self.cursor && end < self.text.len() {
                    end + 1
                } else {
                    end
                };
                self.text.replace_range(self.cursor..end, "");
            }
            KeyCode::Char('w') if control => {
                let end = self.cursor;
                while self.cursor > 0
                    && self.text[self.previous()..self.cursor]
                        .chars()
                        .all(char::is_whitespace)
                {
                    self.cursor = self.previous();
                }
                while self.cursor > 0
                    && !self.text[self.previous()..self.cursor]
                        .chars()
                        .all(char::is_whitespace)
                {
                    self.cursor = self.previous();
                }
                self.text.replace_range(self.cursor..end, "");
            }
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                let mut bytes = [0; 4];
                self.insert(c.encode_utf8(&mut bytes));
            }
            _ => return false,
        }
        // Deleting a newline or separator can merge the surrounding graphemes.
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= self.cursor)
            .unwrap_or(self.text.len());
        true
    }
}
