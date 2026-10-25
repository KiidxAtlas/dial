//! Searchable application actions. The controller supplies the real command registry and dispatch
//! policy; this panel never touches the chat draft, starts compute, or executes commands itself.
//! The same panel serves as a picker ([`ChoiceSpec`]) for commands that take one value.
//!
//! Browsing starts at a short list of goals, each opening the registry commands for one task.
//! Typed search always covers every registered command; the category tabs stay one key away.
use super::{
    DrawCtx, OverlayEvent,
    settings::{editor_spans, filled, hint_line, panel, pressed, tab_strip, typed_char},
};
use crate::ui::{
    chrome::{self, Section},
    composer::{CommandEntry, CommandKind},
    editor::Editor,
    keys::{Action, Keymap},
    text,
    theme::{Sym, Theme, Tok},
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
};

/// Tabs for the category view: everything, one per command category, then custom commands and
/// skills.
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

/// A task-first starting point. `commands` are registry names in display order; names that are
/// not registered are skipped. `custom` also takes user commands and skills.
struct Goal {
    title: &'static str,
    blurb: &'static str,
    commands: &'static [&'static str],
    custom: bool,
}

/// The first screen. Every command stays reachable from typed search and the category tabs.
const GOALS: &[Goal] = &[
    Goal {
        title: "Start or continue work",
        blurb: "begin, resume or plan a conversation",
        commands: &[
            "new", "resume", "sessions", "model", "plan", "rename", "fork", "help",
        ],
        custom: false,
    },
    Goal {
        title: "Inspect or review changes",
        blurb: "see what changed, review it, then undo or commit",
        commands: &[
            "changes",
            "review",
            "checks",
            "undo",
            "commit",
            "rollback",
            "code-history",
            "plan-review",
        ],
        custom: false,
    },
    Goal {
        title: "Manage ongoing work",
        blurb: "background agents, processes, queued messages and todos",
        commands: &[
            "workers",
            "processes",
            "todo",
            "queue",
            "goal",
            "message",
            "cancel",
            "compute",
        ],
        custom: false,
    },
    Goal {
        title: "Configure Dial",
        blurb: "providers, permissions, appearance, skills and extensions",
        commands: &[
            "providers",
            "settings",
            "setup",
            "permissions",
            "approval",
            "effort",
            "theme",
            "skills",
            "memory",
            "extensions",
            "compact",
        ],
        custom: true,
    },
];

/// Blurb for the row that opens the category tabs.
const ALL_BLURB: &str = "every command by category; type to search all of them";

/// Getting-started actions lead the category view's unfiltered list; defined with the other
/// starter lists.
const STARTERS: &[&str] = crate::ui::command_surface::STARTERS;

impl Goal {
    fn holds(&self, command: &CommandEntry) -> bool {
        self.commands.contains(&command.name.as_str())
            || (self.custom && command.kind != CommandKind::Builtin)
    }

    /// Display position inside the goal; unlisted (custom) commands follow the listed ones.
    fn position(&self, command: &CommandEntry) -> usize {
        self.commands
            .iter()
            .position(|name| *name == command.name)
            .unwrap_or(self.commands.len())
    }
}

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
///
/// The copy fields are optional: an empty `prompt` reads `Filter choices:`, an empty `summary`
/// describes the list generically, and `current_tag` names the marked item (`None` reads
/// `current`; an empty string hides the tag and its marker when the label already says it).
#[derive(Default)]
pub struct ChoiceSpec {
    pub title: String,
    pub command: String,
    pub verb: &'static str,
    pub remove: Option<&'static str>,
    pub items: Vec<ChoiceItem>,
    pub prompt: String,
    pub summary: String,
    pub current_tag: Option<String>,
}

impl ChoiceSpec {
    /// The right-column tag for the current item; empty when the picker hides it.
    fn current_tag(&self) -> &str {
        self.current_tag.as_deref().unwrap_or("current")
    }
}

/// Which list the panel shows when nothing is typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    /// The goal list (the first screen).
    Goals,
    /// The commands of one goal, by index into `GOALS`.
    Goal(usize),
    /// The category tabs, showing `group`.
    Tabs,
}

/// One selectable line: a goal, the all-actions door, or a command entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Goal(usize),
    AllActions,
    Entry(usize),
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

/// The one-line description for a picker row. A detail whose first paragraph repeats the label
/// drops it, so the list column shows what the label does not already say.
fn list_detail(label: &str, detail: &str) -> String {
    let rest = match detail.split_once("\n\n") {
        Some((first, rest)) if label.contains(first.trim()) => rest,
        None if label.contains(detail.trim()) => "",
        _ => detail,
    };
    text::one_line(rest)
}

/// The action that runs `/command verb value` for a picker row.
fn choice_action(spec: &ChoiceSpec, value: &str, verb: &str) -> OverlayEvent {
    OverlayEvent::Action(format!("command:/{}", words(&[&spec.command, verb, value])))
}

pub struct PaletteOverlay {
    entries: Vec<PaletteEntry>,
    search: Editor,
    view: View,
    group: usize,
    cursor: usize,
    scroll: usize,
    page: usize,
    /// Interactive rows from the last bordered panel draw; absent before drawing or on tiny panels.
    list_bounds: Option<Rect>,
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
            view: View::Goals,
            group: 0,
            cursor: 0,
            scroll: 0,
            page: 8,
            list_bounds: None,
            arguments: None,
            choice: None,
            notice: None,
            busy,
        }
    }

    /// A picker over `spec.items`, reusing the palette's search, cursor, panel and footer. The
    /// cursor starts on the current value, so reopening a picker lands on what is already set.
    pub fn choice(spec: ChoiceSpec) -> Self {
        let mut palette = Self::new(Vec::new(), false);
        palette.entries = spec
            .items
            .iter()
            .map(|item| PaletteEntry {
                command: CommandEntry {
                    name: item.label.clone(),
                    description: list_detail(&item.label, &item.detail),
                    kind: CommandKind::Builtin,
                    args_hint: None,
                },
                during_turn: "",
            })
            .collect();
        palette.cursor = spec.items.iter().position(|item| item.current).unwrap_or(0);
        palette.choice = Some(spec);
        palette
    }

    /// Keeps the "turn running" wording in step with the shell while the palette stays open.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    fn rows(&self) -> Vec<Row> {
        let query = self.search.text.trim().trim_start_matches('/');
        let picker = self.choice.is_some();
        if !picker && query.is_empty() {
            match self.view {
                View::Goals => {
                    return (0..GOALS.len())
                        .map(Row::Goal)
                        .chain(std::iter::once(Row::AllActions))
                        .collect();
                }
                View::Goal(goal) => {
                    let goal = &GOALS[goal];
                    let mut rows: Vec<_> = self
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| goal.holds(&entry.command))
                        .map(|(index, _)| index)
                        .collect();
                    rows.sort_by_key(|&index| goal.position(&self.entries[index].command));
                    return rows.into_iter().map(Row::Entry).collect();
                }
                View::Tabs => {}
            }
        }
        let tabs = !picker && self.view == View::Tabs;
        let mut rows: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if tabs
                    && query.is_empty()
                    && self.group != 0
                    && group(&entry.command) != GROUPS[self.group]
                {
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
        if query.is_empty() && tabs {
            rows.sort_by_key(|(index, _)| {
                let name = self.entries[*index].command.name.as_str();
                STARTERS
                    .iter()
                    .position(|starter| *starter == name)
                    .unwrap_or(STARTERS.len())
            });
        }
        rows.into_iter()
            .map(|(index, _)| Row::Entry(index))
            .collect()
    }

    /// Command entries among the rows, in order. Picker rows are all entries.
    fn entry_rows(&self) -> Vec<usize> {
        self.rows()
            .into_iter()
            .filter_map(|row| match row {
                Row::Entry(index) => Some(index),
                _ => None,
            })
            .collect()
    }

    /// Number of registered commands a goal opens; the goal row shows it.
    fn goal_size(&self, goal: usize) -> usize {
        self.entries
            .iter()
            .filter(|entry| GOALS[goal].holds(&entry.command))
            .count()
    }

    fn reset_selection(&mut self) {
        // Input may be drained in a batch before drawing. Never map an old painted row onto
        // a newly filtered list or view; selection-only clicks retain their existing hitbox.
        self.list_bounds = None;
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Leaves a goal or the category tabs for the goal list, with the cursor on the row left.
    /// Returns false when already on the goal list (or in a picker), so Esc can close instead.
    fn back_to_goals(&mut self) -> bool {
        if self.choice.is_some() {
            return false;
        }
        let cursor = match self.view {
            View::Goal(goal) => goal,
            View::Tabs => GOALS.len(),
            View::Goals => return false,
        };
        self.view = View::Goals;
        self.list_bounds = None;
        self.cursor = cursor;
        self.scroll = 0;
        true
    }

    /// Whether the category tab strip is drawn (and therefore shifts the list down one row).
    fn shows_tabs(&self) -> bool {
        self.choice.is_none() && self.view == View::Tabs
    }

    /// What Esc does in the current mode, so the footer names it correctly.
    fn cancel_label(&self) -> &'static str {
        if self.arguments.is_some() {
            "back"
        } else if !self.search.text.is_empty() {
            "clear search"
        } else if self.choice.is_none() && self.view != View::Goals {
            "back"
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
            if self.back_to_goals() {
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
                return match self.entry_rows().get(self.cursor) {
                    Some(&index) => choice_action(spec, &spec.items[index].value, verb),
                    None => OverlayEvent::None,
                };
            }
        }
        if self.choice.is_none()
            && (keys.matches(Action::SectionNext, key) || keys.matches(Action::SectionPrev, key))
        {
            let forward = keys.matches(Action::SectionNext, key);
            if self.view == View::Tabs {
                self.group =
                    (self.group + if forward { 1 } else { GROUPS.len() - 1 }) % GROUPS.len();
            } else {
                // Tab leaves the goal list for the category tabs, starting at "All".
                self.view = View::Tabs;
                self.group = 0;
            }
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
            if let Some(&row) = rows.get(self.cursor) {
                return self.activate(row);
            }
        } else if keys.matches(Action::Newline, key) && self.choice.is_none() {
            if let Some(&Row::Entry(index)) = rows.get(self.cursor) {
                self.list_bounds = None;
                self.arguments = Some((index, Editor::default()));
            }
        } else if self.search.text.is_empty() && key.code == KeyCode::Home {
            self.cursor = 0;
        } else if self.search.text.is_empty() && key.code == KeyCode::End {
            self.cursor = rows.len().saturating_sub(1);
        } else if let Some(c) = typed_char(key) {
            self.search.insert(&c.to_string());
            self.reset_selection();
        } else if self.search.key(*key) {
            self.reset_selection();
        }
        OverlayEvent::None
    }

    /// Open a goal or the tabs, or run the entry (or ask for its required arguments first).
    fn activate(&mut self, row: Row) -> OverlayEvent {
        match row {
            Row::Goal(goal) => {
                self.view = View::Goal(goal);
                self.reset_selection();
                OverlayEvent::None
            }
            Row::AllActions => {
                self.view = View::Tabs;
                self.group = 0;
                self.reset_selection();
                OverlayEvent::None
            }
            Row::Entry(index) => {
                if let Some(spec) = &self.choice {
                    return choice_action(spec, &spec.items[index].value, spec.verb);
                }
                if self.entries[index]
                    .command
                    .args_hint
                    .as_deref()
                    .is_some_and(|hint| hint.starts_with('<'))
                {
                    self.list_bounds = None;
                    self.arguments = Some((index, Editor::default()));
                    OverlayEvent::None
                } else {
                    OverlayEvent::Action(format!("command:/{}", self.entries[index].command.name))
                }
            }
        }
    }

    /// Screen row after the panel border and mode-specific headers.
    fn list_top(&self) -> usize {
        // Border, search, count and list; the category tab strip adds one more row.
        if self.shows_tabs() { 4 } else { 3 }
    }

    /// Select the action drawn at this screen position; clicking the selected action runs it.
    pub fn click(&mut self, column: u16, row: u16) -> OverlayEvent {
        let Some(bounds) = self.list_bounds else {
            return OverlayEvent::None;
        };
        if self.arguments.is_some() || !bounds.contains(Position::new(column, row)) {
            return OverlayEvent::None;
        }
        let offset = usize::from(row - bounds.y);
        let rows = self.rows();
        let position = self.scroll + offset;
        let Some(&picked) = rows.get(position) else {
            return OverlayEvent::None;
        };
        if position == self.cursor {
            return self.activate(picked);
        }
        self.cursor = position;
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        // Only a running turn is worth a title note; an idle palette needs no status.
        let title = self
            .choice
            .as_ref()
            .map_or("Actions", |spec| spec.title.as_str())
            .to_owned();
        let extras: &[&str] = if self.busy {
            &["turn running", ""]
        } else {
            &[""]
        };
        let extra = title_extra(ctx.theme, &title, frame.area().width, extras);
        let area = frame.area();
        self.list_bounds = None;
        let inner = (area.width >= 5 && area.height >= 5).then(|| {
            chrome::panel_block_titled(ctx.theme, Section::Dial, &title, &extra).inner(area)
        });
        panel(frame, ctx.theme, Section::Dial, &title, &extra, |w, h| {
            self.compose(w, h, ctx)
        });
        if let Some(inner) = inner
            && self.arguments.is_none()
        {
            let headers = self.list_top() as u16 - 1;
            let visible = self.page.min(self.rows().len().saturating_sub(self.scroll)) as u16;
            let height = visible.min(inner.height.saturating_sub(headers + 1));
            if inner.width > 0 && height > 0 {
                self.list_bounds = Some(Rect::new(inner.x, inner.y + headers, inner.width, height));
            }
        }
    }

    /// Key hints for the current mode. Arguments and pickers list only what they can do here, and
    /// selection keys are left out when the list is empty, so nothing advertises a no-op.
    fn footer(&self, keys: &Keymap, theme: &Theme, w: usize) -> Line<'static> {
        let mut items = Vec::new();
        let rows = self.rows();
        let empty = self.arguments.is_none() && rows.is_empty();
        if let Some(spec) = &self.choice {
            if !empty {
                let verb = if !spec.verb.is_empty() {
                    spec.verb
                } else if spec.items.iter().any(|item| item.current) {
                    "apply"
                } else {
                    "choose"
                };
                items.push(keys_hint(keys, Action::SelectConfirm, verb));
            }
            if spec.remove.is_some() && !empty {
                items.push("Delete/x remove".to_owned());
            }
        } else if self.arguments.is_some() {
            items.push(keys_hint(keys, Action::SelectConfirm, "run"));
        } else if !empty {
            items.push(keys_hint(keys, Action::SelectConfirm, "choose"));
            items.push(keys_hint(keys, Action::SelectDown, "navigate"));
            items.push(keys_hint(keys, Action::SectionNext, "category"));
            if matches!(
                rows.get(self.cursor),
                Some(&Row::Entry(index)) if self.entries[index].command.args_hint.is_some()
            ) {
                items.push(keys_hint(keys, Action::Newline, "arguments"));
            }
        } else {
            // Nothing to select: the only useful move is to look at another category.
            items.push(keys_hint(keys, Action::SectionNext, "category"));
        }
        items.push(keys_hint(keys, Action::SelectCancel, self.cancel_label()));
        hint_line(theme, &items, w)
    }

    /// What an empty list says, naming the key that gets the user out of the dead end.
    fn empty_message(&self) -> String {
        let searching = !self.search.text.trim().is_empty();
        match (self.choice.is_some(), searching, self.view) {
            (true, true, _) => " No choices match. Esc clears the search.",
            (true, false, _) => " No choices to pick from.",
            (false, true, _) => " No actions match. Esc clears the search; Tab changes category.",
            (false, false, View::Goal(_)) => " No actions in this goal. Esc goes back.",
            (false, false, _) => " No actions in this category. Tab changes category.",
        }
        .to_owned()
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
            } else if self.busy && !self.entries[*index].during_turn.is_empty() {
                // What running it now does to the turn in progress.
                out.push(line(
                    format!(" {}", self.entries[*index].during_turn),
                    Tok::Muted,
                ));
            }
            out.push(line(
                " Type the arguments; Enter runs the command. Your chat draft is kept.".into(),
                Tok::Muted,
            ));
        } else {
            let prompt = match &self.choice {
                Some(spec) if !spec.prompt.is_empty() => format!(" {} ", spec.prompt.trim()),
                Some(_) => " Filter choices: ".to_owned(),
                None if !self.search.text.is_empty() => " Search: ".to_owned(),
                None => " Find an action: ".to_owned(),
            };
            let mut spans = vec![Span::styled(prompt.clone(), theme.fg(Tok::Accent))];
            spans.extend(editor_spans(
                theme,
                &self.search,
                w.saturating_sub(text::width(&prompt) + 1),
                false,
            ));
            out.push(filled(spans, w, Style::default()));
            if self.shows_tabs() {
                // Windowed around the active category, with an ellipsis where more tabs sit.
                out.push(filled(
                    tab_strip(theme, GROUPS, self.group, true, w).spans,
                    w,
                    Style::default(),
                ));
            }
            let rows = self.rows();
            self.cursor = self.cursor.min(rows.len().saturating_sub(1));
            // The header says what the list is for, so each view teaches itself. A search says
            // where its matches came from instead of repeating the view's purpose.
            let searching = !self.search.text.trim().is_empty();
            let (label, purpose) = match (&self.choice, self.view) {
                (Some(spec), _) if !spec.summary.is_empty() => {
                    (spec.summary.as_str(), String::new())
                }
                (Some(spec), _)
                    if !spec.current_tag().is_empty()
                        && spec.items.iter().any(|item| item.current) =>
                {
                    ("Choose", "the current value is marked".to_owned())
                }
                (Some(_), _) => ("Choose", "select an option to continue".to_owned()),
                (None, _) if searching => (
                    "Search",
                    match self.view {
                        View::Tabs => "all categories".to_owned(),
                        _ => "all commands".to_owned(),
                    },
                ),
                (None, View::Goals) => (
                    "Start here",
                    "pick a goal, or type to search every command".to_owned(),
                ),
                (None, View::Goal(goal)) => (GOALS[goal].title, GOALS[goal].blurb.to_owned()),
                (None, View::Tabs) => (
                    GROUPS[self.group],
                    crate::ui::command_surface::Category::ALL
                        .iter()
                        .find(|c| c.tab() == GROUPS[self.group])
                        .map(|c| c.blurb())
                        .unwrap_or(match GROUPS[self.group] {
                            "All" => "all registered actions; type to search",
                            _ => "your commands from .dial/commands and installed skills",
                        })
                        .to_owned(),
                ),
            };
            // The count is what someone scanning the list needs, so the purpose gives way first.
            // An empty list has no position to report; the empty message says what to do.
            let dot = ctx.theme.sym(Sym::Dot);
            let tail = if rows.is_empty() {
                String::new()
            } else {
                format!(" {dot} {}/{}", self.cursor + 1, rows.len())
            };
            let head = if purpose.is_empty() {
                format!(" {label}")
            } else {
                format!(" {label} {dot} ")
            };
            let room = w.saturating_sub(text::width(&head) + text::width(&tail));
            let header = if purpose.is_empty() {
                // A picker summary is the whole header: it shortens before the count does.
                format!(
                    " {}{tail}",
                    text::ellipsize(label, w.saturating_sub(1 + text::width(&tail)), ascii)
                )
            } else {
                format!("{head}{}{tail}", text::ellipsize(&purpose, room, ascii))
            };
            out.push(line(header, Tok::Muted));
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
            let picker = self.choice.is_some();
            let name_w = self.name_width(&rows, w);
            let mut pending_empty: Vec<String> = Vec::new();
            for row in 0..list_h {
                if let Some(&picked) = rows.get(self.scroll + row) {
                    let selected = self.scroll + row == self.cursor;
                    let fill = if selected {
                        theme.bg(Tok::SelectedBg).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let (name, description, right, current) = match picked {
                        Row::Goal(goal) => {
                            let n = self.goal_size(goal);
                            let right =
                                format!("{n} {}", if n == 1 { "action" } else { "actions" });
                            (
                                GOALS[goal].title.to_owned(),
                                GOALS[goal].blurb.to_owned(),
                                right,
                                false,
                            )
                        }
                        Row::AllActions => (
                            "All actions".to_owned(),
                            ALL_BLURB.to_owned(),
                            "categories".to_owned(),
                            false,
                        ),
                        Row::Entry(index) => {
                            let entry = &self.entries[index];
                            let (right, current) = match &self.choice {
                                Some(spec) if spec.items[index].current => (
                                    spec.current_tag().to_owned(),
                                    !spec.current_tag().is_empty(),
                                ),
                                Some(_) => (String::new(), false),
                                None => (
                                    shortcut(&entry.command.name)
                                        .map(|a| ctx.keys.labels(a))
                                        .unwrap_or_else(|| group(&entry.command).into()),
                                    false,
                                ),
                            };
                            let name = if picker {
                                entry.command.name.clone()
                            } else {
                                action_label(&entry.command).to_owned()
                            };
                            (name, entry.command.description.clone(), right, current)
                        }
                    };
                    let right = if w < 64 { String::new() } else { right };
                    let right_w = text::width(&right);
                    let description_w = w.saturating_sub(name_w + right_w + 6);
                    let description = if description_w == 0 {
                        String::new()
                    } else {
                        text::ellipsize(&description, description_w, ascii)
                    };
                    let mut spans = vec![
                        Span::styled(
                            format!(
                                " {} ",
                                if selected {
                                    theme.sym(Sym::Pointer)
                                } else if current {
                                    "*"
                                } else {
                                    " "
                                }
                            ),
                            theme.fg(Tok::Accent),
                        ),
                        Span::styled(
                            text::pad(&text::ellipsize(&name, name_w, ascii), name_w),
                            theme.fg(Tok::Text),
                        ),
                        Span::styled(
                            format!(" {} ", text::pad(&description, description_w)),
                            theme.fg(Tok::Muted),
                        ),
                        Span::styled(right, theme.fg(Tok::Muted)),
                    ];
                    if selected {
                        crate::ui::chrome::select_spans(theme, &mut spans);
                    }
                    out.push(filled(spans, w, fill));
                } else if row == 0 {
                    // The empty message names the way out, so it wraps rather than clipping it.
                    let room = w.saturating_sub(2).max(1);
                    let wrapped = text::wrap_words(self.empty_message().trim(), room);
                    let mut wrapped = wrapped.into_iter();
                    if let Some(first) = wrapped.next() {
                        out.push(line(format!(" {first}"), Tok::Muted));
                    }
                    pending_empty = wrapped.collect();
                } else if !pending_empty.is_empty() {
                    out.push(line(format!(" {}", pending_empty.remove(0)), Tok::Muted));
                } else {
                    out.push(Line::default());
                }
            }
            if let Some(&picked) = rows.get(self.cursor) {
                let (header, description, note) = match picked {
                    Row::Goal(goal) => (
                        format!(" {}", GOALS[goal].title),
                        GOALS[goal].blurb.to_owned(),
                        " Enter opens this goal. Your chat draft is preserved.".to_owned(),
                    ),
                    Row::AllActions => (
                        " All actions".to_owned(),
                        ALL_BLURB.to_owned(),
                        " Enter opens the categories. Your chat draft is preserved.".to_owned(),
                    ),
                    Row::Entry(index) => {
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
                        let note = if self.choice.is_some() {
                            " Your chat draft is preserved.".into()
                        } else if self.busy {
                            format!(" {}", entry.during_turn)
                        } else {
                            " Your chat draft is preserved.".into()
                        };
                        // The list column trims what the label repeats; the detail pane keeps all.
                        let description = match &self.choice {
                            Some(spec) => text::one_line(&spec.items[index].detail),
                            None => entry.command.description.clone(),
                        };
                        (header, description, note)
                    }
                };
                out.push(line(text::ellipsize(&header, w, ascii), Tok::Accent));
                // Two description rows; when the text needs more, the second row ends in an
                // ellipsis instead of silently dropping the rest.
                let room = w.saturating_sub(2).max(1);
                let wrapped = text::wrap_words(&description, room);
                let mut shown: Vec<String> = wrapped.iter().take(2).cloned().collect();
                if wrapped.len() > 2 {
                    shown[1] = text::ellipsize(&wrapped[1..].join(" "), room, ascii);
                }
                out.extend(
                    shown
                        .into_iter()
                        .map(|value| line(format!(" {value}"), Tok::Text)),
                );
                out.push(line(note, Tok::Muted));
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

    /// Width of the label column. Action names keep a fixed column; picker labels are the content,
    /// so they take the room their longest label needs, leaving a readable description column.
    fn name_width(&self, rows: &[Row], w: usize) -> usize {
        let fixed = (w / 2).clamp(1, 32);
        let Some(spec) = &self.choice else {
            return fixed;
        };
        let entries: Vec<&PaletteEntry> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Entry(index) => Some(&self.entries[*index]),
                _ => None,
            })
            .collect();
        let longest = entries
            .iter()
            .map(|entry| text::width(&entry.command.name))
            .max()
            .unwrap_or(1)
            .max(1);
        let described = entries
            .iter()
            .any(|entry| !entry.command.description.is_empty());
        let tag = if w < 64 {
            0
        } else {
            text::width(spec.current_tag())
        };
        // Narrow panels give the label the row; the detail pane below still shows the rest.
        let description = if described && w >= 64 {
            (w / 4).min(24)
        } else {
            0
        };
        longest.min(
            w.saturating_sub(tag + description + 6)
                .max(fixed.min(longest)),
        )
    }
}

/// The longest of `extras` that fits on a `width`-cell panel top row beside `title` without
/// touching it; the last candidate (usually empty) when none does.
pub(super) fn title_extra(theme: &Theme, title: &str, width: u16, extras: &[&str]) -> String {
    let title_w = 1 + crate::ui::chrome::fig_label(theme, Section::Dial, title)
        .iter()
        .map(|span| text::width(&span.content))
        .sum::<usize>();
    extras
        .iter()
        .find(|extra| extra.is_empty() || title_w + 2 + text::width(extra) < usize::from(width))
        .or(extras.last())
        .map(|extra| (*extra).to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod mode_tests;
