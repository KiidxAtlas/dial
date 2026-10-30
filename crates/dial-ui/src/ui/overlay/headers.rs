use super::settings::{Axis, arrows, editor_spans, filled, fit, is_ctrl, typed_char};
use crate::{
    llm::model_config::{HeaderValue, RequestHeader, RequestHeaders, validate_headers},
    ui::{
        chrome,
        editor::Editor,
        keys::{Action, Keymap},
        theme::{Sym, Theme, Tok},
    },
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

struct Row {
    name: Editor,
    value: Editor,
    env: bool,
}
pub(super) struct HeaderEditor {
    rows: Vec<Row>,
    row: usize,
    value_field: bool,
    inherit: bool,
    title: String,
    error: Option<String>,
}
pub(super) enum Outcome {
    Pending,
    Cancel,
    Save(Option<RequestHeaders>),
}
impl HeaderEditor {
    pub fn new(headers: &Option<RequestHeaders>) -> Self {
        let rows = headers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|header| {
                let mut name = Editor::default();
                name.set(&header.name);
                let mut value = Editor::default();
                let (text, env) = match &header.value {
                    HeaderValue::Literal(value) => (value, false),
                    HeaderValue::Env(name) => (name, true),
                };
                value.set(text);
                Row { name, value, env }
            })
            .collect();
        Self {
            rows,
            row: 0,
            value_field: false,
            inherit: headers.is_none(),
            error: None,
            title: String::new(),
        }
    }
    /// Names the model and role being edited in the title line.
    pub fn titled(mut self, title: String) -> Self {
        self.title = title;
        self
    }
    pub fn paste(&mut self, text: &str) {
        if text.len() > 4096 {
            self.error = Some("Header input is limited to 4096 bytes".into());
            return;
        }
        if !text.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
            self.error =
                Some("Header input must be printable ASCII without control characters".into());
            return;
        }
        if let Some(row) = self.rows.get_mut(self.row) {
            let editor = if self.value_field {
                &mut row.value
            } else {
                &mut row.name
            };
            if editor.text.len() + text.len() > 4096 {
                self.error = Some("Header input is limited to 4096 bytes".into());
                return;
            }
            editor.insert(text);
            self.error = None;
        }
    }
    pub fn key(&mut self, key: &KeyEvent, keys: &Keymap) -> Outcome {
        if keys.matches(Action::SelectCancel, key) {
            return Outcome::Cancel;
        }
        // F10 opens the menu bar in GNOME Terminal and Ctrl+Enter needs the kitty protocol, so
        // Ctrl+S saves everywhere.
        if key.code == KeyCode::F(10)
            || is_ctrl(key, 's')
            || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            if self.inherit {
                return Outcome::Save(None);
            }
            let rows: RequestHeaders = self
                .rows
                .iter()
                .map(|row| RequestHeader {
                    name: row.name.text.clone(),
                    value: if row.env {
                        HeaderValue::Env(row.value.text.clone())
                    } else {
                        HeaderValue::Literal(row.value.text.clone())
                    },
                })
                .collect();
            match validate_headers(&rows) {
                Ok(()) => return Outcome::Save(Some(rows)),
                Err(error) => self.error = Some(error.to_string()),
            }
        } else if is_ctrl(key, 'n') {
            if self.rows.len() >= 32 {
                self.error = Some("At most 32 headers".into());
            } else {
                self.rows.push(Row {
                    name: Editor::default(),
                    value: Editor::default(),
                    env: true,
                });
                self.row = self.rows.len() - 1;
                self.value_field = false;
                self.inherit = false;
            }
        } else if is_ctrl(key, 'd') {
            if self.row < self.rows.len() {
                self.rows.remove(self.row);
            }
            self.row = self.row.min(self.rows.len().saturating_sub(1));
            self.inherit = false;
        } else if key.code == KeyCode::F(9) {
            // Ctrl+I is Tab on legacy terminals, so inherit lives on a function key.
            self.rows.clear();
            self.inherit = true;
        } else if is_ctrl(key, 'e') {
            self.rows.clear();
            self.inherit = false;
        } else if key.code == KeyCode::F(2) {
            if let Some(row) = self.rows.get_mut(self.row) {
                row.env = !row.env;
            }
        } else if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.value_field = !self.value_field;
        } else if key.code == KeyCode::Up {
            self.row = self.row.saturating_sub(1);
        } else if key.code == KeyCode::Down {
            self.row = (self.row + 1).min(self.rows.len().saturating_sub(1));
        } else if let Some(row) = self.rows.get_mut(self.row) {
            let editor = if self.value_field {
                &mut row.value
            } else {
                &mut row.name
            };
            if editor.text.len() < 4096 || typed_char(key).is_none() {
                editor.key(*key);
            }
        }
        Outcome::Pending
    }
    pub fn lines(&self, theme: &Theme, width: usize, height: usize) -> Vec<Line<'static>> {
        let muted = theme.fg(Tok::Muted);
        let dim = theme.fg(Tok::Dim);
        // Short panels drop the column header and merge the hints so rows always keep room.
        let compact = height < 10;
        let mut lines: Vec<Line<'static>> = Vec::new();

        let mut title = vec![
            chrome::label(theme, " HTTP headers"),
            Span::styled(format!("  {}/{MAX_ROWS}", self.rows.len()), muted),
        ];
        if !self.title.is_empty() {
            title.push(Span::styled(format!(" · {}", self.title), muted));
        }
        lines.push(Line::from(fit(title, width)));

        let literal = self.rows.iter().any(|row| !row.env);
        let (status, tone) = if self.inherit {
            (
                " Inheriting the provider's headers. Add a row to override them.",
                Tok::Muted,
            )
        } else if self.rows.is_empty() {
            (" Saving clears every inherited header.", Tok::Warning)
        } else if literal {
            (
                " Literal values are stored unencrypted in config. Keep secrets in env rows.",
                Tok::Warning,
            )
        } else {
            (
                " Env rows take their value from the named environment variable.",
                Tok::Muted,
            )
        };
        lines.push(Line::from(fit(
            vec![Span::styled(status, theme.fg(tone))],
            width,
        )));
        if let Some(error) = &self.error {
            lines.push(Line::from(fit(
                vec![Span::styled(format!(" ! {error}"), theme.fg(Tok::Error))],
                width,
            )));
        }

        let hint =
            |text: &str| Line::from(fit(vec![Span::styled(format!(" {text}"), muted)], width));
        if compact {
            lines.push(hint(
                "^N add · ^D delete · Tab field · F2 env/literal · ^S save · Esc cancel",
            ));
        } else {
            let arrows = arrows(theme, Axis::Vertical);
            lines.push(hint(&format!(
                "^N add · ^D delete · Tab field · {arrows} row · F2 env/literal"
            )));
            lines.push(hint("F9 inherit · ^E clear all · ^S/F10 save · Esc cancel"));
        }

        let name_w = (width.saturating_sub(FIXED_COLUMNS) / 3).clamp(6, 28);
        let value_w = width.saturating_sub(FIXED_COLUMNS + name_w);
        let has_selection = !self.rows.is_empty();
        if !compact {
            let field = |active: bool| if active { theme.fg(Tok::Accent) } else { dim };
            lines.push(Line::from(fit(
                vec![
                    Span::styled(format!("   {:<KIND_W$}  ", "KIND"), dim),
                    Span::styled(
                        format!("{:<name_w$}", "NAME"),
                        field(has_selection && !self.value_field),
                    ),
                    Span::styled("   ", dim),
                    Span::styled("VALUE", field(has_selection && self.value_field)),
                ],
                width,
            )));
        }

        if self.rows.is_empty() {
            lines.push(Line::from(fit(
                vec![Span::styled(
                    " No header rows. Press Ctrl+N to add one.",
                    dim,
                )],
                width,
            )));
            lines.truncate(height.max(1));
            return lines;
        }

        let mut room = height.saturating_sub(lines.len());
        let overflow = self.rows.len() > room && room > 1;
        if overflow {
            room -= 1;
        }
        let start = if room == 0 {
            self.row
        } else {
            self.row.saturating_sub(room - 1)
        };
        for (index, row) in self.rows.iter().enumerate().skip(start).take(room) {
            let selected = index == self.row;
            let chip = if row.env {
                Span::styled(format!("{:<KIND_W$}", "env"), theme.fg(Tok::Success))
            } else {
                Span::styled(format!("{:<KIND_W$}", "literal"), theme.fg(Tok::Warning))
            };
            let pointer = if selected {
                theme.sym(Sym::Pointer).to_owned()
            } else {
                " ".to_owned()
            };
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(pointer, theme.fg(Tok::Accent)),
                Span::raw(" "),
                chip,
                Span::raw("  "),
            ];
            spans.extend(pad(
                self.cell(
                    theme,
                    &row.name,
                    name_w,
                    selected && !self.value_field,
                    "name",
                ),
                name_w,
            ));
            spans.push(Span::styled(" = ", dim));
            spans.extend(self.cell(
                theme,
                &row.value,
                value_w,
                selected && self.value_field,
                "value",
            ));
            if selected {
                lines.push(filled(spans, width, theme.bg(Tok::SelectedBg)));
            } else {
                lines.push(Line::from(fit(spans, width)));
            }
        }
        if overflow {
            lines.push(Line::from(fit(
                vec![Span::styled(
                    format!(
                        " row {} of {}  ·  {} scroll",
                        self.row + 1,
                        self.rows.len(),
                        arrows(theme, Axis::Vertical)
                    ),
                    dim,
                )],
                width,
            )));
        }
        lines.truncate(height.max(1));
        lines
    }

    /// One name/value cell: the live editor when it is the active field, otherwise static text
    /// (or a dim placeholder when empty).
    fn cell(
        &self,
        theme: &Theme,
        editor: &Editor,
        width: usize,
        active: bool,
        placeholder: &str,
    ) -> Vec<Span<'static>> {
        if active {
            return editor_spans(theme, editor, width, false);
        }
        if editor.text.is_empty() {
            return fit(
                vec![Span::styled(format!("({placeholder})"), theme.fg(Tok::Dim))],
                width,
            );
        }
        fit(
            vec![Span::styled(editor.text.clone(), theme.fg(Tok::Text))],
            width,
        )
    }
}

const MAX_ROWS: usize = 32;
const KIND_W: usize = 7;
/// Cells used by everything except the name and value columns: margin, pointer, gap, kind chip,
/// gap and the ` = ` separator.
const FIXED_COLUMNS: usize = 1 + 1 + 1 + KIND_W + 2 + 3;

fn pad(mut spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let used: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans
}

#[cfg(test)]
mod tests;
