//! Single-buffer text editor shared by the composer and every dialog text field.
//!
//! Editing operations are [`Action`]s from the keymap (`tui.editor.*`), so they are rebindable and
//! listed in `/hotkeys`. Typing coalesces into word-sized undo units, kills feed an Emacs-style kill
//! ring with yank and yank-pop.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::LazyLock;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    keys::{Action, Keymap},
    text::sanitize,
};

const MAX_UNDO: usize = 100;
const MAX_KILLS: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    text: String,
    cursor: usize,
}

/// What the previous editing command was; drives undo coalescing, kill accumulation and yank-pop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Last {
    #[default]
    Other,
    TypeWord,
    Kill,
    Yank {
        start: usize,
        end: usize,
    },
}

/// Ring of killed text. Consecutive kills accumulate into one entry; yank-pop rotates.
#[derive(Clone, Debug, Default)]
pub struct KillRing {
    ring: Vec<String>,
}

impl KillRing {
    /// `prepend` is for backward kills, so `Alt+Backspace` twice yields the text in reading order.
    pub fn push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        match self.ring.last_mut() {
            Some(last) if accumulate => {
                if prepend {
                    last.insert_str(0, text);
                } else {
                    last.push_str(text);
                }
            }
            _ => {
                self.ring.push(text.to_owned());
                if self.ring.len() > MAX_KILLS {
                    self.ring.remove(0);
                }
            }
        }
    }

    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(String::as_str)
    }

    /// Move the newest entry to the oldest slot, exposing the previous one.
    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            let last = self.ring.pop().unwrap_or_default();
            self.ring.insert(0, last);
        }
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// The cursor is a byte offset, always at an extended-grapheme boundary.
#[derive(Default, Clone)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last: Last,
    kills: KillRing,
    /// Pending jump-to-character: `Some(true)` forward, `Some(false)` backward.
    jump: Option<bool>,
}

static DEFAULT_KEYS: LazyLock<Keymap> = LazyLock::new(Keymap::defaults);

fn default_keymap() -> &'static Keymap {
    &DEFAULT_KEYS
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WordKind {
    Space,
    Word,
    Cjk,
    Delimiter,
}

fn word_kind(grapheme: &str) -> WordKind {
    let Some(c) = grapheme.chars().next() else {
        return WordKind::Delimiter;
    };
    if c.is_whitespace() {
        WordKind::Space
    } else if matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
    {
        WordKind::Cjk
    } else if c == '_' || c.is_alphanumeric() {
        WordKind::Word
    } else {
        WordKind::Delimiter
    }
}

fn is_joiner(grapheme: &str) -> bool {
    matches!(grapheme, "'" | "\u{2019}" | "-" | "\u{2010}" | "\u{2011}")
}

/// Start of the word before `cursor`: skip whitespace, then one run of the same kind. Apostrophes and
/// hyphens between letters stay inside a word (`don't`, `well-known`).
pub(crate) fn word_left(text: &str, cursor: usize) -> usize {
    let marks: Vec<(usize, &str)> = text[..cursor].grapheme_indices(true).collect();
    let mut at = marks.len();
    while at > 0 && word_kind(marks[at - 1].1) == WordKind::Space {
        at -= 1;
    }
    if at == 0 {
        return 0;
    }
    let kind = word_kind(marks[at - 1].1);
    while at > 0 {
        let g = marks[at - 1].1;
        let joined = kind == WordKind::Word
            && is_joiner(g)
            && at >= 2
            && word_kind(marks[at - 2].1) == WordKind::Word
            && marks
                .get(at)
                .is_some_and(|(_, n)| word_kind(n) == WordKind::Word);
        if word_kind(g) != kind && !joined {
            break;
        }
        at -= 1;
    }
    marks.get(at).map_or(cursor, |(i, _)| *i)
}

/// End of the word after `cursor`, mirroring [`word_left`].
pub(crate) fn word_right(text: &str, cursor: usize) -> usize {
    let marks: Vec<(usize, &str)> = text[cursor..].grapheme_indices(true).collect();
    let mut at = 0;
    while at < marks.len() && word_kind(marks[at].1) == WordKind::Space {
        at += 1;
    }
    if at == marks.len() {
        return text.len();
    }
    let kind = word_kind(marks[at].1);
    while at < marks.len() {
        let g = marks[at].1;
        let joined = kind == WordKind::Word
            && is_joiner(g)
            && at >= 1
            && word_kind(marks[at - 1].1) == WordKind::Word
            && marks
                .get(at + 1)
                .is_some_and(|(_, n)| word_kind(n) == WordKind::Word);
        if word_kind(g) != kind && !joined {
            break;
        }
        at += 1;
    }
    marks.get(at).map_or(text.len(), |(i, _)| cursor + i)
}

fn prev_boundary(text: &str, at: usize) -> usize {
    text[..at]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(i, _)| i)
}

fn next_boundary(text: &str, at: usize) -> usize {
    text[at..]
        .graphemes(true)
        .next()
        .map_or(at, |g| at + g.len())
}

/// The first grapheme boundary at or after `at`.
fn snap(text: &str, at: usize) -> usize {
    text.grapheme_indices(true)
        .map(|(i, _)| i)
        .find(|&i| i >= at)
        .unwrap_or(text.len())
}

fn line_start_of(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end_of(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i)
}

impl Editor {
    /// Replace the text (cursor at the end) as one undoable change.
    pub fn set(&mut self, text: &str) {
        let clean = sanitize(text);
        if clean != self.text {
            self.record();
        }
        self.text = clean;
        self.cursor = self.text.len();
        self.last = Last::Other;
        self.jump = None;
    }

    /// Whether text can return through the buffer, undo/redo or the bounded kill ring.
    /// Compound inputs use this to retain payloads only while their markers are recoverable.
    pub(super) fn references(&self, marker: &str) -> bool {
        self.text.contains(marker)
            || self
                .undo
                .iter()
                .chain(&self.redo)
                .any(|s| s.text.contains(marker))
            || self.kills.ring.iter().any(|text| text.contains(marker))
    }

    /// Insert at the cursor as one undo unit.
    pub fn insert(&mut self, text: &str) {
        let clean = sanitize(text);
        if clean.is_empty() {
            return;
        }
        self.record();
        self.insert_raw(&clean);
        self.last = Last::Other;
    }

    fn insert_raw(&mut self, clean: &str) {
        self.text.insert_str(self.cursor, clean);
        self.cursor += clean.len();
        // Inserting a combining mark or ZWJ can join the neighboring graphemes.
        self.cursor = snap(&self.text, self.cursor);
    }

    fn record(&mut self) {
        let snapshot = Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        };
        if self.undo.last() != Some(&snapshot) {
            self.undo.push(snapshot);
            if self.undo.len() > MAX_UNDO {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    #[cfg(test)]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop() else {
            return false;
        };
        self.redo.push(Snapshot {
            text: std::mem::replace(&mut self.text, snapshot.text),
            cursor: self.cursor,
        });
        self.cursor = snap(&self.text, snapshot.cursor.min(self.text.len()));
        self.last = Last::Other;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo.pop() else {
            return false;
        };
        self.undo.push(Snapshot {
            text: std::mem::replace(&mut self.text, snapshot.text),
            cursor: self.cursor,
        });
        self.cursor = snap(&self.text, snapshot.cursor.min(self.text.len()));
        self.last = Last::Other;
        true
    }

    #[cfg(test)]
    pub fn kill_ring(&self) -> &KillRing {
        &self.kills
    }

    fn previous(&self) -> usize {
        prev_boundary(&self.text, self.cursor)
    }

    fn next(&self) -> usize {
        next_boundary(&self.text, self.cursor)
    }

    fn line_start(&self) -> usize {
        line_start_of(&self.text, self.cursor)
    }

    fn line_end(&self) -> usize {
        line_end_of(&self.text, self.cursor)
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
        self.last = Last::Other;
        true
    }

    /// Handle a key with the default bindings (dialog text fields).
    pub fn key(&mut self, key: KeyEvent) -> bool {
        self.key_with(key, default_keymap())
    }

    /// Handle a key with the user's bindings. False means the key was not an editing key.
    pub fn key_with(&mut self, key: KeyEvent, keys: &Keymap) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if let Some(forward) = self.jump.take() {
            if let KeyCode::Char(c) = key.code
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                self.jump_to(c, forward);
            }
            // Any other key cancels the pending jump without acting.
            return true;
        }
        if let Some(action) = Action::EDITOR
            .iter()
            .copied()
            .find(|action| keys.matches(*action, &key))
        {
            return self.apply(action);
        }
        match key.code {
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.type_char(c);
                true
            }
            _ => false,
        }
    }

    /// Typing coalesces: a run of word characters is one undo unit; whitespace and punctuation
    /// start a new one.
    fn type_char(&mut self, c: char) {
        let mut bytes = [0; 4];
        let clean = sanitize(c.encode_utf8(&mut bytes));
        if clean.is_empty() {
            return;
        }
        let word = !c.is_whitespace();
        if word && self.last == Last::TypeWord {
            self.redo.clear();
        } else {
            self.record();
        }
        self.insert_raw(&clean);
        self.last = if word { Last::TypeWord } else { Last::Other };
    }

    /// Run one editor action. Returns true when the action is an editing action (even a no-op).
    pub fn apply(&mut self, action: Action) -> bool {
        let last = std::mem::take(&mut self.last);
        match action {
            Action::EditorLeft => self.cursor = self.previous(),
            Action::EditorRight => self.cursor = self.next(),
            Action::EditorWordLeft => self.cursor = word_left(&self.text, self.cursor),
            Action::EditorWordRight => self.cursor = word_right(&self.text, self.cursor),
            Action::EditorLineStart => self.cursor = self.line_start(),
            Action::EditorLineEnd => self.cursor = self.line_end(),
            Action::EditorJumpForward => self.jump = Some(true),
            Action::EditorJumpBackward => self.jump = Some(false),
            Action::EditorDeleteBackward => {
                let start = self.previous();
                self.delete(start, self.cursor);
            }
            Action::EditorDeleteForward => {
                let end = self.next();
                self.delete(self.cursor, end);
            }
            Action::EditorDeleteWordBackward => {
                let start = word_left(&self.text, self.cursor);
                self.kill(start, self.cursor, true, last);
            }
            Action::EditorDeleteWordForward => {
                let end = word_right(&self.text, self.cursor);
                self.kill(self.cursor, end, false, last);
            }
            Action::EditorDeleteToLineStart => {
                let start = self.line_start();
                self.kill(start, self.cursor, true, last);
            }
            Action::EditorDeleteToLineEnd => {
                let end = self.line_end();
                // At the end of a line, join the next one (readline).
                let end = if end == self.cursor && end < self.text.len() {
                    end + 1
                } else {
                    end
                };
                self.kill(self.cursor, end, false, last);
            }
            Action::EditorYank => self.yank(),
            Action::EditorYankPop => self.yank_pop(last),
            Action::EditorUndo => {
                self.undo();
            }
            Action::EditorRedo => {
                self.redo();
            }
            _ => {
                self.last = last;
                return false;
            }
        }
        // Deleting a newline or separator can merge the surrounding graphemes.
        self.cursor = snap(&self.text, self.cursor.min(self.text.len()));
        true
    }

    fn delete(&mut self, start: usize, end: usize) {
        if start < end {
            self.record();
            self.text.replace_range(start..end, "");
            self.cursor = start;
        }
    }

    fn kill(&mut self, start: usize, end: usize, backward: bool, last: Last) {
        if start >= end {
            self.last = if last == Last::Kill {
                Last::Kill
            } else {
                Last::Other
            };
            return;
        }
        self.record();
        let killed: String = self.text.drain(start..end).collect();
        self.kills.push(&killed, backward, last == Last::Kill);
        self.cursor = start;
        self.last = Last::Kill;
    }

    fn yank(&mut self) {
        let Some(text) = self.kills.peek().map(str::to_owned) else {
            return;
        };
        self.record();
        let start = self.cursor;
        self.text.insert_str(start, &text);
        self.cursor = start + text.len();
        self.last = Last::Yank {
            start,
            end: self.cursor,
        };
    }

    /// Replace the text just yanked with the previous kill-ring entry.
    fn yank_pop(&mut self, last: Last) {
        let Last::Yank { start, end } = last else {
            return;
        };
        let current = self.kills.peek().unwrap_or_default();
        if self.kills.len() < 2
            || end > self.text.len()
            || !self.text.is_char_boundary(start)
            || !self.text.is_char_boundary(end)
            || self.text[start..end] != *current
        {
            self.last = last;
            return;
        }
        self.record();
        self.kills.rotate();
        let replacement = self.kills.peek().unwrap_or_default().to_owned();
        self.text.replace_range(start..end, &replacement);
        self.cursor = start + replacement.len();
        self.last = Last::Yank {
            start,
            end: self.cursor,
        };
    }

    /// Jump to the next (or previous) occurrence of `c`, across lines. No match leaves the cursor.
    fn jump_to(&mut self, c: char, forward: bool) {
        self.last = Last::Other;
        let found = if forward {
            let from = self.next();
            self.text[from..].find(c).map(|i| from + i)
        } else {
            self.text[..self.cursor].rfind(c)
        };
        if let Some(at) = found {
            self.cursor = snap(&self.text, at);
        }
    }
}

#[cfg(test)]
mod tests;
