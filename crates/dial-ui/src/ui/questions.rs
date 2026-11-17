//! Typed question dialog. Previews are text/Markdown source, never fabricated graphics.
use super::{
    editor::Editor,
    keys::{Action, Keymap},
    text,
};
use crate::agent::workflow::questions::{self, QuestionAnswer, QuestionOutcome, QuestionRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Longest attachment path the path field accepts.
const MAX_PATH: usize = 4096;

/// Apply an editing key unless it would grow the text past `limit`. Deletion, cursor movement and
/// undo always work, so a field filled to its limit stays editable.
fn edit_within(editor: &mut Editor, key: KeyEvent, limit: usize, keys: &Keymap) {
    let before = editor.text.len();
    // A typed character adds at most four bytes; chords (yank, for example) can add more.
    let may_overflow = before + 4 > limit || !(key.modifiers - KeyModifiers::SHIFT).is_empty();
    if !may_overflow {
        editor.key_with(key, keys);
        return;
    }
    let mut trial = editor.clone();
    trial.key_with(key, keys);
    if trial.text.len() <= limit || trial.text.len() <= before {
        *editor = trial;
    }
}

/// Key names for the dialog's hint rows, taken from the keymap so remapped keys are advertised.
/// Arrow keys keep their arrow glyphs.
struct Hints {
    select: String,
    confirm: String,
    cancel: String,
    back: String,
    next: String,
    newline: String,
    attach: String,
    page: String,
    interrupt: String,
}

impl Hints {
    fn new(keys: &Keymap) -> Hints {
        let name = |action: Action| match keys.label(action).as_str() {
            "Up" => "↑".to_owned(),
            "Down" => "↓".to_owned(),
            "Left" => "←".to_owned(),
            "Right" => "→".to_owned(),
            other => other.to_owned(),
        };
        let (up, down) = (name(Action::SelectUp), name(Action::SelectDown));
        Hints {
            select: if up == "↑" && down == "↓" {
                "↑↓".to_owned()
            } else {
                [up, down]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("/")
            },
            confirm: name(Action::SelectConfirm),
            cancel: name(Action::SelectCancel),
            back: name(Action::TabPrev),
            next: name(Action::TabNext),
            newline: keys.reachable_label(Action::Newline, super::term::reports_key_releases()),
            attach: name(Action::PasteImage),
            page: [name(Action::SelectPageUp), name(Action::SelectPageDown)]
                .into_iter()
                .filter(|label| !label.is_empty())
                .collect::<Vec<_>>()
                .join("/"),
            interrupt: name(Action::Interrupt),
        }
    }
}

/// `"Esc cancel"`, or nothing when the action is unbound.
fn keyed(key: &str, what: &str) -> Option<String> {
    (!key.is_empty()).then(|| format!("{key} {what}"))
}

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
    /// Rendering clamps this presentation-only offset to the current wrapped content.
    preview_scroll: Cell<usize>,
    mode: Mode,
    presented: bool,
    deadline: Option<Instant>,
    paused_timeout: Option<Duration>,
    feedback: String,
    /// What a timeout chose for the user, one line per question.
    timeout_notes: Vec<String>,
    hints: Hints,
}
impl<'a> Dialog<'a> {
    pub fn new(request: &'a QuestionRequest, root: &Path) -> Self {
        Self {
            request,
            answers: questions::empty_answers(request),
            root: root.to_owned(),
            page: 0,
            cursor: request.questions[0].recommended.unwrap_or(0),
            preview_scroll: Cell::new(0),
            mode: Mode::Choices,
            presented: false,
            deadline: None,
            paused_timeout: None,
            feedback: String::new(),
            timeout_notes: Vec::new(),
            hints: Hints::new(&Keymap::defaults()),
        }
    }
    /// Advertise the user's key bindings (the dialog also picks them up from the first key).
    pub fn with_keys(mut self, keys: &Keymap) -> Self {
        self.hints = Hints::new(keys);
        self
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
    /// Inspection owns input without consuming question time or advancing answers.
    pub fn pause_timeout(&mut self, now: Instant) {
        if let Some(deadline) = self.deadline.take() {
            self.paused_timeout = Some(deadline.saturating_duration_since(now));
        }
    }

    pub fn resume_timeout(&mut self, now: Instant) -> Result<(), String> {
        if let Some(remaining) = self.paused_timeout {
            self.deadline = Some(
                now.checked_add(remaining)
                    .ok_or("ask timeout cannot be represented by the input clock")?,
            );
            self.paused_timeout = None;
        }
        Ok(())
    }

    pub fn expired(&mut self) -> Option<QuestionOutcome> {
        self.expired_at(Instant::now())
    }

    fn expired_at(&mut self, now: Instant) -> Option<QuestionOutcome> {
        if !matches!(self.mode, Mode::Choices)
            || !self.deadline.is_some_and(|deadline| now >= deadline)
        {
            return None;
        }
        let question = &self.request.questions[self.page];
        let answer = &mut self.answers[self.page];
        let mut note = None;
        if answer.selected_options.is_empty()
            && answer.custom_input.is_none()
            && let Some(option) = question.options.get(question.recommended.unwrap_or(0))
        {
            answer.selected_options.push(option.label.clone());
            note = Some(format!(
                "No answer in time to \u{201c}{}\u{201d}; chose \u{201c}{}\u{201d}.",
                text::one_line(&question.question),
                text::one_line(&option.label)
            ));
        }
        answer.timed_out = true;
        let page = self.page;
        let outcome = self.advance();
        if let Some(note) = note {
            // The next page says what was picked on the one that timed out.
            if self.page != page {
                self.feedback = note.clone();
            }
            self.timeout_notes.push(note);
        }
        outcome
    }

    /// One line per question the timeout answered on the user's behalf, for the transcript.
    pub fn timeout_notes(&self) -> &[String] {
        &self.timeout_notes
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
        self.preview_scroll.set(0);
        self.presented = false;
        self.deadline = None;
        self.paused_timeout = None;
        self.feedback.clear();
    }
    pub fn paste(&mut self, value: &str) {
        let clean = questions::sanitize(value);
        match &mut self.mode {
            Mode::Text(_, editor) if editor.text.len() + clean.len() <= questions::MAX_TEXT => {
                editor.insert(&clean)
            }
            Mode::Attach(_, _, editor) if editor.text.len() + clean.len() <= MAX_PATH => {
                editor.insert(&clean)
            }
            Mode::Text(..) | Mode::Attach(..) => {
                self.feedback =
                    "That paste is too long for this field; nothing was inserted.".into();
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
        self.hints = Hints::new(keys);
        let text_mode = matches!(self.mode, Mode::Text(..) | Mode::Attach(..));
        // Interrupt (Ctrl+C) cancels the turn from anywhere, and SelectCancel (Esc) cancels it from
        // the choices; inside a text field Esc only leaves the field.
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
                _ if keys.matches(Action::Newline, &key) => {
                    if editor.text.len() < questions::MAX_TEXT {
                        editor.insert("\n");
                    }
                    self.mode = Mode::Text(field, editor);
                }
                _ if keys.matches(Action::SelectConfirm, &key) => {
                    let answer = &mut self.answers[self.page];
                    match field {
                        Field::Custom => {
                            answer.custom_input = Some(questions::sanitize(&editor.text));
                            answer.timed_out = false;
                            if !self.request.questions[self.page].multi {
                                answer.selected_options.clear();
                            }
                            self.presented = false;
                            return self.advance();
                        }
                        Field::Note => answer.note = Some(questions::sanitize(&editor.text)),
                    }
                    self.presented = false;
                }
                // Ctrl+A stays line start; attaching an image path uses the attach-image key.
                _ if keys.matches(Action::PasteImage, &key) => {
                    self.mode = Mode::Attach(field, editor, Editor::default())
                }
                _ => {
                    edit_within(&mut editor, key, questions::MAX_TEXT, keys);
                    self.mode = Mode::Text(field, editor);
                }
            },
            Mode::Attach(field, mut answer_editor, mut path_editor) => match key.code {
                _ if keys.matches(Action::SelectCancel, &key) => {
                    self.mode = Mode::Text(field, answer_editor)
                }
                _ if keys.matches(Action::SelectConfirm, &key) => {
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
                    edit_within(&mut path_editor, key, MAX_PATH, keys);
                    self.mode = Mode::Attach(field, answer_editor, path_editor);
                }
            },
            Mode::Choices => {
                let count = self.request.questions[self.page].options.len();
                let toggle = keys.matches(Action::SelectConfirm, &key)
                    || (key.code == KeyCode::Char(' ') && key.modifiers.is_empty());
                match key.code {
                    _ if keys.matches(Action::SelectCancel, &key) => {
                        return Some(QuestionOutcome::Cancelled);
                    }
                    _ if keys.matches(Action::SelectUp, &key) => {
                        self.cursor = self.cursor.saturating_sub(1);
                        self.preview_scroll.set(0);
                    }
                    _ if keys.matches(Action::SelectDown, &key) => {
                        self.cursor = (self.cursor + 1).min(count + 3);
                        self.preview_scroll.set(0);
                    }
                    _ if keys.matches(Action::TabPrev, &key) => {
                        if self.page > 0 {
                            self.change_page(self.page - 1)
                        }
                    }
                    _ if keys.matches(Action::TabNext, &key) => return self.advance(),
                    _ if keys.matches(Action::SelectPageUp, &key) => self
                        .preview_scroll
                        .set(self.preview_scroll.get().saturating_sub(1)),
                    _ if keys.matches(Action::SelectPageDown, &key) => self
                        .preview_scroll
                        .set(self.preview_scroll.get().saturating_add(1)),
                    KeyCode::Char('o') if key.modifiers.is_empty() => {
                        self.start_text(Field::Custom)
                    }
                    KeyCode::Char('n') if key.modifiers.is_empty() => self.start_text(Field::Note),
                    KeyCode::Char('c') if key.modifiers.is_empty() => {
                        return Some(QuestionOutcome::Chat);
                    }
                    _ if toggle => {
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
    /// Lay out within the final panel dimensions, including [`framed`]'s inset and fill row.
    /// Essential controls and the active choice/input take precedence over secondary hints.
    pub fn rows(&self, width: usize, height: usize) -> Vec<String> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        // Match framed's inset and bottom fill before allocating any body rows. The active
        // control and essential keys are pinned; only explanatory content scrolls.
        let inner = if width >= 8 { width - 4 } else { width }.max(1);
        let capacity = height.saturating_sub(usize::from(width >= 8 && height > 1));
        let question = &self.request.questions[self.page];
        let answer = &self.answers[self.page];
        let total = self.request.questions.len();
        let header = question
            .header
            .as_deref()
            .map(text::one_line)
            .filter(|header| !header.trim().is_empty());
        let mut title = match (total, header) {
            (1, Some(header)) => header,
            (1, None) => "Question".to_owned(),
            (_, Some(header)) => format!("Question {}/{total} · {header}", self.page + 1),
            (_, None) => format!("Question {}/{total}", self.page + 1),
        };
        let mut context = text::wrap_words(&question.question, inner);
        let mut context_extent = context.len();
        let hints = &self.hints;
        let mut extra_keys = Vec::new();
        let mut choices = Vec::new();
        let mut input = Vec::new();
        let mut cursor_row = 0;
        let essential =
            match &self.mode {
                Mode::Choices => {
                    let count = question.options.len();
                    for index in 0..count + 4 {
                        let label = if let Some(option) = question.options.get(index) {
                            format!(
                                "{} {}{}",
                                if answer.selected_options.contains(&option.label) {
                                    "[x]"
                                } else {
                                    "[ ]"
                                },
                                text::one_line(&option.label),
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
                                _ if self.page + 1 == total => "Done selecting",
                                _ => "Next →",
                            }
                            .into()
                        };
                        choices.push(format!(
                            "{} {label}",
                            if index == self.cursor { ">" } else { " " }
                        ));
                    }
                    // Reserve the largest option's detail area, not the current option's.
                    // Otherwise every navigation key moves the choices and transcript above us.
                    let mut largest_detail = 0;
                    let mut selected_detail = 0;
                    for (index, option) in question.options.iter().enumerate() {
                        let mut detail_rows = Vec::new();
                        if let Some(detail) = &option.description {
                            detail_rows.extend(text::wrap_words(detail, inner));
                        }
                        if let Some(preview) = &option.preview {
                            detail_rows
                                .extend(text::wrap_words(&format!("Preview: {preview}"), inner));
                        }
                        largest_detail = largest_detail.max(detail_rows.len());
                        if index == self.cursor {
                            selected_detail = detail_rows.len();
                            context.extend(detail_rows);
                        }
                    }
                    if let Some(custom) = &answer.custom_input {
                        context.extend(text::wrap_words(&format!("Custom: {custom}"), inner));
                    }
                    if let Some(note) = &answer.note {
                        context.extend(text::wrap_words(&format!("Note: {note}"), inner));
                    }
                    if let Some(deadline) = self.deadline {
                        context.extend(text::wrap_words(&format!(
                        "Answers itself in {}s with your selection or the recommended choice",
                        deadline.saturating_duration_since(Instant::now()).as_secs()), inner));
                    }
                    context_extent = context.len() + largest_detail - selected_detail;
                    extra_keys.push(keyed(&hints.select, "choose"));
                    if self.page > 0 {
                        extra_keys.push(keyed(&hints.back, "back"));
                    }
                    extra_keys.push(keyed(
                        &hints.next,
                        if self.page + 1 == total {
                            "done"
                        } else {
                            "next"
                        },
                    ));
                    extra_keys.extend(["o other", "n note", "c chat"].map(|s| Some(s.to_owned())));
                    [
                        keyed(&hints.confirm, "select"),
                        keyed(&hints.cancel, "cancel"),
                    ]
                }
                Mode::Text(field, editor) => {
                    title.push_str(match field {
                        Field::Custom => " · Other/custom input",
                        Field::Note => " · Note",
                    });
                    (input, cursor_row) = input_rows(editor, inner.saturating_sub(4).max(1));
                    extra_keys.extend([
                        keyed(&hints.newline, "newline"),
                        keyed(&hints.attach, "attach image path"),
                        keyed(&hints.interrupt, "cancel turn"),
                    ]);
                    [
                        keyed(
                            &hints.confirm,
                            match field {
                                Field::Custom => "submit",
                                Field::Note => "save",
                            },
                        ),
                        keyed(&hints.cancel, "back"),
                    ]
                }
                Mode::Attach(_, _, editor) => {
                    title.push_str(" · Image path");
                    (input, cursor_row) = input_rows(editor, inner.saturating_sub(4).max(1));
                    extra_keys.push(keyed(&hints.interrupt, "cancel turn"));
                    [
                        keyed(&hints.confirm, "attach"),
                        keyed(&hints.cancel, "back"),
                    ]
                }
            };
        let mut footer = hint_rows(essential.into_iter().flatten(), inner);
        let body_capacity = capacity.saturating_sub(1);
        // Leave at least one active row even when every hint cannot fit.
        footer.truncate(body_capacity.saturating_sub(1));
        let mut room = body_capacity.saturating_sub(footer.len());
        let feedback = text::wrap_words(&self.feedback, inner);
        let feedback_count = if self.feedback.is_empty() {
            0
        } else {
            feedback.len().min(room.saturating_sub(1)).min(2)
        };
        room = room.saturating_sub(feedback_count);
        let mut rows = vec![title];
        if matches!(self.mode, Mode::Choices) {
            // One context row is enough to reach every line with page keys. Larger dialogs
            // also show neighbouring options; the selected option never leaves the viewport.
            let extra_rows = hint_rows(extra_keys.iter().flatten().cloned(), inner)
                .len()
                .min(room.saturating_sub(2));
            let choice_rows = room
                .saturating_sub(extra_rows + 1)
                .clamp(1, 5)
                .min(choices.len());
            let context_room = room.saturating_sub(extra_rows + choice_rows);
            let scroll_hint = keyed(&hints.page, "scroll details");
            if context_extent > context_room && room >= 3 {
                extra_keys.insert(0, scroll_hint);
            }
            let extras = hint_rows(extra_keys.into_iter().flatten(), inner);
            let extras_count = extras.len().min(room.saturating_sub(2));
            let choice_room = room
                .saturating_sub(extras_count)
                .saturating_sub(1)
                .clamp(1, 5)
                .min(choices.len());
            let context_room = room
                .saturating_sub(extras_count + choice_room)
                .min(context_extent);
            let start = self
                .preview_scroll
                .get()
                .min(context.len().saturating_sub(context_room.max(1)));
            self.preview_scroll.set(start);
            rows.extend(context.into_iter().skip(start).take(context_room));
            rows.resize(1 + context_room, String::new());
            let start = self
                .cursor
                .saturating_sub(choice_room / 2)
                .min(choices.len().saturating_sub(choice_room));
            rows.extend(choices.into_iter().skip(start).take(choice_room.min(room)));
            footer.splice(0..0, extras.into_iter().take(extras_count));
        } else {
            let extras = hint_rows(extra_keys.into_iter().flatten(), inner);
            let extras_count = extras.len().min(room.saturating_sub(3));
            let context_room = context.len().min(room.saturating_sub(extras_count + 3));
            rows.extend(context.into_iter().take(context_room));
            let input_room = room.saturating_sub(extras_count + context_room).max(1);
            let boxed = inner >= 8 && input_room >= 3;
            let content_room = input_room.saturating_sub(if boxed { 2 } else { 0 }).max(1);
            let start = cursor_row.saturating_sub(content_room.saturating_sub(1));
            if boxed {
                let label = match self.mode {
                    Mode::Text(Field::Custom, _) => "Your answer",
                    Mode::Text(Field::Note, _) => "Your note",
                    _ => "Image path",
                };
                let label = text::clip(label, inner.saturating_sub(5));
                rows.push(format!(
                    "┌─ {label} {}┐",
                    "─".repeat(inner - text::width(&label) - 5)
                ));
            }
            for line in input.into_iter().skip(start).take(content_room.min(room)) {
                if inner >= 8 {
                    let pad = inner.saturating_sub(4 + text::width(&line));
                    rows.push(format!("│ {line}{} │", " ".repeat(pad)));
                } else {
                    rows.push(line);
                }
            }
            if boxed {
                rows.push(format!("└{}┘", "─".repeat(inner - 2)));
            }
            footer.splice(0..0, extras.into_iter().take(extras_count));
        }
        rows.extend(feedback.into_iter().take(feedback_count));
        rows.extend(footer);
        rows.truncate(capacity);
        rows
    }
}

/// Keep each binding next to its action when packing narrow footer rows.
fn hint_rows(hints: impl IntoIterator<Item = String>, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for hint in hints {
        if let Some(last) = rows.last_mut()
            && text::width(last) + 3 + text::width(&hint) <= width
        {
            last.push_str(" · ");
            last.push_str(&hint);
        } else {
            rows.extend(text::wrap_words(&hint, width));
        }
    }
    rows
}

/// Wrap both fields with the cursor as a distinct span, so pasted cursor glyphs and word
/// wrapping cannot misidentify the active row. Hard wrapping preserves input whitespace.
fn input_rows(editor: &Editor, width: usize) -> (Vec<String>, usize) {
    use ratatui::{
        style::{Modifier, Style},
        text::{Line, Span},
    };
    let mut paragraphs: Vec<Line<'static>> = editor.text[..editor.cursor]
        .split('\n')
        .map(|line| Line::from(line.to_owned()))
        .collect();
    let active = paragraphs
        .last_mut()
        .expect("split always yields one paragraph");
    active.spans.push(Span::styled(
        "│",
        Style::default().add_modifier(Modifier::REVERSED),
    ));
    let mut after = editor.text[editor.cursor..].split('\n');
    active
        .spans
        .push(Span::raw(after.next().unwrap_or_default().to_owned()));
    paragraphs.extend(after.map(|line| Line::from(line.to_owned())));
    let wrapped = text::wrap(paragraphs, width);
    let cursor = wrapped
        .iter()
        .position(|line| {
            line.spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::REVERSED))
        })
        .unwrap_or(0);
    (wrapped.iter().map(ToString::to_string).collect(), cursor)
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
        // Input boundaries are emitted by rows(), including on cramped viewports where only
        // the side rails fit. Keep editable text at normal contrast, unlike explanatory copy.
        if (row.starts_with("│ ") && row.ends_with(" │"))
            || (row.starts_with("┌─") && row.ends_with('┐'))
            || (row.starts_with('└') && row.ends_with('┘'))
        {
            let input = row.starts_with('│');
            let row = if ascii {
                if input {
                    format!("|{}|", &row['│'.len_utf8()..row.len() - '│'.len_utf8()])
                } else {
                    row.replace(['┌', '┐', '└', '┘'], "+").replace('─', "-")
                }
            } else {
                row
            };
            let row = text::clip(&row, inner);
            let pad = inner.saturating_sub(text::width(&row));
            out.push(Line::from(super::chrome::on_surface(
                theme,
                vec![
                    Span::raw("  "),
                    Span::styled(
                        row,
                        theme.fg(if input { Tok::Text } else { Tok::BorderAccent }),
                    ),
                    Span::raw(" ".repeat(pad + 2)),
                ],
            )));
            continue;
        }
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
