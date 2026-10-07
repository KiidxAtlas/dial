//! Searchable application actions. The controller supplies the real command registry and dispatch
//! policy; this panel never touches the chat draft, starts compute, or executes commands itself.
use super::{
    DrawCtx, OverlayEvent,
    settings::{editor_spans, filled, hint, hint_line, panel, pressed, title_line, typed_char},
};
use crate::ui::{
    composer::{CommandEntry, CommandKind},
    editor::Editor,
    keys::{Action, Keymap},
    text,
    theme::{Sym, Tok},
};
use crossterm::event::Event;
use ratatui::{
    Frame,
    style::Style,
    text::{Line, Span},
};

const GROUPS: &[&str] = &[
    "All",
    "Sessions",
    "Models",
    "Tools",
    "Review",
    "Settings",
    "Extensions",
];

pub struct PaletteEntry {
    pub command: CommandEntry,
    pub during_turn: &'static str,
}

fn group(command: &CommandEntry) -> &'static str {
    if command.kind != CommandKind::Builtin {
        return "Extensions";
    }
    match command.name.as_str() {
        "new" | "sessions" | "resume" | "fork" | "rename" | "export" | "tree" | "search"
        | "copy" => "Sessions",
        "model" | "providers" | "profile" | "reasoning" | "thinking" | "context" | "cost" => {
            "Models"
        }
        "compute" | "jobs" | "message" | "cancel" | "skills" | "commands" | "memory"
        | "understanding" | "laya" => "Tools",
        "undo" | "rollback" | "plan" | "todo" | "reliability" | "policy" | "experience"
        | "outcome" | "compact" => "Review",
        _ => "Settings",
    }
}

fn shortcut(name: &str) -> Option<Action> {
    match name {
        "model" => Some(Action::ModelSelect),
        "providers" => Some(Action::ProviderSetup),
        "settings" => Some(Action::Settings),
        "palette" => Some(Action::CommandPalette),
        _ => None,
    }
}

pub struct PaletteOverlay {
    entries: Vec<PaletteEntry>,
    search: Editor,
    group: usize,
    cursor: usize,
    scroll: usize,
    page: usize,
    arguments: Option<(usize, Editor)>,
    busy: bool,
}

impl PaletteOverlay {
    pub fn new(entries: Vec<PaletteEntry>, busy: bool) -> Self {
        let mut seen = std::collections::HashSet::new();
        let entries = entries
            .into_iter()
            .filter_map(|mut entry| {
                let name = &entry.command.name;
                if name == "palette"
                    || name.is_empty()
                    || name
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control() || c == '/')
                    || !seen.insert(name.clone())
                {
                    return None;
                }
                entry.command.description = text::one_line(&entry.command.description);
                entry.command.args_hint = entry.command.args_hint.map(|s| text::one_line(&s));
                Some(entry)
            })
            .collect();
        Self {
            entries,
            search: Editor::default(),
            group: 0,
            cursor: 0,
            scroll: 0,
            page: 8,
            arguments: None,
            busy,
        }
    }

    fn rows(&self) -> Vec<usize> {
        let query = self.search.text.trim().trim_start_matches('/');
        let mut rows: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if self.group != 0 && group(&entry.command) != GROUPS[self.group] {
                    return None;
                }
                let name = text::fuzzy(query, &entry.command.name);
                let other = text::fuzzy(
                    query,
                    &format!("{} {}", entry.command.description, group(&entry.command)),
                );
                let score = match (name, other) {
                    (Some((score, _)), _) => {
                        (entry.command.name.eq_ignore_ascii_case(query), true, score)
                    }
                    (None, Some((score, _))) => (false, false, score),
                    _ => return None,
                };
                Some((index, score))
            })
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        rows.into_iter().map(|(index, _)| index).collect()
    }

    fn reset_selection(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if let Event::Paste(value) = event {
            let value = text::one_line(value);
            if let Some((_, editor)) = &mut self.arguments {
                editor.insert(&value);
            } else {
                self.search.insert(&value);
                self.reset_selection();
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
            if self.arguments.take().is_some() {
                return OverlayEvent::None;
            }
            if !self.search.text.is_empty() {
                self.search.set("");
                self.reset_selection();
                return OverlayEvent::None;
            }
            return OverlayEvent::Close;
        }
        if let Some((index, editor)) = &mut self.arguments {
            if keys.matches(Action::SelectConfirm, key) {
                let command = &self.entries[*index].command;
                let args = editor.text.trim();
                if args.is_empty()
                    && command
                        .args_hint
                        .as_deref()
                        .is_some_and(|hint| hint.starts_with('<'))
                {
                    return OverlayEvent::None;
                }
                return OverlayEvent::Action(format!(
                    "command:/{}{}{}",
                    command.name,
                    if args.is_empty() { "" } else { " " },
                    args
                ));
            }
            editor.key(*key);
            return OverlayEvent::None;
        }
        if keys.matches(Action::SectionNext, key) || keys.matches(Action::SectionPrev, key) {
            self.group = (self.group
                + if keys.matches(Action::SectionNext, key) {
                    1
                } else {
                    GROUPS.len() - 1
                })
                % GROUPS.len();
            self.reset_selection();
            return OverlayEvent::None;
        }
        let rows = self.rows();
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
            if let Some(&index) = rows.get(self.cursor) {
                if self.entries[index]
                    .command
                    .args_hint
                    .as_deref()
                    .is_some_and(|hint| hint.starts_with('<'))
                {
                    self.arguments = Some((index, Editor::default()));
                } else {
                    return OverlayEvent::Action(format!(
                        "command:/{}",
                        self.entries[index].command.name
                    ));
                }
            }
        } else if keys.matches(Action::Newline, key) {
            if let Some(&index) = rows.get(self.cursor) {
                self.arguments = Some((index, Editor::default()));
            }
        } else if let Some(c) = typed_char(key) {
            self.search.insert(&c.to_string());
            self.reset_selection();
        } else if self.search.key(*key) {
            self.reset_selection();
        }
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let title = title_line(
            ctx.theme,
            Sym::IconCommand,
            "Actions",
            if self.busy { "turn running" } else { "ready" },
        );
        panel(frame, ctx.theme, title, |w, h| self.compose(w, h, ctx));
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
        if let Some((index, editor)) = &self.arguments {
            let command = &self.entries[*index].command;
            out.push(line(
                format!(
                    " /{} {}",
                    command.name,
                    command.args_hint.as_deref().unwrap_or("")
                ),
                Tok::Accent,
            ));
            let mut spans = vec![Span::styled(" Arguments: ", theme.fg(Tok::Accent))];
            spans.extend(editor_spans(theme, editor, w.saturating_sub(12), false));
            out.push(filled(spans, w, Style::default()));
            out.push(line(format!(" {}", command.description), Tok::Text));
            if self.busy {
                out.push(line(
                    " Argument forms follow the same rules as slash commands.".into(),
                    Tok::Muted,
                ));
            }
            out.push(line(
                " Your chat draft is preserved. Paste edits arguments; Enter runs.".into(),
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
            // Keep the active category visible even when all category names do not fit.
            let mut first = 0;
            while first < self.group
                && GROUPS[first..=self.group]
                    .iter()
                    .map(|name| text::width(name) + 2)
                    .sum::<usize>()
                    > w
            {
                first += 1;
            }
            let tabs = GROUPS
                .iter()
                .enumerate()
                .skip(first)
                .map(|(i, name)| {
                    Span::styled(
                        format!(" {name} "),
                        if i == self.group {
                            theme.fg(Tok::Accent).patch(theme.bg(Tok::SelectedBg))
                        } else {
                            theme.fg(Tok::Muted)
                        },
                    )
                })
                .collect();
            out.push(filled(tabs, w, Style::default()));
            let rows = self.rows();
            self.cursor = self.cursor.min(rows.len().saturating_sub(1));
            out.push(line(
                format!(
                    " {} actions · {}/{}",
                    GROUPS[self.group],
                    if rows.is_empty() { 0 } else { self.cursor + 1 },
                    rows.len()
                ),
                Tok::Muted,
            ));
            let list_h = h.saturating_sub(8).max(1).min(h.saturating_sub(3));
            self.page = list_h.max(1);
            if self.cursor < self.scroll {
                self.scroll = self.cursor;
            }
            if self.cursor >= self.scroll + list_h {
                self.scroll = (self.cursor + 1).saturating_sub(list_h.max(1));
            }
            self.scroll = self.scroll.min(rows.len().saturating_sub(list_h));
            for row in 0..list_h {
                if let Some(&index) = rows.get(self.scroll + row) {
                    let entry = &self.entries[index];
                    let selected = self.scroll + row == self.cursor;
                    let fill = if selected {
                        theme.bg(Tok::SelectedBg)
                    } else {
                        Style::default()
                    };
                    let name_w = (w / 3).clamp(1, 24);
                    let right = if w >= 64 {
                        shortcut(&entry.command.name)
                            .map(|a| ctx.keys.labels(a))
                            .unwrap_or_else(|| group(&entry.command).into())
                    } else {
                        String::new()
                    };
                    let right_w = text::width(&right);
                    let description_w = w.saturating_sub(name_w + right_w + 6);
                    out.push(filled(
                        vec![
                            Span::styled(
                                format!(
                                    " {} ",
                                    if selected {
                                        theme.sym(Sym::Pointer)
                                    } else {
                                        " "
                                    }
                                ),
                                theme.fg(Tok::Accent).patch(fill),
                            ),
                            Span::styled(
                                text::pad(
                                    &text::clip(&format!("/{}", entry.command.name), name_w),
                                    name_w,
                                ),
                                theme.fg(Tok::Text).patch(fill),
                            ),
                            Span::styled(
                                format!(
                                    " {} ",
                                    text::pad(
                                        &text::clip(&entry.command.description, description_w),
                                        description_w
                                    )
                                ),
                                theme.fg(Tok::Muted).patch(fill),
                            ),
                            Span::styled(right, theme.fg(Tok::Muted).patch(fill)),
                        ],
                        w,
                        fill,
                    ));
                } else if row == 0 {
                    out.push(line(
                        " No matching actions. Clear search or choose All.".into(),
                        Tok::Muted,
                    ));
                } else {
                    out.push(Line::default());
                }
            }
            if let Some(&index) = rows.get(self.cursor) {
                let entry = &self.entries[index];
                out.push(line(
                    format!(
                        " /{} {}",
                        entry.command.name,
                        entry.command.args_hint.as_deref().unwrap_or("")
                    ),
                    Tok::Accent,
                ));
                out.extend(
                    text::wrap_words(&entry.command.description, w.saturating_sub(2).max(1))
                        .into_iter()
                        .take(2)
                        .map(|value| line(format!(" {value}"), Tok::Text)),
                );
                out.push(line(
                    if self.busy {
                        format!(" {}", entry.during_turn)
                    } else {
                        " Your chat draft is preserved.".into()
                    },
                    Tok::Muted,
                ));
            }
        }
        while out.len() + 1 < h {
            out.push(Line::default());
        }
        out.push(hint_line(
            theme,
            &[
                hint(ctx.keys, Action::SelectConfirm, "run"),
                hint(ctx.keys, Action::Newline, "arguments"),
                hint(ctx.keys, Action::SectionNext, "category"),
                hint(ctx.keys, Action::SelectCancel, "back / close"),
            ],
            w,
        ));
        out.truncate(h);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::settings::testkit::*;
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn entries() -> Vec<PaletteEntry> {
        [
            ("compute", "Stop unused processes and inspect memory", None),
            ("model", "Select a model", None),
            ("resume", "Resume a conversation", Some("[id]")),
            ("rename", "Name this conversation", Some("<title>")),
            ("undo", "Restore changes from the latest turn", None),
            ("settings", "Change preferences", None),
        ]
        .into_iter()
        .map(|(name, description, args)| PaletteEntry {
            command: CommandEntry {
                name: name.into(),
                description: description.into(),
                kind: CommandKind::Builtin,
                args_hint: args.map(str::to_owned),
            },
            during_turn: "Stops the current turn before running.",
        })
        .collect()
    }
    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }
    fn plain(code: KeyCode) -> Event {
        key(code, KeyModifiers::NONE)
    }

    #[test]
    fn search_categories_and_default_actions_use_the_selected_command() {
        let keys = Keymap::defaults();
        let mut palette = PaletteOverlay::new(entries(), true);
        palette.handle(&Event::Paste("MODEL".into()), &keys);
        assert_eq!(palette.entries[palette.rows()[0]].command.name, "model");
        assert!(
            matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/model")
        );
        palette.handle(&plain(KeyCode::Esc), &keys);
        palette.handle(&plain(KeyCode::Tab), &keys);
        let rows: Vec<_> = palette
            .rows()
            .iter()
            .map(|&i| palette.entries[i].command.name.as_str())
            .collect();
        assert_eq!(rows, ["resume", "rename"]);
        assert!(
            matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/resume")
        );
    }

    #[test]
    fn arguments_require_explicit_confirmation_and_escape_keeps_search() {
        let keys = Keymap::defaults();
        let mut palette = PaletteOverlay::new(entries(), false);
        palette.handle(&Event::Paste("rename".into()), &keys);
        assert!(matches!(
            palette.handle(&plain(KeyCode::Enter), &keys),
            OverlayEvent::None
        ));
        assert!(palette.arguments.is_some());
        assert!(matches!(
            palette.handle(&plain(KeyCode::Enter), &keys),
            OverlayEvent::None
        ));
        assert!(matches!(
            palette.handle(&Event::Paste("日本語\nproject".into()), &keys),
            OverlayEvent::None
        ));
        assert!(
            matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/rename 日本語 project")
        );
        palette.handle(&plain(KeyCode::Esc), &keys);
        assert_eq!(palette.search.text, "rename");
        assert!(palette.arguments.is_none());
        palette.handle(&plain(KeyCode::Esc), &keys);
        assert!(matches!(
            palette.handle(&plain(KeyCode::Esc), &keys),
            OverlayEvent::Close
        ));
    }

    #[test]
    fn optional_arguments_are_available_without_replacing_the_default_action() {
        let keys = Keymap::defaults();
        let mut palette = PaletteOverlay::new(entries(), false);
        palette.handle(&Event::Paste("resume".into()), &keys);
        palette.handle(&key(KeyCode::Enter, KeyModifiers::ALT), &keys);
        assert!(palette.arguments.is_some());
        palette.handle(&Event::Paste("abc123".into()), &keys);
        assert!(
            matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/resume abc123")
        );
    }

    #[test]
    fn unmatched_and_untrusted_entries_cannot_dispatch() {
        let keys = Keymap::defaults();
        let mut source = entries();
        source.push(PaletteEntry {
            command: CommandEntry {
                name: "bad\x1bname".into(),
                description: "bad".into(),
                kind: CommandKind::Custom,
                args_hint: None,
            },
            during_turn: "later",
        });
        let mut palette = PaletteOverlay::new(source, false);
        assert_eq!(palette.entries.len(), entries().len());
        palette.handle(&Event::Paste("unmatchable-command".into()), &keys);
        assert!(palette.rows().is_empty());
        assert!(matches!(
            palette.handle(&plain(KeyCode::Enter), &keys),
            OverlayEvent::None
        ));
    }

    #[test]
    fn layout_preserves_details_controls_and_fits_small_terminals() {
        let rig = Rig::new();
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let mut palette = PaletteOverlay::new(entries(), true);
        for (w, h) in [(0, 0), (1, 1), (2, 2), (8, 4), (30, 10), (100, 24)] {
            for mode in [false, true] {
                palette.arguments = mode.then(|| (3, Editor::default()));
                let rows = palette.compose(w, h, &ctx);
                assert!(rows.len() <= usize::from(h));
                assert!(
                    rows.iter()
                        .all(|row| text::line_width(row) <= usize::from(w))
                );
                render(w, h, |frame| palette.draw(frame, &ctx));
            }
        }
        palette.arguments = None;
        let lines = palette.compose(100, 24, &ctx);
        let lines: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(lines.iter().any(|l| l.contains("Stops the current turn")));
        assert!(lines.last().unwrap().contains("back / close"));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("All") && l.contains("Extensions"))
        );
    }
}
