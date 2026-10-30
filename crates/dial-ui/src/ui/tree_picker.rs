//! The in-session tree navigator: every user message and labeled entry of one journal, abandoned
//! branches included. Presentation and key handling only; the controller asks the session for the
//! rendered context of the selected node and moves the leaf through the session's own navigation.
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

use super::{
    Rows, chrome, text,
    theme::{Sym, Theme, Tok},
};
use crate::storage::session::{TreeNode, TreeNodeKind};

pub enum Action {
    None,
    Cancel,
    /// Index into [`Picker::nodes`] of the node to move the conversation to.
    Select(usize),
}

pub struct Picker {
    pub nodes: Vec<TreeNode>,
    query: String,
    /// Indices into `nodes` that match the query, in tree order.
    found: Vec<usize>,
    /// Position in `found`.
    selected: usize,
}

/// The conversation to the selected node, rendered by the transcript the main view uses at the
/// picker's preview width. `messages` counts the stored messages it was built from.
pub struct Preview {
    lines: Rows,
    messages: usize,
}

impl Preview {
    pub fn new(lines: Rows, messages: usize) -> Self {
        Self { lines, messages }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new(), 0)
    }
}

impl Picker {
    /// Opens on the node the conversation currently stands at (or the newest one).
    pub fn new(nodes: Vec<TreeNode>) -> Self {
        let mut picker = Self {
            nodes,
            query: String::new(),
            found: Vec::new(),
            selected: 0,
        };
        picker.refilter();
        picker.selected = picker
            .found
            .iter()
            .position(|&i| picker.nodes[i].current)
            .unwrap_or(picker.found.len().saturating_sub(1));
        picker
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Id of the node under the selection; the controller renders its context from it.
    pub fn selected_id(&self) -> Option<&str> {
        self.found
            .get(self.selected)
            .map(|&i| self.nodes[i].id.as_str())
    }

    fn refilter(&mut self) {
        let terms: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.found = (0..self.nodes.len())
            .filter(|&i| {
                let node = &self.nodes[i];
                let haystack =
                    format!("{} {}", node.text, node.label.as_deref().unwrap_or("")).to_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect();
        self.selected = self.selected.min(self.found.len().saturating_sub(1));
    }

    /// Rows for the list and for the rendered context, as `(list, context)`. `height` includes the
    /// frame. Tall terminals split the body; short ones show the list alone.
    fn layout(height: usize) -> (usize, usize) {
        let inner = height.saturating_sub(2);
        if height >= 12 {
            // Search, context header and hint take three rows.
            let room = inner.saturating_sub(3).max(1);
            let list = (room / 2).max(3).min(room);
            (list, room - list)
        } else {
            // Search and hint take two rows.
            (inner.saturating_sub(2).max(1), 0)
        }
    }

    /// Width of the rendered conversation for a terminal `cols` wide: the picker's text column.
    pub fn preview_width(cols: usize) -> usize {
        cols.saturating_sub(4)
    }

    /// Positions in `found` of the nodes sharing the selected node's parent, in tree order.
    fn siblings_of_selected(&self) -> Vec<usize> {
        let Some(&current) = self.found.get(self.selected) else {
            return Vec::new();
        };
        let parent = &self.nodes[current].parent;
        (0..self.found.len())
            .filter(|&p| self.nodes[self.found[p]].parent == *parent)
            .collect()
    }

    /// The selected node's place among its siblings: `(position, count)`, both one-based.
    fn sibling_slot(&self) -> Option<(usize, usize)> {
        let siblings = self.siblings_of_selected();
        let at = siblings.iter().position(|&p| p == self.selected)?;
        Some((at + 1, siblings.len()))
    }

    /// Moves to the next or previous branch that starts from the same message.
    fn step_sibling(&mut self, forward: bool) {
        let siblings = self.siblings_of_selected();
        let next = if forward {
            siblings.iter().copied().find(|&p| p > self.selected)
        } else {
            siblings.iter().copied().rev().find(|&p| p < self.selected)
        };
        if let Some(position) = next {
            self.selected = position;
        }
    }

    /// Moves to the previous or next user message among the matches: the conversation's turns.
    fn step_turn(&mut self, forward: bool) {
        let turns: Vec<usize> = (0..self.found.len())
            .filter(|&p| matches!(self.nodes[self.found[p]].kind, TreeNodeKind::User))
            .collect();
        let next = if forward {
            turns.iter().copied().find(|&p| p > self.selected)
        } else {
            turns.iter().copied().rev().find(|&p| p < self.selected)
        };
        if let Some(position) = next {
            self.selected = position;
        }
    }

    /// Moves to the parent of the selected node, or to its nearest ancestor that matches the
    /// search when the parent is hidden by it.
    fn select_parent(&mut self) {
        let Some(&current) = self.found.get(self.selected) else {
            return;
        };
        let mut parent = self.nodes[current].parent.clone();
        // At most one step per node, so a cyclic parent chain ends the walk instead of hanging.
        for _ in 0..self.nodes.len() {
            let Some(id) = parent else { return };
            let Some(index) = self.nodes.iter().position(|node| node.id == id) else {
                return;
            };
            if let Some(position) = self.found.iter().position(|&i| i == index) {
                self.selected = position;
                return;
            }
            parent = self.nodes[index].parent.clone();
        }
    }

    pub fn handle(&mut self, event: &Event, height: usize) -> Action {
        let page = Self::layout(height).0;
        let last = self.found.len().saturating_sub(1);
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc if self.query.is_empty() => return Action::Cancel,
                    KeyCode::Esc => {
                        self.query.clear();
                        self.refilter();
                    }
                    KeyCode::Char('c') if ctrl => return Action::Cancel,
                    KeyCode::Char('u') if ctrl => {
                        self.query.clear();
                        self.refilter();
                    }
                    KeyCode::Up if ctrl => self.step_turn(false),
                    KeyCode::Down if ctrl => self.step_turn(true),
                    KeyCode::Left if ctrl => self.select_parent(),
                    KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                    KeyCode::Down => self.selected = (self.selected + 1).min(last),
                    KeyCode::Left => self.step_sibling(false),
                    KeyCode::Right => self.step_sibling(true),
                    KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
                    KeyCode::PageDown => self.selected = (self.selected + page).min(last),
                    KeyCode::Home => self.selected = 0,
                    KeyCode::End => self.selected = last,
                    KeyCode::Enter => {
                        if let Some(&index) = self.found.get(self.selected) {
                            return Action::Select(index);
                        }
                    }
                    KeyCode::Backspace => {
                        self.query.pop();
                        self.refilter();
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.query.push(c);
                        self.refilter();
                    }
                    _ => {}
                }
            }
            Event::Paste(value) => {
                self.query.push_str(&text::one_line(value));
                self.refilter();
            }
            _ => {}
        }
        Action::None
    }

    /// Renders the picker. `preview` is the conversation to the selected node, already rendered
    /// by the caller at [`Picker::preview_width`]; the picker only lays it out.
    pub fn rows(&self, cols: usize, height: usize, theme: &Theme, preview: &Preview) -> Rows {
        if height == 0 || cols == 0 {
            return Vec::new();
        }
        if height < 5 || cols < 12 {
            return vec![Line::from(Span::styled(
                text::ellipsize(
                    "Resize to browse the tree · Esc cancel",
                    cols,
                    theme.ascii(),
                ),
                theme.fg(Tok::Muted),
            ))];
        }
        let width = cols.saturating_sub(4);
        let (list_rows, context_rows) = Self::layout(height);
        let mut body = vec![Line::from(vec![
            Span::styled("Search  ", theme.fg(Tok::Accent)),
            Span::styled(
                text::ellipsize(
                    if self.query.is_empty() {
                        "type to filter messages and labels"
                    } else {
                        &self.query
                    },
                    width.saturating_sub(9),
                    theme.ascii(),
                ),
                theme.fg(if self.query.is_empty() {
                    Tok::Dim
                } else {
                    Tok::Text
                }),
            ),
            Span::styled(" ", theme.fg(Tok::Text).add_modifier(Modifier::REVERSED)),
        ])];
        if self.found.is_empty() {
            let message = if self.nodes.is_empty() {
                "No messages in this session yet."
            } else {
                "No matches. Ctrl+U clears the search."
            };
            body.push(Line::from(Span::styled(
                text::ellipsize(message, width, theme.ascii()),
                theme.fg(Tok::Muted),
            )));
        }
        let start = self.selected.saturating_sub(list_rows.saturating_sub(1));
        for (position, &index) in self.found.iter().enumerate().skip(start).take(list_rows) {
            body.push(self.row(&self.nodes[index], position == self.selected, width, theme));
        }
        if context_rows > 0 {
            body.push(self.context_header(preview.messages, width, theme));
            body.extend(Self::context_lines(&preview.lines, context_rows, theme));
        }
        let arrows = if theme.ascii() { "Up/Down" } else { "↑↓" };
        let sides = if theme.ascii() {
            "Left/Right"
        } else {
            "←→"
        };
        let esc = if self.query.is_empty() {
            "close"
        } else {
            "clear search"
        };
        let position = if self.found.is_empty() {
            String::new()
        } else {
            format!("{}/{}", self.selected + 1, self.found.len())
        };
        let hints = [
            (arrows, "move"),
            (sides, "branch"),
            ("Enter", "go here"),
            ("Esc", esc),
        ];
        let sep = if theme.ascii() { " | " } else { " · " };
        let mut spans = Vec::new();
        if !position.is_empty() {
            spans.push(Span::styled(position, theme.fg(Tok::Muted)));
            spans.push(Span::styled(sep, theme.fg(Tok::Dim)));
        }
        spans.extend(chrome::key_hint_spans(theme, &hints));
        body.push(Line::from(spans));
        let extra = format!(
            "{} node{}",
            self.nodes.len(),
            if self.nodes.len() == 1 { "" } else { "s" }
        );
        chrome::framed_lines(
            theme,
            chrome::Section::Dial,
            "Session tree",
            &extra,
            body,
            cols,
        )
    }

    /// Header of the context pane: how many messages lead to the selection and which branch it is.
    fn context_header(&self, messages: usize, width: usize, theme: &Theme) -> Line<'static> {
        let mut label = if self.selected_id().is_none() {
            "No node selected".to_owned()
        } else {
            format!(
                "Conversation to here · {messages} message{}",
                if messages == 1 { "" } else { "s" }
            )
        };
        if let Some((at, total)) = self.sibling_slot().filter(|&(_, total)| total > 1) {
            let sides = if theme.ascii() {
                "Left/Right"
            } else {
                "←→"
            };
            label.push_str(&format!(" · branch {at} of {total} ({sides} switch)"));
        }
        Line::from(Span::styled(
            text::ellipsize(&label, width, theme.ascii()),
            theme.fg(Tok::Accent),
        ))
    }

    /// The rendered conversation, ending at the selection. When it is taller than `rows`, the
    /// newest lines stay and the oldest collapse into one notice.
    fn context_lines(lines: &Rows, rows: usize, theme: &Theme) -> Rows {
        if rows == 0 {
            return Vec::new();
        }
        if lines.is_empty() {
            return vec![Line::from(Span::styled(
                "Nothing above this point.",
                theme.fg(Tok::Muted),
            ))];
        }
        if lines.len() <= rows {
            return lines.clone();
        }
        let shown = rows - 1;
        let hidden = lines.len() - shown;
        let dots = if theme.ascii() { "..." } else { "…" };
        let mut out = vec![Line::from(Span::styled(
            format!("{dots} {hidden} earlier lines"),
            theme.fg(Tok::Muted),
        ))];
        out.extend(lines[lines.len() - shown..].iter().cloned());
        out
    }

    fn row(&self, node: &TreeNode, selected: bool, width: usize, theme: &Theme) -> Line<'static> {
        let marker = if selected {
            theme.sym(Sym::Pointer)
        } else {
            " "
        };
        // Filled for the branch the model sees, hollow for abandoned ones.
        let dot = match (node.on_active_branch, theme.ascii()) {
            (true, true) => "*",
            (true, false) => "●",
            (false, true) => "o",
            (false, false) => "○",
        };
        let indent = "  ".repeat(node.depth.min(8));
        let who = match node.kind {
            TreeNodeKind::User => "You",
            TreeNodeKind::Assistant => "Dial",
            TreeNodeKind::Summary => "Summary",
            TreeNodeKind::Other => "Entry",
        };
        let label = node
            .label
            .as_deref()
            .map(|l| format!("[{l}] "))
            .unwrap_or_default();
        let here = if node.current { "  <- here" } else { "" };
        let fork = if node.children > 1 {
            format!("  ({} branches)", node.children)
        } else {
            String::new()
        };
        let lead = format!("{marker} {indent}{dot} ");
        let fixed = text::width(&lead)
            + text::width(&label)
            + text::width(who)
            + 2
            + text::width(here)
            + text::width(&fork);
        let room = width.saturating_sub(fixed).max(4);
        let preview = text::ellipsize(&text::one_line(&node.text), room, theme.ascii());
        let fill = if selected {
            theme.bg(Tok::SelectedBg)
        } else {
            Default::default()
        };
        let body_tok = if node.on_active_branch {
            Tok::Text
        } else {
            Tok::Muted
        };
        let tail = text::width(here) + text::width(&fork);
        let used = fixed - tail + text::width(&preview);
        Line::from(vec![
            Span::styled(lead, theme.fg(Tok::Accent).patch(fill)),
            Span::styled(label, theme.fg(Tok::Warning).patch(fill)),
            Span::styled(format!("{who}: "), theme.fg(Tok::Muted).patch(fill)),
            Span::styled(
                preview,
                theme
                    .fg(body_tok)
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    })
                    .patch(fill),
            ),
            Span::styled(fork, theme.fg(Tok::Dim).patch(fill)),
            Span::styled(here, theme.fg(Tok::Accent).patch(fill)),
            Span::styled(" ".repeat(width.saturating_sub(used + tail)), fill),
        ])
        .style(fill)
    }
}

#[cfg(test)]
mod tests;
