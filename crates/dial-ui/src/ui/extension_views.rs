//! Persistent, bounded declarative extension surfaces. This module only renders neutral snapshots
//! and emits revisioned intents; it cannot call tools or grant extension permissions.
use super::{
    Rows,
    keys::{Action, Keymap},
    markdown, text,
    theme::{Theme, Tok},
};
use crate::runtime::extensions::{Component, OwnedView, TreeNode};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::{Line, Span};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const MAX_ROWS: usize = 1024;
const MAX_INPUT: usize = 4096;

#[derive(Clone, Copy)]
struct RenderStyle<'a> {
    width: usize,
    theme: &'a Theme,
}

#[derive(Clone, Debug)]
enum Control {
    Activate,
    Input(String),
    Select(Vec<(String, String)>, String),
    Tree,
    Tab(String, String),
}
#[derive(Clone, Debug)]
struct Target {
    key: String,
    node: String,
    row: Option<String>,
    label: String,
    line: usize,
    control: Control,
}

/// An event is addressed to the snapshot that produced it, never to a future revision.
#[derive(Clone, Debug)]
pub struct ViewEvent {
    pub plugin: String,
    pub params: Value,
}

/// Local interaction state survives a newer snapshot if its addressed nodes still exist.
pub struct ViewState {
    snapshot: OwnedView,
    focus: Option<String>,
    scroll: usize,
    expanded: BTreeSet<String>,
    tabs: BTreeMap<String, String>,
    drafts: BTreeMap<String, String>,
    targets: Vec<Target>,
    search: String,
    searching: bool,
    page: usize,
}
impl ViewState {
    pub fn new(snapshot: OwnedView) -> Self {
        Self {
            snapshot,
            focus: None,
            scroll: 0,
            expanded: BTreeSet::new(),
            tabs: BTreeMap::new(),
            drafts: BTreeMap::new(),
            targets: Vec::new(),
            search: String::new(),
            searching: false,
            page: 8,
        }
    }
    pub fn snapshot(&self) -> &OwnedView {
        &self.snapshot
    }
    /// Reject reordered/stale publications, including same-revision replacement.
    pub fn update(&mut self, snapshot: OwnedView) -> bool {
        if snapshot.plugin != self.snapshot.plugin
            || snapshot.view.id != self.snapshot.view.id
            || snapshot.revision <= self.snapshot.revision
        {
            return false;
        }
        self.snapshot = snapshot;
        self.targets.clear();
        true
    }
    /// Render at most `height` rows, all clipped to `width` terminal cells. Empty/small panes work.
    pub fn rows(&mut self, width: usize, height: usize, theme: &Theme, focused: bool) -> Rows {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        self.page = height.saturating_sub(2).max(1);
        let mut body = Vec::new();
        let mut targets = Vec::new();
        self.elements(
            &self.snapshot.view.elements,
            "",
            RenderStyle { width, theme },
            &mut body,
            &mut targets,
            0,
        );
        body.truncate(MAX_ROWS);
        targets.retain(|target| target.line < body.len());
        let valid: BTreeSet<_> = targets.iter().map(|t| t.key.clone()).collect();
        self.drafts.retain(|key, _| valid.contains(key));
        self.expanded.retain(|key| valid.contains(key));
        if self.focus.as_ref().is_none_or(|key| !valid.contains(key)) {
            self.focus = targets
                .iter()
                .find(|t| t.line >= self.scroll && t.line < self.scroll + self.page)
                .map(|t| t.key.clone());
        }
        if focused
            && let Some(target) = targets.iter().find(|t| Some(&t.key) == self.focus.as_ref())
        {
            if target.line < self.scroll {
                self.scroll = target.line;
            }
            if target.line >= self.scroll + self.page {
                self.scroll = target.line + 1 - self.page;
            }
            if let Some(line) = body.get_mut(target.line) {
                *line = line.clone().style(theme.fg(Tok::Accent));
            }
        }
        self.scroll = self.scroll.min(body.len().saturating_sub(self.page));
        self.targets = targets;
        let title = format!(
            "{} / {}  [{}]",
            self.snapshot.plugin,
            self.snapshot.view.title,
            if focused {
                "focused · Esc composer"
            } else {
                "view"
            }
        );
        let mut out = vec![fit(&title, width, theme, Tok::Accent)];
        out.extend(
            body.into_iter()
                .skip(self.scroll)
                .take(height.saturating_sub(2)),
        );
        if height > 1 {
            let hint = if self.searching {
                format!("Find: {}", self.search)
            } else {
                "Tab focus · arrows move · Enter activate · / find · PgUp/PgDn scroll".into()
            };
            out.push(fit(&hint, width, theme, Tok::Muted));
        }
        out
    }
    fn elements(
        &self,
        elements: &[Component],
        prefix: &str,
        style: RenderStyle<'_>,
        rows: &mut Rows,
        targets: &mut Vec<Target>,
        depth: usize,
    ) {
        let RenderStyle { width, theme } = style;
        if depth > 8 {
            return;
        }
        for (i, element) in elements.iter().enumerate() {
            if rows.len() >= MAX_ROWS {
                break;
            }
            let path = format!("{prefix}/{i}");
            let mut add = |node: &str,
                           row: Option<String>,
                           control: Control,
                           label: String,
                           rows: &mut Rows| {
                let key = row
                    .as_ref()
                    .map_or_else(|| node.to_owned(), |r| format!("{node}#{r}"));
                targets.push(Target {
                    key,
                    node: node.into(),
                    row,
                    label: label.clone(),
                    line: rows.len(),
                    control,
                });
                rows.push(fit(&label, width, theme, Tok::Text));
            };
            match element {
                Component::Text { text: value } => {
                    for line in value.lines().take(MAX_ROWS.saturating_sub(rows.len())) {
                        rows.push(fit(line, width, theme, Tok::Text));
                    }
                }
                Component::Markdown { text: value } => rows.extend(
                    markdown::render(
                        value,
                        width.min(u16::MAX as usize) as u16,
                        theme,
                        theme.ascii(),
                    )
                    .into_iter()
                    .take(MAX_ROWS.saturating_sub(rows.len()))
                    .map(|line| clip(line, width, theme)),
                ),
                Component::Code {
                    text: value,
                    language,
                } => {
                    rows.push(fit(language, width, theme, Tok::Muted));
                    for line in value.lines().take(MAX_ROWS.saturating_sub(rows.len())) {
                        rows.push(fit(line, width, theme, Tok::MdCode));
                    }
                }
                Component::Diff {
                    before,
                    after,
                    path,
                } => {
                    rows.push(fit(path, width, theme, Tok::Muted));
                    // A bounded whole-file comparison: unchanged prefix/suffix are not marked changed.
                    let old: Vec<_> = before.lines().collect();
                    let new: Vec<_> = after.lines().collect();
                    let common = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
                    let suffix = old[common..]
                        .iter()
                        .rev()
                        .zip(new[common..].iter().rev())
                        .take_while(|(a, b)| a == b)
                        .count();
                    for (mark, lines) in [
                        (" ", &old[..common]),
                        ("-", &old[common..old.len() - suffix]),
                        ("+", &new[common..new.len() - suffix]),
                        (" ", &old[old.len() - suffix..]),
                    ] {
                        for line in lines.iter().take(MAX_ROWS.saturating_sub(rows.len())) {
                            let value = text::ellipsize(
                                &text::one_line(&format!("{mark}{line}")),
                                width,
                                theme.ascii(),
                            );
                            let style = markdown::diff_style(theme, &value);
                            rows.push(Line::from(Span::styled(value, style)));
                        }
                    }
                }
                Component::List { items } => {
                    for item in items.iter().take(MAX_ROWS.saturating_sub(rows.len())) {
                        let label = item.description.as_ref().map_or_else(
                            || item.label.clone(),
                            |d| format!("{} — {d}", item.label),
                        );
                        add(&path, Some(item.id.clone()), Control::Activate, label, rows);
                    }
                }
                Component::Table {
                    columns,
                    rows: table,
                } => {
                    rows.push(fit(
                        &columns
                            .iter()
                            .map(|c| c.label.as_str())
                            .collect::<Vec<_>>()
                            .join(" | "),
                        width,
                        theme,
                        Tok::Muted,
                    ));
                    for row in table.iter().take(MAX_ROWS.saturating_sub(rows.len())) {
                        add(
                            &path,
                            Some(row.id.clone()),
                            Control::Activate,
                            row.cells.join(" | "),
                            rows,
                        );
                    }
                }
                Component::Tree { nodes } => self.tree(nodes, &path, style, rows, targets, 0),
                Component::Tabs { tabs } => {
                    let active = self
                        .tabs
                        .get(&path)
                        .and_then(|id| tabs.iter().find(|t| &t.id == id))
                        .or_else(|| tabs.first());
                    for tab in tabs.iter().take(MAX_ROWS.saturating_sub(rows.len())) {
                        let chosen = active.is_some_and(|t| t.id == tab.id);
                        add(
                            &path,
                            Some(tab.id.clone()),
                            Control::Tab(path.clone(), tab.id.clone()),
                            format!("{} {}", if chosen { "[x]" } else { "[ ]" }, tab.title),
                            rows,
                        );
                    }
                    if let Some(tab) = active {
                        self.elements(
                            &tab.elements,
                            &format!("{path}/{}", tab.id),
                            style,
                            rows,
                            targets,
                            depth + 1,
                        );
                    }
                }
                Component::Progress {
                    value,
                    total,
                    label,
                } => {
                    let percent = if total.is_finite() && *total > 0.0 {
                        (value / total * 100.0).clamp(0.0, 100.0)
                    } else {
                        0.0
                    };
                    rows.push(fit(
                        &format!("{label} {percent:.0}%"),
                        width,
                        theme,
                        Tok::Success,
                    ));
                }
                Component::Button { id, label } => {
                    add(id, None, Control::Activate, format!("[ {label} ]"), rows)
                }
                Component::Input { id, label, value } => {
                    let shown = self.drafts.get(id).unwrap_or(value);
                    add(
                        id,
                        None,
                        Control::Input(value.clone()),
                        format!("{label}: {shown}"),
                        rows,
                    );
                }
                Component::Select {
                    id,
                    label,
                    options,
                    value,
                } => {
                    let selected = self.drafts.get(id).unwrap_or(value);
                    let shown = options
                        .iter()
                        .find(|o| &o.value == selected)
                        .map_or(selected.as_str(), |o| o.label.as_str());
                    add(
                        id,
                        None,
                        Control::Select(
                            options
                                .iter()
                                .map(|o| (o.value.clone(), o.label.clone()))
                                .collect(),
                            value.clone(),
                        ),
                        format!("{label}: < {shown} >"),
                        rows,
                    );
                }
            }
        }
    }
    fn tree(
        &self,
        nodes: &[TreeNode],
        path: &str,
        style: RenderStyle<'_>,
        rows: &mut Rows,
        targets: &mut Vec<Target>,
        depth: usize,
    ) {
        let RenderStyle { width, theme } = style;
        if depth > 8 {
            return;
        }
        for node in nodes {
            if rows.len() >= MAX_ROWS {
                return;
            }
            let key = format!("{path}#{}", node.id);
            let open = self.expanded.contains(&key);
            targets.push(Target {
                key,
                node: path.into(),
                row: Some(node.id.clone()),
                label: node.label.clone(),
                line: rows.len(),
                control: if node.children.is_empty() {
                    Control::Activate
                } else {
                    Control::Tree
                },
            });
            rows.push(fit(
                &format!(
                    "{}{} {}",
                    "  ".repeat(depth),
                    if node.children.is_empty() {
                        " "
                    } else if open {
                        "-"
                    } else {
                        "+"
                    },
                    node.label
                ),
                width,
                theme,
                Tok::Text,
            ));
            if open {
                self.tree(&node.children, path, style, rows, targets, depth + 1);
            }
        }
    }
    /// Handles a key only when the controller has focused this view. Built-in application shortcuts
    /// must be routed before this call. Text editing does not execute callbacks until confirmation.
    pub fn key(&mut self, key: KeyEvent, keys: &Keymap) -> Option<ViewEvent> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let actions = keys.actions_for(&key);
        if self.searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Char(c)
                    if (key.modifiers - KeyModifiers::SHIFT).is_empty()
                        && self.search.len() < 256 =>
                {
                    self.search.push(c)
                }
                _ => {}
            }
            if let Some(target) = self
                .targets
                .iter()
                .find(|t| t.label.to_lowercase().contains(&self.search.to_lowercase()))
            {
                self.focus = Some(target.key.clone());
            }
            return None;
        }
        let index = self
            .targets
            .iter()
            .position(|t| Some(&t.key) == self.focus.as_ref())
            .unwrap_or(0);
        let backwards =
            actions.contains(&Action::SelectUp) || actions.contains(&Action::SectionPrev);
        let forwards =
            actions.contains(&Action::SelectDown) || actions.contains(&Action::SectionNext);
        if (backwards || forwards) && !self.targets.is_empty() {
            let next = if backwards {
                index.checked_sub(1).unwrap_or(self.targets.len() - 1)
            } else {
                (index + 1) % self.targets.len()
            };
            self.focus = Some(self.targets[next].key.clone());
            return None;
        }
        if actions.contains(&Action::SelectPageDown) {
            self.scroll = self.scroll.saturating_add(self.page);
            self.focus = None;
            return None;
        }
        if actions.contains(&Action::SelectPageUp) {
            self.scroll = self.scroll.saturating_sub(self.page);
            self.focus = None;
            return None;
        }
        if self.targets.is_empty() {
            if backwards {
                self.scroll = self.scroll.saturating_sub(1);
            }
            if forwards {
                self.scroll = self.scroll.saturating_add(1);
            }
            return None;
        }
        let target = self
            .targets
            .iter()
            .find(|target| Some(&target.key) == self.focus.as_ref())?
            .clone();
        match &target.control {
            Control::Input(value) => {
                let draft = self
                    .drafts
                    .entry(target.key.clone())
                    .or_insert_with(|| value.clone());
                match key.code {
                    KeyCode::Backspace => {
                        draft.pop();
                    }
                    KeyCode::Char(c)
                        if (key.modifiers - KeyModifiers::SHIFT).is_empty()
                            && draft.len() + c.len_utf8() <= MAX_INPUT =>
                    {
                        draft.push(c)
                    }
                    _ => {}
                }
            }
            Control::Select(options, value)
                if matches!(key.code, KeyCode::Left | KeyCode::Right) =>
            {
                let current = self.drafts.get(&target.key).unwrap_or(value);
                let at = options.iter().position(|(v, _)| v == current).unwrap_or(0);
                if !options.is_empty() {
                    let next = if key.code == KeyCode::Left {
                        at.checked_sub(1).unwrap_or(options.len() - 1)
                    } else {
                        (at + 1) % options.len()
                    };
                    self.drafts
                        .insert(target.key.clone(), options[next].0.clone());
                }
                return None;
            }
            _ if key.code == KeyCode::Char('/') => {
                self.searching = true;
                self.search.clear();
                return None;
            }
            _ => {}
        }
        if !actions.contains(&Action::SelectConfirm) {
            return None;
        }
        match &target.control {
            Control::Tree => {
                if !self.expanded.remove(&target.key) {
                    self.expanded.insert(target.key);
                }
                return None;
            }
            Control::Tab(path, id) => {
                self.tabs.insert(path.clone(), id.clone());
                return None;
            }
            _ => {}
        }
        let value = match &target.control {
            Control::Input(initial) | Control::Select(_, initial) => {
                Some(self.drafts.get(&target.key).unwrap_or(initial).clone())
            }
            _ => None,
        };
        let kind = if value.is_some() {
            "change"
        } else if target.row.is_some() {
            "select"
        } else {
            "activate"
        };
        let mut event = json!({"type":kind,"node":target.node});
        if let Some(row) = target.row {
            event["row"] = json!(row);
        }
        if let Some(value) = value {
            event["value"] = json!(value);
        }
        Some(ViewEvent {
            plugin: self.snapshot.plugin.clone(),
            params: json!({"name":self.snapshot.view.id,"revision":self.snapshot.revision,"event":event}),
        })
    }
}
fn fit(value: &str, width: usize, theme: &Theme, token: Tok) -> Line<'static> {
    Line::from(Span::styled(
        text::ellipsize(&text::one_line(value), width, theme.ascii()),
        theme.fg(token),
    ))
}
fn clip(line: Line<'static>, width: usize, theme: &Theme) -> Line<'static> {
    if line.width() <= width
        && line
            .spans
            .iter()
            .all(|s| !s.content.chars().any(char::is_control))
    {
        return line;
    }
    let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    fit(&plain, width, theme, Tok::Text)
}
#[cfg(test)]
mod tests;
