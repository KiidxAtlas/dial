//! The in-session tree navigator: every user message and labeled entry of one journal, abandoned
//! branches included. Presentation and key handling only; the controller asks the session to move
//! the leaf.
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
    selected: usize,
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

    fn page(&self, height: usize) -> usize {
        // Frame title and trailing blank, search line and hint line.
        height.saturating_sub(4).max(1)
    }

    pub fn handle(&mut self, event: &Event, height: usize) -> Action {
        let page = self.page(height);
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
                    KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                    KeyCode::Down => self.selected = (self.selected + 1).min(last),
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

    pub fn rows(&self, cols: usize, height: usize, theme: &Theme) -> Rows {
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
        let visible = self.page(height);
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
        let start = self.selected.saturating_sub(visible.saturating_sub(1));
        for (position, &index) in self.found.iter().enumerate().skip(start).take(visible) {
            body.push(self.row(&self.nodes[index], position == self.selected, width, theme));
        }
        let arrows = if theme.ascii() { "Up/Down" } else { "↑↓" };
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
        let hints = [(arrows, "move"), ("Enter", "go here"), ("Esc", esc)];
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
