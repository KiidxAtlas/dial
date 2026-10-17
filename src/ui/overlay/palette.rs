//! Searchable application actions. The controller supplies the real command registry and dispatch
//! policy; this panel never touches the chat draft, starts compute, or executes commands itself.
//! The same panel serves as a picker ([`ChoiceSpec`]) for commands that take one value.
use super::{
    DrawCtx, OverlayEvent,
    settings::{editor_spans, filled, hint_line, panel, pressed, tab_style, typed_char},
};
use crate::ui::{
    chrome::Section,
    composer::{CommandEntry, CommandKind},
    editor::Editor,
    keys::{Action, Keymap},
    text,
    theme::{Sym, Theme, Tok},
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
};

/// Tabs: everything, one per command category, then custom commands and skills.
const GROUPS: &[&str] = &[
    "All",
    "Sessions",
    "Models",
    "Changes",
    "Tasks",
    "Modes",
    "Context",
    "Tools",
    "Settings",
    "Upkeep",
    "Learn",
    "Extensions",
];

/// Getting-started actions lead the unfiltered list; defined with the other starter lists.
const STARTERS: &[&str] = crate::ui::command_surface::STARTERS;

pub struct PaletteEntry {
    pub command: CommandEntry,
    pub during_turn: &'static str,
}

/// One value a picker can apply. `value` is what the command receives; `label` and `detail` are shown.
pub struct ChoiceItem {
    pub value: String,
    pub label: String,
    pub detail: String,
    pub current: bool,
}

/// A picker over one command's values. Enter runs `/command [verb] value`, where an empty `verb`
/// applies the value directly. When `remove` is set, Delete, Backspace on an empty search, or `x`
/// on an empty search runs `/command <remove> value` for the selected item.
pub struct ChoiceSpec {
    pub title: String,
    pub command: String,
    pub verb: &'static str,
    pub remove: Option<&'static str>,
    pub items: Vec<ChoiceItem>,
}

fn group(command: &CommandEntry) -> &'static str {
    if command.kind != CommandKind::Builtin {
        return "Extensions";
    }
    crate::ui::command_surface::category(&command.name).tab()
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

/// Describe the task first; the exact slash command remains visible in the detail pane.
/// Custom commands and skills keep their own names rather than borrowing a built-in label.
fn action_label(command: &CommandEntry) -> &str {
    if command.kind != CommandKind::Builtin {
        return &command.name;
    }
    match command.name.as_str() {
        "model" => "Choose a model",
        "providers" => "Connect a provider",
        "settings" => "Customize Dial",
        "setup" => "Set up optional tools",
        "sessions" | "resume" => "Continue a conversation",
        "new" => "Start a conversation",
        "rename" => "Name this conversation",
        "fork" => "Branch this conversation",
        "plan" => "Plan before making changes",
        "review" => "Review code changes",
        "changes" => "Inspect changed files",
        "undo" => "Undo the latest turn",
        "permissions" => "Manage tool permissions",
        "approval" => "Choose when Dial asks",
        "effort" => "Choose thinking effort",
        "workers" => "Manage background agents",
        "processes" => "Manage running processes",
        "queue" => "Edit queued messages",
        "skills" => "Manage skills",
        "memory" => "Inspect project memory",
        "compact" => "Free conversation context",
        "cost" => "Check token usage and cost",
        "theme" => "Change appearance",
        "extensions" => "Browse extensions",
        "help" => "Explore available actions",
        "exit" => "Exit Dial",
        _ => &command.name,
    }
}

/// A key hint that lists every chord for the action (Shift+Enter and Ctrl+J both work for newline).
fn keys_hint(keys: &Keymap, action: Action, what: &str) -> String {
    let label = keys.labels(action);
    if label.is_empty() {
        String::new()
    } else {
        format!("{label} {what}")
    }
}

/// Space-separated non-empty words, for showing the exact command a row will run.
fn words(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The action that runs `/command verb value` for a picker row.
fn choice_action(spec: &ChoiceSpec, value: &str, verb: &str) -> OverlayEvent {
    OverlayEvent::Action(format!("command:/{}", words(&[&spec.command, verb, value])))
}

pub struct PaletteOverlay {
    entries: Vec<PaletteEntry>,
    search: Editor,
    group: usize,
    cursor: usize,
    scroll: usize,
    page: usize,
    arguments: Option<(usize, Editor)>,
    choice: Option<ChoiceSpec>,
    /// Why the last Enter did nothing, shown until the next key.
    notice: Option<String>,
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
            choice: None,
            notice: None,
            busy,
        }
    }

    /// A picker over `spec.items`, reusing the palette's search, cursor, panel and footer.
    pub fn choice(spec: ChoiceSpec) -> Self {
        let mut palette = Self::new(Vec::new(), false);
        palette.entries = spec
            .items
            .iter()
            .map(|item| PaletteEntry {
                command: CommandEntry {
                    name: item.label.clone(),
                    description: text::one_line(&item.detail),
                    kind: CommandKind::Builtin,
                    args_hint: None,
                },
                during_turn: "",
            })
            .collect();
        palette.choice = Some(spec);
        palette
    }

    /// Keeps the "turn running" wording in step with the shell while the palette stays open.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    fn rows(&self) -> Vec<usize> {
        let query = self.search.text.trim().trim_start_matches('/');
        let picker = self.choice.is_some();
        let mut rows: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if !picker
                    && self.group == 0
                    && query.is_empty()
                    && entry.command.kind == CommandKind::Builtin
                    && !crate::ui::command_surface::everyday(&entry.command.name)
                {
                    return None;
                }
                if !picker && self.group != 0 && group(&entry.command) != GROUPS[self.group] {
                    return None;
                }
                let name = text::fuzzy(query, &entry.command.name);
                let other = text::fuzzy(
                    query,
                    &format!(
                        "{} {} {}",
                        action_label(&entry.command),
                        entry.command.description,
                        group(&entry.command)
                    ),
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
        if query.is_empty() && !picker {
            rows.sort_by_key(|(index, _)| {
                let name = self.entries[*index].command.name.as_str();
                STARTERS
                    .iter()
                    .position(|starter| *starter == name)
                    .unwrap_or(STARTERS.len())
            });
        }
        rows.into_iter().map(|(index, _)| index).collect()
    }

    fn reset_selection(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    /// What Esc does in the current mode, so the footer names it correctly.
    fn cancel_label(&self) -> &'static str {
        if self.arguments.is_some() {
            "back"
        } else if !self.search.text.is_empty() {
            "clear search"
        } else {
            "close"
        }
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if let Event::Paste(value) = event {
            let value = text::one_line(value);
            self.notice = None;
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
        self.notice = None;
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
                    && let Some(hint) = command.args_hint.as_deref()
                    && hint.starts_with('<')
                {
                    let required = hint.split_whitespace().next().unwrap_or(hint).to_owned();
                    self.notice = Some(format!("Enter a value for {required}"));
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
        if let Some(spec) = &self.choice
            && let Some(verb) = spec.remove
        {
            let clear = self.search.text.is_empty();
            let removes = matches!(key.code, KeyCode::Delete)
                || (clear && matches!(key.code, KeyCode::Backspace | KeyCode::Char('x')));
            if removes {
                return match self.rows().get(self.cursor) {
                    Some(&index) => choice_action(spec, &spec.items[index].value, verb),
                    None => OverlayEvent::None,
                };
            }
        }
        if self.choice.is_none()
            && (keys.matches(Action::SectionNext, key) || keys.matches(Action::SectionPrev, key))
        {
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
                return self.activate(index);
            }
        } else if keys.matches(Action::Newline, key) && self.choice.is_none() {
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

    /// Run the entry (or ask for its required arguments first).
    fn activate(&mut self, index: usize) -> OverlayEvent {
        if let Some(spec) = &self.choice {
            return choice_action(spec, &spec.items[index].value, spec.verb);
        }
        if self.entries[index]
            .command
            .args_hint
            .as_deref()
            .is_some_and(|hint| hint.starts_with('<'))
        {
            self.arguments = Some((index, Editor::default()));
            OverlayEvent::None
        } else {
            OverlayEvent::Action(format!("command:/{}", self.entries[index].command.name))
        }
    }

    /// Screen row after the panel border and mode-specific headers.
    fn list_top(&self) -> usize {
        if self.choice.is_some() { 3 } else { 4 }
    }

    /// A left click on screen `row`: selects the action drawn there; clicking the selected action
    /// runs it, like Enter. Rows follow `compose`: border, search, groups, count, then the list.
    pub fn click(&mut self, row: u16) -> OverlayEvent {
        let list_top = self.list_top();
        if self.arguments.is_some() {
            return OverlayEvent::None;
        }
        let Some(offset) = usize::from(row).checked_sub(list_top) else {
            return OverlayEvent::None;
        };
        if offset >= self.page {
            return OverlayEvent::None;
        }
        let rows = self.rows();
        let position = self.scroll + offset;
        let Some(&index) = rows.get(position) else {
            return OverlayEvent::None;
        };
        if position == self.cursor {
            return self.activate(index);
        }
        self.cursor = position;
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let extra = if self.busy { "turn running" } else { "ready" };
        let title = self
            .choice
            .as_ref()
            .map_or("Actions", |spec| spec.title.as_str())
            .to_owned();
        panel(frame, ctx.theme, Section::Dial, &title, extra, |w, h| {
            self.compose(w, h, ctx)
        });
    }

    /// Key hints for the current mode. Arguments and pickers list only what they can do here.
    fn footer(&self, keys: &Keymap, theme: &Theme, w: usize) -> Line<'static> {
        let mut items = vec![keys_hint(keys, Action::SelectConfirm, "choose")];
        if let Some(spec) = &self.choice {
            items[0] = keys_hint(
                keys,
                Action::SelectConfirm,
                if spec.verb.is_empty() && spec.items.iter().any(|item| item.current) {
                    "apply"
                } else if spec.verb.is_empty() {
                    "choose"
                } else {
                    spec.verb
                },
            );
            if spec.remove.is_some() {
                items.push("Delete/x remove".to_owned());
            }
        } else if self.arguments.is_some() {
            items[0] = keys_hint(keys, Action::SelectConfirm, "run");
        } else {
            items.push(keys_hint(keys, Action::SelectDown, "navigate"));
            items.push(keys_hint(keys, Action::SectionNext, "category"));
            if self
                .rows()
                .get(self.cursor)
                .is_some_and(|&index| self.entries[index].command.args_hint.is_some())
            {
                items.push(keys_hint(keys, Action::Newline, "arguments"));
            }
        }
        items.push(keys_hint(keys, Action::SelectCancel, self.cancel_label()));
        hint_line(theme, &items, w)
    }

    /// Usage and the full description of the selected command, drawn under the menu. Both rows are
    /// ellipsized to the width, so the detail never wraps into the composer.
    fn compose(&mut self, width: u16, height: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(width), usize::from(height));
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let theme = ctx.theme;
        let ascii = theme.ascii();
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
            out.push(line(
                format!(
                    " {}",
                    text::ellipsize(&command.description, w.saturating_sub(1), ascii)
                ),
                Tok::Text,
            ));
            if let Some(notice) = &self.notice {
                out.push(line(format!(" {notice}"), Tok::Warning));
            } else if self.busy {
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
            let prompt = if !self.search.text.is_empty() {
                " Search: "
            } else if self.choice.is_some() {
                " Filter choices: "
            } else {
                " Find an action: "
            };
            let mut spans = vec![Span::styled(prompt, theme.fg(Tok::Accent))];
            spans.extend(editor_spans(
                theme,
                &self.search,
                w.saturating_sub(text::width(prompt) + 1),
                false,
            ));
            out.push(filled(spans, w, Style::default()));
            let picker = self.choice.is_some();
            // Keep the active category visible even when all category names do not fit.
            if !picker {
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
                        Span::styled(format!(" {name} "), tab_style(theme, i == self.group))
                    })
                    .collect();
                out.push(filled(tabs, w, Style::default()));
            }
            let rows = self.rows();
            self.cursor = self.cursor.min(rows.len().saturating_sub(1));
            // The active tab says what it is for, so the categories teach themselves.
            let (label, purpose) = match &self.choice {
                Some(spec) if spec.items.iter().any(|item| item.current) => {
                    ("Choose", "the current value is marked")
                }
                Some(_) => ("Choose", "select an option to continue"),
                None => (
                    GROUPS[self.group],
                    crate::ui::command_surface::Category::ALL
                        .iter()
                        .find(|c| c.tab() == GROUPS[self.group])
                        .map(|c| c.blurb())
                        .unwrap_or(match GROUPS[self.group] {
                            "All" => "everyday actions; type to search all of them",
                            _ => "your commands from .dial/commands and installed skills",
                        }),
                ),
            };
            out.push(line(
                format!(
                    " {label} · {purpose} · {}/{}",
                    if rows.is_empty() { 0 } else { self.cursor + 1 },
                    rows.len()
                ),
                Tok::Muted,
            ));
            let headers = out.len();
            let list_h = h
                .saturating_sub(headers + 5)
                .max(1)
                .min(h.saturating_sub(headers + 1));
            self.page = list_h;
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
                        theme.bg(Tok::SelectedBg).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let name_w = (w / 2).clamp(1, 32);
                    let right = if w < 64 {
                        String::new()
                    } else {
                        match &self.choice {
                            Some(spec) if spec.items[index].current => "current".to_owned(),
                            Some(_) => String::new(),
                            None => shortcut(&entry.command.name)
                                .map(|a| ctx.keys.labels(a))
                                .unwrap_or_else(|| group(&entry.command).into()),
                        }
                    };
                    let right_w = text::width(&right);
                    let description_w = w.saturating_sub(name_w + right_w + 6);
                    let name = if picker {
                        entry.command.name.as_str()
                    } else {
                        action_label(&entry.command)
                    };
                    let description = if description_w == 0 {
                        String::new()
                    } else {
                        text::ellipsize(&entry.command.description, description_w, ascii)
                    };
                    out.push(filled(
                        vec![
                            Span::styled(
                                format!(
                                    " {} ",
                                    if selected {
                                        theme.sym(Sym::Pointer)
                                    } else if self
                                        .choice
                                        .as_ref()
                                        .is_some_and(|spec| spec.items[index].current)
                                    {
                                        "*"
                                    } else {
                                        " "
                                    }
                                ),
                                theme.fg(Tok::Accent).patch(fill),
                            ),
                            Span::styled(
                                text::pad(&text::ellipsize(name, name_w, ascii), name_w),
                                theme.fg(Tok::Text).patch(fill),
                            ),
                            Span::styled(
                                format!(" {} ", text::pad(&description, description_w)),
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
                let header = match &self.choice {
                    Some(spec) => format!(
                        " /{}",
                        words(&[&spec.command, spec.verb, &spec.items[index].value])
                    ),
                    None => format!(
                        " /{} {}",
                        entry.command.name,
                        entry.command.args_hint.as_deref().unwrap_or("")
                    )
                    .trim_end()
                    .to_owned(),
                };
                out.push(line(text::ellipsize(&header, w, ascii), Tok::Accent));
                // Two description rows; when the text needs more, the second row ends in an
                // ellipsis instead of silently dropping the rest.
                let room = w.saturating_sub(2).max(1);
                let wrapped = text::wrap_words(&entry.command.description, room);
                let mut shown: Vec<String> = wrapped.iter().take(2).cloned().collect();
                if wrapped.len() > 2 {
                    shown[1] = text::ellipsize(&wrapped[1..].join(" "), room, ascii);
                }
                out.extend(
                    shown
                        .into_iter()
                        .map(|value| line(format!(" {value}"), Tok::Text)),
                );
                out.push(line(
                    if self.choice.is_some() {
                        " Your chat draft is preserved.".into()
                    } else if self.busy {
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
        // Controls must survive short terminals; details yield space before the footer does.
        out.truncate(h.saturating_sub(1));
        out.push(self.footer(ctx.keys, theme, w));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::settings::testkit::*;
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn consistent_ux_advanced_commands_stay_searchable_outside_the_everyday_menu() {
        let mut entries = entries();
        entries.push(PaletteEntry {
            command: CommandEntry {
                name: "ratchet".into(),
                description: "Advanced verification loop".into(),
                kind: CommandKind::Builtin,
                args_hint: None,
            },
            during_turn: "Runs after this turn.",
        });
        let mut palette = PaletteOverlay::new(entries, false);
        assert!(
            !palette
                .rows()
                .iter()
                .any(|&i| palette.entries[i].command.name == "ratchet")
        );
        palette.handle(&Event::Paste("ratchet".into()), &Keymap::defaults());
        assert_eq!(palette.entries[palette.rows()[0]].command.name, "ratchet");
    }

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
        palette.handle(&key(KeyCode::Enter, KeyModifiers::SHIFT), &keys);
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
        assert!(lines.last().unwrap().contains("close"));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("All") && l.contains("Extensions"))
        );
    }

    #[test]
    fn task_labels_are_searchable_without_changing_command_dispatch() {
        let keys = Keymap::defaults();
        let mut palette = PaletteOverlay::new(entries(), false);
        palette.handle(&Event::Paste("Choose a model".into()), &keys);
        assert_eq!(palette.rows().len(), 1);
        assert!(matches!(palette.handle(&plain(KeyCode::Enter), &keys),
            OverlayEvent::Action(action) if action == "command:/model"));
        let custom = CommandEntry {
            name: "model".into(),
            description: "My workflow".into(),
            kind: CommandKind::Custom,
            args_hint: None,
        };
        assert_eq!(action_label(&custom), "model");
    }

    #[test]
    fn short_panels_keep_cancel_visible_and_clicks_match_drawn_rows() {
        let rig = Rig::new();
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        for picker in [false, true] {
            let mut palette = if picker {
                PaletteOverlay::choice(ChoiceSpec {
                    title: "Choice".into(),
                    command: "theme".into(),
                    verb: "",
                    remove: None,
                    items: ["dark", "light"]
                        .into_iter()
                        .map(|value| ChoiceItem {
                            value: value.into(),
                            label: value.into(),
                            detail: String::new(),
                            current: false,
                        })
                        .collect(),
                })
            } else {
                PaletteOverlay::new(entries(), false)
            };
            for height in [4, 6, 8, 12] {
                let rows = palette.compose(40, height, &ctx);
                let footer: String = rows
                    .last()
                    .unwrap()
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect();
                assert!(footer.contains("Esc"), "{footer}");
                assert_eq!(rows.len(), usize::from(height));
            }
            let buffer = render(80, 24, |frame| palette.draw(frame, &ctx));
            let first_row = rows(&buffer)[palette.list_top()].clone();
            assert!(first_row.contains(if picker { "dark" } else { "Choose a model" }));
            let expected = if picker {
                "command:/theme dark"
            } else {
                "command:/model"
            };
            assert!(
                matches!(palette.click(palette.list_top() as u16), OverlayEvent::Action(action) if action == expected)
            );
        }
    }
}

#[cfg(test)]
mod mode_tests {
    use super::super::settings::testkit::*;
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn plain(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn entries() -> Vec<PaletteEntry> {
        [("rename", "Name this conversation", Some("<title>"))]
            .into_iter()
            .map(|(name, description, args)| PaletteEntry {
                command: CommandEntry {
                    name: name.into(),
                    description: description.into(),
                    kind: CommandKind::Builtin,
                    args_hint: args.map(str::to_owned),
                },
                during_turn: "Runs after the current turn finishes.",
            })
            .collect()
    }

    fn item(value: &str, current: bool) -> ChoiceItem {
        ChoiceItem {
            value: value.into(),
            label: value.into(),
            detail: format!("{value} detail"),
            current,
        }
    }

    fn rendered(palette: &mut PaletteOverlay, width: u16) -> Vec<String> {
        let rig = Rig::new();
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        palette
            .compose(width, 24, &ctx)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn footer_lists_only_the_keys_each_mode_uses() {
        let keys = Keymap::defaults();
        let mut palette = PaletteOverlay::new(entries(), false);
        let footer = rendered(&mut palette, 100).pop().unwrap();
        assert!(
            footer.contains("Tab category")
                && footer.contains("arguments")
                && footer.contains("close"),
            "{footer}"
        );
        palette.handle(&Event::Paste("rename".into()), &keys);
        palette.handle(&plain(KeyCode::Enter), &keys);
        let footer = rendered(&mut palette, 100).pop().unwrap();
        assert!(
            footer.contains("back") && !footer.contains("Tab") && !footer.contains("arguments"),
            "{footer}"
        );
    }

    #[test]
    fn empty_required_argument_says_which_value_is_missing() {
        let keys = Keymap::defaults();
        let mut palette = PaletteOverlay::new(entries(), false);
        palette.handle(&Event::Paste("rename".into()), &keys);
        palette.handle(&plain(KeyCode::Enter), &keys);
        assert!(matches!(
            palette.handle(&plain(KeyCode::Enter), &keys),
            OverlayEvent::None
        ));
        assert!(
            rendered(&mut palette, 100)
                .iter()
                .any(|l| l.contains("Enter a value for <title>")),
        );
    }

    #[test]
    fn picker_applies_the_selected_value_and_marks_the_current_one() {
        let keys = Keymap::defaults();
        let spec = ChoiceSpec {
            title: "Approval".into(),
            command: "approval".into(),
            verb: "",
            remove: None,
            items: vec![item("ask", false), item("write", true), item("auto", false)],
        };
        let mut palette = PaletteOverlay::choice(spec);
        assert!(
            rendered(&mut palette, 100)
                .iter()
                .any(|l| l.contains("current"))
        );
        palette.handle(&plain(KeyCode::Down), &keys);
        assert!(matches!(
            palette.handle(&plain(KeyCode::Enter), &keys),
            OverlayEvent::Action(action) if action == "command:/approval write"
        ));
        assert!(matches!(
            palette.handle(&plain(KeyCode::Esc), &keys),
            OverlayEvent::Close
        ));
    }

    #[test]
    fn picker_with_remove_edits_or_removes_the_selected_row() {
        let keys = Keymap::defaults();
        let spec = ChoiceSpec {
            title: "Queued messages".into(),
            command: "queue".into(),
            verb: "edit",
            remove: Some("remove"),
            items: vec![item("1", false), item("2", false)],
        };
        let mut palette = PaletteOverlay::choice(spec);
        palette.handle(&plain(KeyCode::Down), &keys);
        assert!(matches!(
            palette.handle(&plain(KeyCode::Enter), &keys),
            OverlayEvent::Action(action) if action == "command:/queue edit 2"
        ));
        assert!(matches!(
            palette.handle(&plain(KeyCode::Delete), &keys),
            OverlayEvent::Action(action) if action == "command:/queue remove 2"
        ));
        assert!(matches!(
            palette.handle(&plain(KeyCode::Char('x')), &keys),
            OverlayEvent::Action(action) if action == "command:/queue remove 2"
        ));
    }

    #[test]
    fn long_details_end_in_an_ellipsis_instead_of_disappearing() {
        let mut palette = PaletteOverlay::new(
            vec![PaletteEntry {
                command: CommandEntry {
                    name: "verbose".into(),
                    description: "word ".repeat(60),
                    kind: CommandKind::Custom,
                    args_hint: None,
                },
                during_turn: "later",
            }],
            false,
        );
        let lines = rendered(&mut palette, 40);
        assert!(lines.iter().any(|l| l.contains('…')), "{lines:?}");
    }
}
