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
        } else if keys.matches(Action::SelectConfirm, key) {
            if let Some(&action) = rows.get(self.cursor) {
                let mut editor = Editor::default();
                editor.set(&chords(keys, action));
                self.edit = Some((action, editor));
            }
        } else if key.code == KeyCode::Delete {
            if let Some(&action) = rows.get(self.cursor) {
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
            let mut spans = vec![Span::styled(" Search: ", theme.fg(Tok::Accent))];
            spans.extend(editor_spans(
                theme,
                &self.search,
                w.saturating_sub(9),
                false,
            ));
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
                    let description =
                        text::pad(&text::clip(action.describe(), description_w), description_w);
                    let mut spans = vec![
                        Span::styled(prefix, theme.fg(Tok::Accent).patch(fill)),
                        Span::styled(description, theme.fg(Tok::Text).patch(fill)),
                        Span::styled(
                            format!(" {}", text::pad(&text::clip(effective, key_w), key_w)),
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
                out.push(line(
                    format!(
                        " {} [{}; {}]",
                        action.id(),
                        action.group(),
                        if ctx.keys.is_customized(action) {
                            "customized"
                        } else {
                            "default"
                        }
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
                hint(ctx.keys, Action::SelectCancel, "cancel edit"),
            ]
        } else {
            let movement = pair_label(ctx.keys, Action::SelectUp, Action::SelectDown);
            vec![
                hint(ctx.keys, Action::SelectConfirm, "edit"),
                if movement.is_empty() {
                    String::new()
                } else {
                    format!("{movement} move")
                },
                "type to search".into(),
                "Delete restore defaults".into(),
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
mod tests {
    use super::super::settings::testkit::*;
    use super::*;
    use crate::ui::theme::{Glyphs, Theme};
    use crossterm::event::{KeyEvent, KeyEventKind, KeyModifiers};

    fn select(overlay: &mut ShortcutOverlay, action: Action) {
        overlay.search.set(action.id());
        overlay.cursor = 0;
    }

    fn draw(overlay: &mut ShortcutOverlay, rig: &Rig, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        render(w, h, |frame| overlay.draw(frame, &ctx))
    }

    #[test]
    fn searches_description_id_group_and_effective_key_including_s() {
        let keys = Keymap::defaults();
        let mut overlay = ShortcutOverlay::new();
        assert_eq!(overlay.rows(&keys), Action::ALL);
        overlay.handle(&ch('s'), &keys);
        assert_eq!(overlay.search.text, "s");
        assert!(overlay.rows(&keys).contains(&Action::Settings));
        for (query, action) in [
            ("prompt history", Action::HistorySearch),
            ("APP.SETTINGS", Action::Settings),
            ("Navigation", Action::SelectDown),
            ("Ctrl+,", Action::Settings),
        ] {
            overlay.search.set(query);
            assert!(overlay.rows(&keys).contains(&action), "{query}");
        }
        overlay.search.set("no-such-action");
        assert!(overlay.rows(&keys).is_empty());
        assert!(matches!(
            overlay.handle(&key(KeyCode::Enter), &keys),
            OverlayEvent::None
        ));
        assert!(matches!(
            overlay.handle(&key(KeyCode::Delete), &keys),
            OverlayEvent::None
        ));
        assert!(overlay.edit.is_none());
    }

    #[test]
    fn edit_paste_save_and_settle_preserve_comma_chords() {
        let keys = Keymap::defaults();
        let mut overlay = ShortcutOverlay::new();
        select(&mut overlay, Action::Settings);
        overlay.handle(&key(KeyCode::Enter), &keys);
        assert_eq!(overlay.edit.as_ref().unwrap().1.text, "Ctrl+, Ctrl+4");
        overlay.handle(&ctrl('u'), &keys);
        overlay.handle(&Event::Paste("Ctrl+,\nAlt+界\x1b[31m".into()), &keys);
        assert_eq!(overlay.edit.as_ref().unwrap().1.text, "Ctrl+, Alt+界");
        let OverlayEvent::Rebind { action, chords } = overlay.handle(&key(KeyCode::Enter), &keys)
        else {
            panic!("expected rebind")
        };
        assert_eq!(action, Action::Settings);
        assert_eq!(chords, ["Ctrl+,", "Alt+界"]);
        assert!(overlay.edit.is_some());
        overlay.settled(Err("Cannot save: read-only".into()));
        assert_eq!(overlay.edit.as_ref().unwrap().1.text, "Ctrl+, Alt+界");
        assert!(!overlay.message.as_ref().unwrap().1);
        let rig = Rig::new();
        assert!(screen(&draw(&mut overlay, &rig, 90, 16)).contains("Cannot save: read-only"));
        overlay.settled(Ok("Saved global shortcuts".into()));
        assert!(overlay.edit.is_none());
        assert_eq!(
            overlay.message,
            Some(("Saved global shortcuts".into(), true))
        );
        assert_eq!(keys.chords(Action::Settings).len(), 2);
    }

    #[test]
    fn layered_cancel_disable_and_restore_defaults() {
        let mut keys = Keymap::defaults();
        keys.rebind(Action::Settings, &["F8"]).unwrap();
        let mut overlay = ShortcutOverlay::new();
        select(&mut overlay, Action::Settings);
        overlay.handle(&key(KeyCode::Enter), &keys);
        assert_eq!(overlay.edit.as_ref().unwrap().1.text, "F8");
        overlay.handle(&ctrl('u'), &keys);
        assert!(
            matches!(overlay.handle(&key(KeyCode::Enter), &keys), OverlayEvent::Rebind { chords, .. } if chords.is_empty())
        );
        assert!(matches!(
            overlay.handle(&key(KeyCode::Esc), &keys),
            OverlayEvent::None
        ));
        assert!(overlay.edit.is_none());
        let OverlayEvent::Rebind { action, chords } = overlay.handle(&key(KeyCode::Delete), &keys)
        else {
            panic!("expected defaults")
        };
        assert_eq!(action, Action::Settings);
        assert_eq!(chords, Action::Settings.defaults());
        assert!(matches!(
            overlay.handle(&key(KeyCode::Esc), &keys),
            OverlayEvent::None
        ));
        assert!(overlay.search.text.is_empty());
        assert!(matches!(
            overlay.handle(&key(KeyCode::Esc), &keys),
            OverlayEvent::Close
        ));
    }

    #[test]
    fn remapped_navigation_confirmation_cancel_and_release() {
        let mut keys = Keymap::defaults();
        for (action, chord) in [
            (Action::SelectDown, "F2"),
            (Action::SelectUp, "F3"),
            (Action::SelectConfirm, "F4"),
            (Action::SelectCancel, "F5"),
            (Action::SelectPageDown, "F6"),
            (Action::SelectPageUp, "F7"),
        ] {
            keys.rebind(action, &[chord]).unwrap();
        }
        let mut overlay = ShortcutOverlay::new();
        overlay.handle(&key(KeyCode::Down), &keys);
        assert_eq!(overlay.cursor, 0);
        overlay.handle(&key(KeyCode::F(2)), &keys);
        assert_eq!(overlay.cursor, 1);
        overlay.handle(&key(KeyCode::F(3)), &keys);
        assert_eq!(overlay.cursor, 0);
        overlay.handle(&key(KeyCode::F(6)), &keys);
        assert_eq!(overlay.cursor, overlay.page);
        overlay.handle(&key(KeyCode::F(7)), &keys);
        assert_eq!(overlay.cursor, 0);
        let release = Event::Key(KeyEvent::new_with_kind(
            KeyCode::F(2),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        overlay.handle(&release, &keys);
        assert_eq!(overlay.cursor, 0);
        overlay.handle(&key(KeyCode::Enter), &keys);
        assert!(overlay.edit.is_none());
        overlay.handle(&key(KeyCode::F(4)), &keys);
        assert!(overlay.edit.is_some());
        assert!(matches!(
            overlay.handle(&key(KeyCode::F(4)), &keys),
            OverlayEvent::Rebind { .. }
        ));
        overlay.handle(&key(KeyCode::F(5)), &keys);
        assert!(overlay.edit.is_none());
        assert!(matches!(
            overlay.handle(&key(KeyCode::F(5)), &keys),
            OverlayEvent::Close
        ));
    }

    #[test]
    fn short_edit_screens_keep_the_field_and_footer_even_after_failure() {
        let rig = Rig::new();
        let mut overlay = ShortcutOverlay::new();
        select(&mut overlay, Action::Settings);
        overlay.handle(&key(KeyCode::Enter), &rig.keys);
        for failed in [false, true] {
            if failed {
                overlay.settled(Err("Cannot save".into()));
            }
            for height in [5, 6, 8] {
                let shown = screen(&draw(&mut overlay, &rig, 80, height));
                assert!(
                    shown.contains("Chords: Ctrl+, Ctrl+4"),
                    "height {height}: {shown}"
                );
                assert!(
                    shown.contains("Enter save globally"),
                    "height {height}: {shown}"
                );
                assert!(
                    shown.contains("Esc cancel edit"),
                    "height {height}: {shown}"
                );
                if failed && height > 5 {
                    assert!(shown.contains("Cannot save"));
                }
            }
        }
    }

    #[test]
    fn disabled_movement_has_no_empty_key_hint() {
        let mut rig = Rig::new();
        rig.keys.rebind(Action::SelectUp, &[]).unwrap();
        rig.keys.rebind(Action::SelectDown, &[]).unwrap();
        let shown = screen(&draw(&mut ShortcutOverlay::new(), &rig, 180, 16));
        assert!(!shown.contains(" move"));
        assert!(shown.contains("Enter edit"));
    }

    #[test]
    fn theme_ascii_selection_scroll_disabled_and_unicode_bounds() {
        let theme = Theme::dark().with_glyphs(Glyphs::Ascii);
        let mut rig = Rig::with_theme(theme);
        rig.keys.rebind(Action::Submit, &[]).unwrap();
        let mut overlay = ShortcutOverlay::new();
        let buffer = draw(&mut overlay, &rig, 100, 16);
        let shown = screen(&buffer);
        assert!(shown.is_ascii());
        for label in [
            "global only",
            "Disabled",
            "customized",
            "Defaults: Enter",
            "Delete restore defaults",
        ] {
            assert!(shown.contains(label), "{label}: {shown}");
        }
        assert_eq!(
            style_of(&buffer, "Send the message").unwrap().bg,
            rig.theme.bg(Tok::SelectedBg).bg
        );
        for _ in 0..Action::ALL.len() {
            overlay.handle(&key(KeyCode::Down), &rig.keys);
        }
        draw(&mut overlay, &rig, 70, 14);
        assert!(overlay.scroll > 0);
        assert!(overlay.cursor >= overlay.scroll && overlay.cursor < overlay.scroll + overlay.page);
        overlay.search.set("界e\u{301}👩‍💻");
        assert!(screen(&draw(&mut overlay, &rig, 100, 16)).contains("No matching shortcuts"));
        select(&mut overlay, Action::Settings);
        overlay.handle(&key(KeyCode::Enter), &rig.keys);
        overlay.handle(&Event::Paste("界e\u{301}👩‍💻".repeat(60)), &rig.keys);
        for editing in [true, false] {
            if !editing {
                overlay.edit = None;
            }
            for (w, h) in [(0, 0), (1, 1), (2, 2), (3, 3), (8, 5), (30, 10), (240, 40)] {
                let lines = overlay.compose(
                    w,
                    h,
                    &DrawCtx {
                        theme: &rig.theme,
                        keys: &rig.keys,
                        status: &rig.status,
                        preview_status: &status_preview,
                    },
                );
                assert!(lines.len() <= usize::from(h));
                assert!(
                    lines
                        .iter()
                        .all(|line| text::line_width(line) <= usize::from(w))
                );
                draw(&mut overlay, &rig, w, h);
            }
        }
    }
}
