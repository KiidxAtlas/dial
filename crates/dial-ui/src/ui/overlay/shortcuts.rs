//! Global keyboard shortcuts: browse/search the live keymap and edit textual chord tokens.
//! Persistence and validation belong to the controller; an edit stays open until `settled` succeeds.
use super::{
    DrawCtx, OverlayEvent,
    settings::{
        editor_spans, filled, hint, hint_line, pair_label, panel, pressed, scroll_cell, typed_char,
    },
};
use crate::ui::{
    chrome::Section,
    editor::Editor,
    keys::{Action, Keymap},
    text,
    theme::{Sym, Tok},
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
};

pub struct ShortcutOverlay {
    search: Editor,
    cursor: usize,
    scroll: usize,
    page: usize,
    edit: Option<(Action, Editor)>,
    message: Option<(String, bool)>,
}

/// Display strings are also valid input tokens, including `Ctrl+,`, `Space` and `Plus`.
fn chords(keys: &Keymap, action: Action) -> String {
    keys.chords(action)
        .iter()
        .map(|chord| chord.display())
        .collect::<Vec<_>>()
        .join(" ")
}

impl ShortcutOverlay {
    pub fn new() -> Self {
        Self {
            search: Editor::default(),
            cursor: 0,
            scroll: 0,
            page: 8,
            edit: None,
            message: None,
        }
    }

    fn rows(&self, keys: &Keymap) -> Vec<Action> {
        let query = self.search.text.to_lowercase();
        Action::ALL
            .iter()
            .copied()
            .filter(|&action| {
                let effective = chords(keys, action);
                let haystack = format!(
                    "{} {} {} {} {}",
                    action.describe(),
                    action.id(),
                    action.group(),
                    if effective.is_empty() {
                        "Disabled"
                    } else {
                        &effective
                    },
                    if keys.is_customized(action) {
                        "customized"
                    } else {
                        "default"
                    }
                )
                .to_lowercase();
                query.split_whitespace().all(|word| haystack.contains(word))
            })
            .collect()
    }

    pub fn settled(&mut self, result: Result<String, String>) {
        self.message = Some(match result {
            Ok(message) => {
                self.edit = None;
                (text::one_line(&message), true)
            }
            Err(message) => (text::one_line(&message), false),
        });
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if let Event::Paste(value) = event {
            self.message = None;
            let value = text::one_line(value);
            if let Some((_, editor)) = &mut self.edit {
                editor.insert(&value);
            } else {
                self.search.insert(&value);
                self.cursor = 0;
                self.scroll = 0;
            }
            return OverlayEvent::None;
        }
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if !pressed(key) {
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectCancel, key) {
            self.message = None;
            if self.edit.take().is_some() {
                return OverlayEvent::None;
            }
            if !self.search.text.is_empty() {
                self.search.set("");
                self.cursor = 0;
                self.scroll = 0;
                return OverlayEvent::None;
            }
            return OverlayEvent::Close;
        }
        if let Some((action, editor)) = &mut self.edit {
            if keys.matches(Action::SelectConfirm, key) {
                self.message = None;
                return OverlayEvent::Rebind {
                    action: *action,
                    chords: editor.text.split_whitespace().map(str::to_owned).collect(),
                };
            }
            if editor.key(*key) {
                self.message = None;
            }
            return OverlayEvent::None;
        }
        let rows = self.rows(keys);
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(rows.len().saturating_sub(1));
        } else if keys.matches(Action::SelectPageUp, key) {
            self.cursor = self.cursor.saturating_sub(self.page);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.cursor = (self.cursor + self.page).min(rows.len().saturating_sub(1));
        } else if self.search.text.is_empty() && key.code == KeyCode::Home {
            self.cursor = 0;
        } else if self.search.text.is_empty() && key.code == KeyCode::End {
            self.cursor = rows.len().saturating_sub(1);
        } else if keys.matches(Action::SelectConfirm, key) {
            if let Some(&action) = rows.get(self.cursor) {
                let mut editor = Editor::default();
                editor.set(&chords(keys, action));
                self.edit = Some((action, editor));
            }
        } else if key.code == KeyCode::Delete {
            if let Some(&action) = rows.get(self.cursor) {
                if !keys.is_customized(action) {
                    // Nothing to restore: say so instead of saving an identical keymap.
                    self.message = Some(("Already using the default keys".into(), true));
                    return OverlayEvent::None;
                }
                self.message = None;
                return OverlayEvent::Rebind {
                    action,
                    chords: action.defaults().iter().map(|s| (*s).to_owned()).collect(),
                };
            }
        } else if let Some(c) = typed_char(key) {
            self.search.insert(&c.to_string());
            self.cursor = 0;
            self.scroll = 0;
        } else if self.search.key(*key) {
            self.cursor = 0;
            self.scroll = 0;
        }
        self.message = None;
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        panel(
            frame,
            ctx.theme,
            Section::Dial,
            "Keyboard shortcuts",
            "global only",
            |w, h| self.compose(w, h, ctx),
        );
    }

    fn compose(&mut self, width: u16, height: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(width), usize::from(height));
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let theme = ctx.theme;
        let line = |value: String, tok| {
            filled(
                vec![Span::styled(value, theme.fg(tok))],
                w,
                Style::default(),
            )
        };
        let mut out = Vec::new();
        if let Some((action, editor)) = &self.edit {
            let mut spans = vec![Span::styled(" Chords: ", theme.fg(Tok::Accent))];
            spans.extend(editor_spans(theme, editor, w.saturating_sub(9), false));
            out.push(filled(spans, w, Style::default()));
            out.push(line(format!(" {}", action.describe()), Tok::Text));
            out.push(line(
                format!(" {} [{}]", action.id(), action.group()),
                Tok::Muted,
            ));
            out.push(line(
                format!(" Defaults: {}", action.defaults().join(" ")),
                Tok::Muted,
            ));
            out.push(line(
                " Space-separated chords (e.g. Ctrl+, Ctrl+4); empty disables.".into(),
                Tok::Muted,
            ));
            out.push(line(
                " Saved globally in keybindings.toml; no project override.".into(),
                Tok::Muted,
            ));
            out.push(line(
                " Conflicts are rejected; confirm/cancel controls cannot be disabled.".into(),
                Tok::Muted,
            ));
        } else {
            let mut spans = vec![Span::styled(" Search: ", theme.fg(Tok::Muted))];
            spans.extend(editor_spans(
                theme,
                &self.search,
                w.saturating_sub(9),
                false,
            ));
            if self.search.text.is_empty() {
                spans.push(Span::styled(
                    "type an action, key or group",
                    theme.fg(Tok::Dim),
                ));
            }
            out.push(filled(spans, w, Style::default()));
            let rows = self.rows(ctx.keys);
            self.cursor = self.cursor.min(rows.len().saturating_sub(1));
            // Reserve details, feedback and footer on normal terminals. Tiny terminals keep a list row.
            let list_h = h.saturating_sub(7).max(1).min(h.saturating_sub(1));
            self.page = list_h.max(1);
            if self.cursor < self.scroll {
                self.scroll = self.cursor;
            }
            if self.cursor >= self.scroll + list_h {
                self.scroll = (self.cursor + 1).saturating_sub(list_h.max(1));
            }
            self.scroll = self.scroll.min(rows.len().saturating_sub(list_h));
            for row in 0..list_h {
                let index = self.scroll + row;
                if let Some(&action) = rows.get(index) {
                    let selected = index == self.cursor;
                    let fill = if selected {
                        theme.bg(Tok::SelectedBg).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let effective = chords(ctx.keys, action);
                    let effective = if effective.is_empty() {
                        "Disabled"
                    } else {
                        &effective
                    };
                    let prefix = format!(
                        " {}{} ",
                        if selected {
                            theme.sym(Sym::Pointer)
                        } else {
                            " "
                        },
                        if ctx.keys.is_customized(action) {
                            "*"
                        } else {
                            " "
                        }
                    );
                    // On narrow terminals the selected row's details retain its group and state.
                    let group = if w >= 88 {
                        format!(" {}", text::pad(action.group(), 11))
                    } else {
                        String::new()
                    };
                    let key_w = (w / 3).min(27);
                    let description_w =
                        w.saturating_sub(text::width(&prefix) + key_w + text::width(&group) + 2);
                    let description = text::pad(
                        &text::ellipsize(action.describe(), description_w, theme.ascii()),
                        description_w,
                    );
                    let mut spans = vec![
                        Span::styled(prefix, theme.fg(Tok::Accent).patch(fill)),
                        Span::styled(description, theme.fg(Tok::Text).patch(fill)),
                        Span::styled(
                            format!(
                                " {}",
                                text::pad(&text::ellipsize(effective, key_w, theme.ascii()), key_w)
                            ),
                            theme.fg(Tok::Accent).patch(fill),
                        ),
                        Span::styled(group, theme.fg(Tok::Muted).patch(fill)),
                    ];
                    spans.push(scroll_cell(theme, row, list_h, rows.len(), self.scroll));
                    out.push(filled(spans, w, fill));
                } else if row == 0 {
                    out.push(line(
                        " No matching shortcuts. Clear the search to see all actions.".into(),
                        Tok::Muted,
                    ));
                } else {
                    out.push(Line::default());
                }
            }
            if let Some(&action) = rows.get(self.cursor) {
                let dot = theme.sym(Sym::Dot);
                out.push(line(
                    format!(
                        " {} {dot} {} {dot} {}",
                        action.group(),
                        if ctx.keys.is_customized(action) {
                            "customized"
                        } else {
                            "default keys"
                        },
                        action.id(),
                    ),
                    Tok::Muted,
                ));
                out.push(line(format!(" {}", action.describe()), Tok::Text));
                let effective = chords(ctx.keys, action);
                out.push(line(
                    format!(
                        " Effective: {}",
                        if effective.is_empty() {
                            "Disabled"
                        } else {
                            &effective
                        }
                    ),
                    Tok::Accent,
                ));
                out.push(line(
                    format!(" Defaults: {}", action.defaults().join(" ")),
                    Tok::Muted,
                ));
            }
        }
        if let Some((message, ok)) = &self.message {
            out.push(line(
                format!(" {message}"),
                if *ok { Tok::Success } else { Tok::Error },
            ));
        }
        let editing = self.edit.is_some();
        let hints = if editing {
            vec![
                hint(ctx.keys, Action::SelectConfirm, "save globally"),
                "empty disables".into(),
                hint(ctx.keys, Action::SelectCancel, "cancel"),
            ]
        } else {
            let movement = pair_label(ctx.keys, Action::SelectUp, Action::SelectDown);
            let rows = self.rows(ctx.keys);
            let selected = rows.get(self.cursor).copied();
            vec![
                if selected.is_some() {
                    hint(ctx.keys, Action::SelectConfirm, "edit")
                } else {
                    String::new()
                },
                if movement.is_empty() || selected.is_none() {
                    String::new()
                } else {
                    format!("{movement} move")
                },
                "type to search".into(),
                // Restoring is only offered where it would change something.
                if selected.is_some_and(|action| ctx.keys.is_customized(action)) {
                    "Delete restore defaults".into()
                } else {
                    String::new()
                },
                hint(
                    ctx.keys,
                    Action::SelectCancel,
                    if self.search.text.is_empty() {
                        "close"
                    } else {
                        "clear search"
                    },
                ),
            ]
        };
        // Keep the footer visible; show save errors ahead of less important detail on short screens.
        if out.len() >= h {
            out.truncate(h.saturating_sub(1));
            if h > 1
                && (!editing || h > 2)
                && let Some((message, ok)) = &self.message
            {
                out[h - 2] = line(
                    format!(" {message}"),
                    if *ok { Tok::Success } else { Tok::Error },
                );
            }
        }
        while out.len() < h.saturating_sub(1) {
            out.push(Line::default());
        }
        out.push(filled(
            hint_line(theme, &hints, w).spans,
            w,
            Style::default(),
        ));
        out
    }
}

impl Default for ShortcutOverlay {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
