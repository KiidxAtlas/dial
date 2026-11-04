//! The work panel: one compact rounded card above the composer. Its first row is the tab strip
//! (`plan`, `evidence`, `approvals`); the rows below it come from the active tab. The plan tab
//! lists as many steps as the card has rows for, always including the focused step, then the
//! focused step's recorded reason or finding. The card is as tall as its content, at most
//! [`MAX_WIDTH`] columns wide, and it yields to the focus summary of [`super::tasks::rows`] below
//! [`MIN_WIDTH`] columns or without the rows for it. Every value comes from the todo list and
//! status the shell already holds; the panel never changes task state.
use super::{
    Rows, chrome,
    markdown::truncate_spans,
    tasks, text,
    theme::{Sym, Theme, Tok},
    welcome,
};
use crate::agent::workflow::todo::{Status, Task, TodoList};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

/// Narrower terminals keep the one-line summary.
pub const MIN_WIDTH: usize = 90;
/// Medium terminals retain a collapsed plan-only card.
pub const COLLAPSED_MIN_WIDTH: usize = 60;
/// The card never grows past this many columns, however wide the terminal is.
pub const MAX_WIDTH: usize = 100;
/// Borders, the tab strip and one step.
pub const MIN_ROWS: usize = 4;
/// Recorded reason or finding lines shown under the steps.
const DETAIL_LINES: usize = 3;

/// The card's view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Plan,
    Evidence,
    Approvals,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Plan, Tab::Evidence, Tab::Approvals];

    pub fn next(self) -> Tab {
        match self {
            Tab::Plan => Tab::Evidence,
            Tab::Evidence => Tab::Approvals,
            Tab::Approvals => Tab::Plan,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tab::Plan => "plan",
            Tab::Evidence => "evidence",
            Tab::Approvals => "approvals",
        }
    }
}

/// Session facts the approvals tab shows, and the key labels the panel advertises.
pub struct Facts<'a> {
    pub approval: &'a str,
    pub sandbox: &'a str,
    /// Label of the key that switches tabs; empty when unbound.
    pub tab_key: &'a str,
    /// Label of the key that cycles the approval mode; empty when unbound.
    pub cycle_key: &'a str,
}

/// The panel rows, at most `max_rows`, each at most `min(width, MAX_WIDTH)` cells wide. Empty
/// when nothing is open or the space is too small, so the caller can fall back to the summary.
pub fn rows(
    todo: &TodoList,
    facts: &Facts<'_>,
    tab: Tab,
    width: usize,
    max_rows: usize,
    theme: &Theme,
    spinner: Option<&str>,
) -> Rows {
    if !todo.has_open() || width < COLLAPSED_MIN_WIDTH || max_rows < MIN_ROWS {
        return Vec::new();
    }
    let card_w = width.min(MAX_WIDTH);
    let content_w = card_w - 4;
    if width < MIN_WIDTH {
        let tasks = todo.tasks();
        let Some(focus) = focus_index(tasks) else {
            return Vec::new();
        };
        let mut body = vec![vec![Span::styled(" plan ", chrome::tab_style(theme, true))]];
        body.extend(
            tasks
                .iter()
                .skip(focus)
                .take(2.min(max_rows - 3))
                .map(|task| tasks::task_spans(task, "", theme, spinner, false)),
        );
        let inner = body.len();
        let (closed, total) = todo.progress();
        return card(
            theme,
            card_w,
            Some(&format!("tasks {closed}/{total}")),
            None,
            body,
            inner,
        );
    }
    // The top and bottom borders and the tab strip take three rows; the detail gets the rest.
    let detail_rows = max_rows - 3;
    let mut body = vec![tab_strip(theme, tab)];
    body.extend(detail(
        todo,
        facts,
        tab,
        content_w,
        detail_rows,
        theme,
        spinner,
    ));
    // Content-sized: the card is as tall as its rows, within the space the caller allows.
    let inner = body.len().min(max_rows - 2);
    let (closed, total) = todo.progress();
    let top = format!("tasks {closed}/{total}");
    let hint = format!("{} next tab", facts.tab_key);
    card(
        theme,
        card_w,
        Some(&top),
        (!facts.tab_key.is_empty()).then_some(hint.as_str()),
        body,
        inner,
    )
}

/// The step the panel follows: in progress, else blocked, else the next pending one.
fn focus_index(tasks: &[Task]) -> Option<usize> {
    [Status::InProgress, Status::Blocked, Status::Pending]
        .into_iter()
        .find_map(|status| tasks.iter().position(|t| t.status == status))
}

fn tab_strip(theme: &Theme, active: Tab) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, tab) in Tab::ALL.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!(" {} ", tab.label()),
            chrome::tab_style(theme, tab == active),
        ));
    }
    spans
}

/// The tab's rows, at most `avail` of them. The plan tab shows the focused step inside the
/// window and gives reason lines the rows left over, so the focus is never clipped.
fn detail(
    todo: &TodoList,
    facts: &Facts<'_>,
    tab: Tab,
    width: usize,
    avail: usize,
    theme: &Theme,
    spinner: Option<&str>,
) -> Vec<Vec<Span<'static>>> {
    let line = |content: String, tok: Tok| vec![Span::styled(content, theme.fg(tok))];
    match tab {
        Tab::Plan => {
            let tasks = todo.tasks();
            let Some(focus) = focus_index(tasks) else {
                return vec![line("nothing in progress".into(), Tok::Muted)];
            };
            let mut reasons = Vec::new();
            for entry in tasks::recorded(&tasks[focus]) {
                for part in entry.lines() {
                    reasons.extend(text::wrap_words(part, width.max(8)));
                }
            }
            // Reasons yield rows to the step window; one row always remains for the focus.
            let reason_rows = reasons.len().min(DETAIL_LINES).min(avail.saturating_sub(1));
            let window = (avail - reason_rows).min(tasks.len());
            // Keep the step before the focus in view when there is room for two steps.
            let lead = usize::from(window >= 2);
            let start = focus.saturating_sub(lead).min(tasks.len() - window);
            let mut out: Vec<Vec<Span<'static>>> = tasks
                .iter()
                .skip(start)
                .take(window)
                .map(|task| tasks::task_spans(task, "", theme, spinner, false))
                .collect();
            out.extend(
                reasons
                    .into_iter()
                    .take(reason_rows)
                    .map(|reason| line(reason, Tok::Text)),
            );
            out
        }
        Tab::Evidence => {
            let mut out = Vec::new();
            for task in todo.tasks().iter().filter(|t| !t.evidence.is_empty()) {
                out.push(vec![
                    Span::styled(
                        format!("{} ", theme.sym(Sym::Check)),
                        theme.fg(Tok::Success),
                    ),
                    Span::styled(text::one_line(&task.content), theme.fg(Tok::Text)),
                ]);
                for reference in &task.evidence {
                    out.push(vec![
                        Span::raw("  "),
                        Span::styled(text::one_line(reference), theme.fg(Tok::Muted)),
                    ]);
                }
            }
            if out.is_empty() {
                out.push(line("no evidence recorded yet".into(), Tok::Muted));
            } else {
                // Recorded references are the agent's observations, not a verification result.
                out.insert(
                    0,
                    line("agent observations, not verified".into(), Tok::Muted),
                );
            }
            out
        }
        Tab::Approvals => {
            let mode = text::one_line(facts.approval.trim());
            if mode.is_empty() {
                return vec![line("approval mode unknown".into(), Tok::Muted)];
            }
            let dot = match mode.as_str() {
                "ask" | "readonly" => Tok::Success,
                "auto" => Tok::Warning,
                _ => Tok::Accent,
            };
            let mut out = vec![vec![
                Span::styled(format!("{} ", theme.sym(Sym::Bullet)), theme.fg(dot)),
                Span::styled(
                    super::status::approval_label(&mode).to_owned(),
                    theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
                ),
            ]];
            let meaning = welcome::approval_meaning(&mode);
            if !meaning.is_empty() {
                out.push(line(meaning.into(), Tok::Muted));
            }
            let sandbox = text::one_line(facts.sandbox.trim());
            if !sandbox.is_empty() {
                out.push(line(format!("sandbox {sandbox}"), Tok::Muted));
            }
            if !facts.cycle_key.is_empty() {
                out.push(line(
                    format!("{} cycles the mode", facts.cycle_key),
                    Tok::Muted,
                ));
            }
            out
        }
    }
}

/// A rounded card of exactly `width` cells and `inner + 2` rows, with optional labels set into
/// the top and bottom edges. Body rows past `inner` are dropped; wide rows are truncated.
fn card(
    theme: &Theme,
    width: usize,
    top: Option<&str>,
    bottom: Option<&str>,
    body: Vec<Vec<Span<'static>>>,
    inner: usize,
) -> Rows {
    let border = theme.fg(Tok::Border);
    let h = theme.sym(Sym::BoxH).to_owned();
    let v = theme.sym(Sym::BoxV).to_owned();
    let ellipsis = theme.sym(Sym::Ellipsis).to_owned();
    let edge = |left: &str, right: &str, label: Option<&str>| -> Line<'static> {
        let label = label
            .map(|label| {
                format!(
                    " {} ",
                    text::clip(&text::one_line(label), width.saturating_sub(7))
                )
            })
            .filter(|label| text::width(label) + 3 <= width);
        match label {
            Some(label) => {
                let fill = width - 3 - text::width(&label);
                Line::from(vec![
                    Span::styled(format!("{left}{h}"), border),
                    Span::styled(label, theme.fg(Tok::Text)),
                    Span::styled(format!("{}{right}", h.repeat(fill)), border),
                ])
            }
            None => Line::from(Span::styled(
                format!("{left}{}{right}", h.repeat(width.saturating_sub(2))),
                border,
            )),
        }
    };
    let content_w = width.saturating_sub(4);
    let mut body = body.into_iter();
    let mut rows = vec![edge(theme.sym(Sym::BoxTl), theme.sym(Sym::BoxTr), top)];
    for _ in 0..inner {
        let spans = truncate_spans(body.next().unwrap_or_default(), content_w, &ellipsis);
        let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
        let mut line = vec![Span::styled(format!("{v} "), border)];
        line.extend(spans);
        line.push(Span::raw(" ".repeat(content_w.saturating_sub(used))));
        line.push(Span::styled(format!(" {v}"), border));
        rows.push(Line::from(line));
    }
    rows.push(edge(theme.sym(Sym::BoxBl), theme.sym(Sym::BoxBr), bottom));
    rows
}

#[cfg(test)]
mod tests;
