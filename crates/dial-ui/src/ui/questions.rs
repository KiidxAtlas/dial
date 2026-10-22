//! Typed question dialog. Previews are text/Markdown source, never fabricated graphics.
use super::{
    editor::Editor,
    keys::{Action, Keymap},
    text,
};
use crate::agent::workflow::questions::{self, QuestionAnswer, QuestionOutcome, QuestionRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
enum Field {
    Custom,
    Note,
}
enum Mode {
    Choices,
    Text(Field, Editor),
    Attach(Field, Editor, Editor),
}
pub struct Dialog<'a> {
    request: &'a QuestionRequest,
    answers: Vec<QuestionAnswer>,
    root: PathBuf,
    page: usize,
    cursor: usize,
    preview_scroll: usize,
    mode: Mode,
    presented: bool,
    deadline: Option<Instant>,
    feedback: String,
}
impl<'a> Dialog<'a> {
    pub fn new(request: &'a QuestionRequest, root: &Path) -> Self {
        Self {
            request,
            answers: questions::empty_answers(request),
            root: root.to_owned(),
            page: 0,
            cursor: request.questions[0].recommended.unwrap_or(0),
            preview_scroll: 0,
            mode: Mode::Choices,
            presented: false,
            deadline: None,
            feedback: String::new(),
        }
    }
    pub fn presented(&mut self) -> Result<(), String> {
        if !self.presented {
            self.presented = true;
            self.arm()?;
        }
        Ok(())
    }
    fn arm(&mut self) -> Result<(), String> {
        self.deadline = if self.request.timeout_seconds == 0 {
            None
        } else {
            Some(
                Instant::now()
                    .checked_add(Duration::from_secs(self.request.timeout_seconds))
                    .ok_or("ask timeout cannot be represented by the input clock")?,
            )
        };
        Ok(())
    }
    pub fn expired(&mut self) -> Option<QuestionOutcome> {
        if !matches!(self.mode, Mode::Choices)
            || !self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return None;
        }
        let question = &self.request.questions[self.page];
        let answer = &mut self.answers[self.page];
        if answer.selected_options.is_empty()
            && answer.custom_input.is_none()
            && let Some(option) = question.options.get(question.recommended.unwrap_or(0))
        {
            answer.selected_options.push(option.label.clone());
        }
        answer.timed_out = true;
        self.advance()
    }
    fn advance(&mut self) -> Option<QuestionOutcome> {
        let question = &self.request.questions[self.page];
        let answer = &self.answers[self.page];
        if !question.multi
            && answer.selected_options.is_empty()
            && answer.custom_input.is_none()
            && !answer.timed_out
        {
            self.feedback =
                "Choose an option or provide Other/custom input before continuing.".into();
            return None;
        }
        if self.page + 1 == self.request.questions.len() {
            let mut answers = self.answers.clone();
            match questions::validate_answers(self.request, &mut answers) {
                Ok(()) => Some(QuestionOutcome::Answered(answers)),
                Err(error) => {
                    self.feedback = error;
                    None
                }
            }
        } else {
            self.change_page(self.page + 1);
            None
        }
    }
    fn change_page(&mut self, page: usize) {
        self.page = page;
        self.cursor = self.request.questions[page].recommended.unwrap_or(0);
        self.preview_scroll = 0;
        self.presented = false;
        self.deadline = None;
        self.feedback.clear();
    }
    pub fn paste(&mut self, value: &str) {
        let clean = questions::sanitize(value);
        match &mut self.mode {
            Mode::Text(_, editor) | Mode::Attach(_, _, editor)
                if editor.text.len() + clean.len() <= questions::MAX_TEXT =>
            {
                editor.insert(&clean)
            }
            _ => {}
        }
    }
    fn start_text(&mut self, field: Field) {
        let answer = &self.answers[self.page];
        let prefill = match field {
            Field::Custom => &answer.custom_input,
            Field::Note => &answer.note,
        };
        let mut editor = Editor::default();
        if let Some(text) = prefill {
            editor.insert(text);
        }
        self.mode = Mode::Text(field, editor);
    }
    pub fn key(&mut self, key: KeyEvent, keys: &Keymap) -> Option<QuestionOutcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let text_mode = matches!(self.mode, Mode::Text(..) | Mode::Attach(..));
        // Interrupt (Ctrl+C, and Esc from the choices) cancels the turn; inside a text field Esc only
        // leaves the field, which is SelectCancel.
        if keys.matches(Action::Interrupt, &key)
            && !(text_mode && keys.matches(Action::SelectCancel, &key))
        {
            return Some(QuestionOutcome::Cancelled);
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Choices);
        match mode {
            Mode::Text(field, mut editor) => match key.code {
                _ if keys.matches(Action::SelectCancel, &key) => {
                    self.presented = false;
                }
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    let answer = &mut self.answers[self.page];
                    match field {
                        Field::Custom => {
                            answer.custom_input = Some(questions::sanitize(&editor.text));
                            answer.timed_out = false;
                            if !self.request.questions[self.page].multi {
                                answer.selected_options.clear();
                            }
                        }
                        Field::Note => answer.note = Some(questions::sanitize(&editor.text)),
                    }
                    self.presented = false;
                }
                KeyCode::Enter => {
                    if editor.text.len() < questions::MAX_TEXT {
                        editor.insert("\n");
                    }
                    self.mode = Mode::Text(field, editor);
                }
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.mode = Mode::Attach(field, editor, Editor::default())
                }
                _ => {
                    if editor.text.len() < questions::MAX_TEXT {
                        editor.key(key);
                    }
                    self.mode = Mode::Text(field, editor);
                }
            },
            Mode::Attach(field, mut answer_editor, mut path_editor) => match key.code {
                _ if keys.matches(Action::SelectCancel, &key) => {
                    self.mode = Mode::Text(field, answer_editor)
                }
                KeyCode::Enter => {
                    let count: usize = self
                        .answers
                        .iter()
                        .map(|answer| answer.custom_input_images.len() + answer.note_images.len())
                        .sum();
                    if count >= questions::MAX_IMAGES {
                        self.feedback =
                            "At most 16 images across all question answers are supported.".into();
                        self.mode = Mode::Attach(field, answer_editor, path_editor);
                        return None;
                    }
                    let answer = &mut self.answers[self.page];
                    let images = match field {
                        Field::Custom => &mut answer.custom_input_images,
                        Field::Note => &mut answer.note_images,
                    };
                    match questions::attach(
                        &self.root,
                        &mut answer_editor.text,
                        images,
                        path_editor.text.trim(),
                    ) {
                        Ok(()) => {
                            answer_editor.cursor = answer_editor.text.len();
                            self.feedback = "Human image reference attached.".into();
                            self.mode = Mode::Text(field, answer_editor);
                        }
                        Err(error) => {
                            self.feedback = error;
                            self.mode = Mode::Attach(field, answer_editor, path_editor);
                        }
                    }
                }
                _ => {
                    if path_editor.text.len() < 4096 {
                        path_editor.key(key);
                    }
                    self.mode = Mode::Attach(field, answer_editor, path_editor);
                }
            },
            Mode::Choices => {
                let count = self.request.questions[self.page].options.len();
                match key.code {
                    KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
                    KeyCode::Down => self.cursor = (self.cursor + 1).min(count + 3),
                    KeyCode::Left if self.page > 0 => self.change_page(self.page - 1),
                    KeyCode::Right => return self.advance(),
                    KeyCode::PageUp => self.preview_scroll = self.preview_scroll.saturating_sub(5),
                    KeyCode::PageDown => {
                        self.preview_scroll = self.preview_scroll.saturating_add(5)
                    }
                    KeyCode::Char('o') => self.start_text(Field::Custom),
                    KeyCode::Char('n') => self.start_text(Field::Note),
                    KeyCode::Char('c') => return Some(QuestionOutcome::Chat),
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        if self.cursor < count {
                            let question = &self.request.questions[self.page];
                            let label = question.options[self.cursor].label.clone();
                            let answer = &mut self.answers[self.page];
                            answer.timed_out = false;
                            if question.multi {
                                if let Some(index) = answer
                                    .selected_options
                                    .iter()
                                    .position(|value| *value == label)
                                {
                                    answer.selected_options.remove(index);
                                } else {
                                    answer.selected_options.push(label);
                                }
                            } else {
                                answer.selected_options = vec![label];
                                answer.custom_input = None;
                                answer.custom_input_images.clear();
                                self.cursor = count + 3;
                            }
                        } else {
                            match self.cursor - count {
                                0 => self.start_text(Field::Custom),
                                1 => self.start_text(Field::Note),
                                2 => return Some(QuestionOutcome::Chat),
                                _ => return self.advance(),
                            }
                        }
                    }
                    _ => {}
                }
                if self.presented
                    && let Err(error) = self.arm()
                {
                    return Some(QuestionOutcome::Unsupported(error));
                }
            }
        }
        None
    }
    pub fn rows(&self, width: usize, height: usize) -> Vec<String> {
        let question = &self.request.questions[self.page];
        let answer = &self.answers[self.page];
        // The first row is the panel title; the model's internal question id is not shown. A lone
        // question needs no `1/1` counter: its header (if any) titles the panel instead.
        let total = self.request.questions.len();
        let header = question
            .header
            .as_deref()
            .map(text::one_line)
            .filter(|header| !header.trim().is_empty());
        let mut rows = vec![match (total, header) {
            (1, Some(header)) => header,
            (1, None) => "Question".to_owned(),
            (_, Some(header)) => format!("Question {}/{total} · {header}", self.page + 1),
            (_, None) => format!("Question {}/{total}", self.page + 1),
        }];
        rows.extend(
            text::wrap_words(&question.question, width.max(1))
                .into_iter()
                .take(3),
        );
        match &self.mode {
            Mode::Text(field, editor) => {
                rows.push(
                    match field {
                        Field::Custom => "Other/custom input",
                        Field::Note => "Note",
                    }
                    .into(),
                );
                rows.push("Enter saves · Shift+Enter newline · Ctrl+A attach image path · Esc returns · Ctrl+C cancels turn".into());
                let before = &editor.text[..editor.cursor];
                let after = &editor.text[editor.cursor..];
                let lines = text::wrap_words(&format!("{before}│{after}"), width.max(1));
                let cursor_row = text::wrap_words(before, width.max(1))
                    .len()
                    .saturating_sub(1);
                rows.extend(
                    lines
                        .into_iter()
                        .skip(cursor_row.saturating_sub(height.saturating_sub(7) / 2))
                        .take(height.saturating_sub(6).max(1)),
                );
            }
            Mode::Attach(_, _, editor) => {
                rows.push(
                    "Explicit human image path (PNG/JPEG/GIF/WebP; no clipboard ingestion)".into(),
                );
                rows.push("Enter attaches verified reference · Esc returns".into());
                rows.push(editor.text.clone());
            }
            Mode::Choices => {
                let available = height.saturating_sub(rows.len() + 9).max(3);
                let count = question.options.len();
                let start = self
                    .cursor
                    .saturating_sub(available / 2)
                    .min((count + 4).saturating_sub(available));
                for index in start..(start + available).min(count + 4) {
                    let label = if index < count {
                        let option = &question.options[index];
                        format!(
                            "{} {}{}",
                            if answer.selected_options.contains(&option.label) {
                                "[x]"
                            } else {
                                "[ ]"
                            },
                            option.label,
                            if question.recommended == Some(index) {
                                " (Recommended)"
                            } else {
                                ""
                            }
                        )
                    } else {
                        match index - count {
                            0 => "Other (type your own)",
                            1 => "Add/edit note",
                            2 => "Chat about this",
                            _ if self.page + 1 == self.request.questions.len() => "Done selecting",
                            _ => "Next →",
                        }
                        .into()
                    };
                    rows.push(format!(
                        "{} {label}",
                        if index == self.cursor { ">" } else { " " }
                    ));
                }
                let option = question.options.get(self.cursor);
                let detail = option
                    .and_then(|option| option.description.as_deref())
                    .unwrap_or("");
                let preview = option
                    .and_then(|option| option.preview.as_deref())
                    .unwrap_or("");
                // Only what the highlighted option adds: its description, then its preview.
                let mut extra = String::new();
                if !detail.trim().is_empty() {
                    extra.push_str(detail.trim());
                }
                if !preview.trim().is_empty() {
                    if !extra.is_empty() {
                        extra.push('\n');
                    }
                    extra.push_str(&format!("Preview: {}", preview.trim()));
                }
                if !extra.is_empty() {
                    let lines = text::wrap_words(&extra, width.max(1));
                    rows.extend(lines.into_iter().skip(self.preview_scroll).take(4));
                }
                if let Some(custom) = &answer.custom_input {
                    rows.push(format!("Custom: {}", text::one_line(custom)));
                }
                if let Some(note) = &answer.note {
                    rows.push(format!("Note: {}", text::one_line(note)));
                }
                // Only keys that do something on this page are advertised.
                let mut keys = vec!["↑↓ choose", "Enter select"];
                if self.page > 0 {
                    keys.push("← back");
                }
                keys.push(if self.page + 1 == total {
                    "→ done"
                } else {
                    "→ next"
                });
                keys.extend(["o other", "n note", "c chat", "Esc cancel"]);
                rows.push(keys.join(" · "));
            }
        }
        if !self.feedback.is_empty() {
            rows.push(self.feedback.clone());
        }
        if let Some(deadline) = self.deadline
            && matches!(self.mode, Mode::Choices)
        {
            rows.push(format!(
                "Answers itself in {}s with your selection or the recommended choice",
                deadline.saturating_duration_since(Instant::now()).as_secs()
            ));
        }
        rows
    }
}

/// [`Dialog::rows`] as an inset panel on the panel surface: the first row becomes the bold
/// `Question n/m` title, the highlighted choice gets the selection fill, check boxes become radio
/// marks (unless ASCII), and the key-hint row is muted. At most `height` rows, each exactly
/// `width` cells.
pub fn framed(
    rows: Vec<String>,
    width: usize,
    height: usize,
    theme: &super::theme::Theme,
) -> Vec<ratatui::text::Line<'static>> {
    use super::theme::{Sym, Tok};
    use ratatui::{
        style::Modifier,
        text::{Line, Span},
    };
    if height == 0 || width == 0 || rows.is_empty() {
        return Vec::new();
    }
    // There is no room for a frame on one row. Never exceed the caller's viewport.
    if height == 1 {
        return vec![Line::from(text::clip(&text::one_line(&rows[0]), width))];
    }
    if width < 8 {
        return rows
            .into_iter()
            .map(|row| Line::from(text::clip(&text::one_line(&row), width)))
            .take(height)
            .collect();
    }
    let ascii = theme.ascii();
    let inner = width - 4;
    let fill = super::chrome::surface(theme);
    let title = text::clip(&text::one_line(&rows[0]), inner);
    let mut out = vec![Line::from(super::chrome::on_surface(
        theme,
        vec![
            Span::raw(" "),
            Span::styled(
                title.clone(),
                theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(width.saturating_sub(1 + text::width(&title)))),
        ],
    ))];
    let selected_row = rows.iter().position(|row| row.starts_with("> "));
    let last = rows.len() - 1;
    for (index, row) in rows.into_iter().enumerate().skip(1) {
        let row = text::one_line(&row);
        let footer = index == last && row.contains(" · ") && !row.starts_with('>');
        let (selected, body) = match row.strip_prefix("> ") {
            Some(rest) => (true, rest.to_owned()),
            None => (
                false,
                row.strip_prefix("  ").map_or(row.clone(), str::to_owned),
            ),
        };
        let body = if ascii {
            body
        } else if let Some(rest) = body.strip_prefix("[x] ") {
            format!("\u{25c9} {rest}")
        } else if let Some(rest) = body.strip_prefix("[ ] ") {
            format!("\u{25cb} {rest}")
        } else {
            body
        };
        let choice = selected || row.starts_with("  ");
        let pointer = if selected {
            theme.sym(Sym::Pointer)
        } else {
            " "
        };
        let text_style = if footer {
            theme.fg(Tok::Muted)
        } else if selected {
            super::chrome::selected_style(theme)
        } else if index == 1 {
            theme.fg(Tok::Text).add_modifier(Modifier::BOLD)
        } else if choice {
            theme.fg(Tok::Text)
        } else {
            theme.fg(Tok::Muted)
        };
        let lead = if choice {
            format!("{pointer} ")
        } else {
            String::new()
        };
        let content = text::clip(&format!("{lead}{body}"), inner);
        let pad = inner.saturating_sub(text::width(&content));
        out.push(Line::from(super::chrome::on_surface(
            theme,
            vec![
                Span::raw("  "),
                Span::styled(content, text_style),
                Span::styled(
                    " ".repeat(pad),
                    if selected {
                        text_style
                    } else {
                        Default::default()
                    },
                ),
                Span::raw("  "),
            ],
        )));
    }
    out.push(Line::from(Span::styled(" ".repeat(width), fill)));
    if out.len() > height {
        // Window the body around the choice being acted on, not just the top of the dialog.
        // Otherwise Enter can select an option that the user cannot see on a short terminal.
        let bottom = out.pop().expect("the panel has a bottom row");
        let room = height - 2;
        let start = selected_row
            .filter(|&index| index > room)
            .map_or(1, |index| {
                index.saturating_sub(room.saturating_sub(1)).max(1)
            });
        let end = (start + room).min(out.len());
        let mut visible = vec![out[0].clone()];
        visible.extend(out.drain(start..end));
        visible.push(bottom);
        out = visible;
    }
    out
}

#[cfg(test)]
mod tests;
