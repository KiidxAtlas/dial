//! Work is presented in two layers: a small live focus summary and a searchable full plan.
//! The plan inspector exposes recorded reasons, findings and evidence without changing task state.
use super::markdown::truncate_spans;
use super::text;
use super::theme::{Sym, Theme, Tok};
use super::{Rows, chrome};
use crate::agent::workflow::todo::{Status, Task, TodoList};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Even a two-row budget can show the current step and how to inspect the plan.
const MIN_ROWS: usize = 2;

/// A focused live summary: current step, blockers, then the next step. Closed work belongs in the
/// inspector, not in the scarce space beside an actively edited draft.
pub fn rows(
    todo: &TodoList,
    width: usize,
    theme: &Theme,
    spinner: Option<&str>,
    max_rows: usize,
) -> Rows {
    if !todo.has_open() || width < 12 || max_rows < MIN_ROWS {
        return Vec::new();
    }
    let mark = theme.sym(Sym::Ellipsis);
    let fit = |spans| Line::from(truncate_spans(spans, width, mark));
    let mut out = vec![fit(header(todo, theme))];
    let tasks = todo.tasks();
    let focus = tasks.iter().find(|t| t.status == Status::InProgress);
    let blocked = tasks.iter().filter(|t| t.status == Status::Blocked);
    let next = tasks.iter().find(|t| t.status == Status::Pending);
    for (label, task) in focus
        .map(|task| ("Now", task))
        .into_iter()
        .chain(blocked.map(|task| ("Blocked", task)))
        .chain(next.map(|task| ("Next", task)))
        .take(max_rows.saturating_sub(1).min(3))
    {
        let mut spans = vec![Span::styled(format!("  {label}: "), theme.fg(Tok::Muted))];
        spans.extend(task_spans(task, "", theme, spinner, false));
        out.push(fit(spans));
    }
    out
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Pending => "Next",
        Status::InProgress => "Working",
        Status::Completed => "Done",
        Status::Abandoned => "Dropped",
        Status::Blocked => "Blocked",
    }
}

/// Recorded detail, not a fresh verification claim. Keep the full reason and evidence available
/// even when the compact summary and the picker's preview must truncate them.
pub fn step_detail(task: &Task) -> String {
    let mut lines = vec![
        task.content.clone(),
        format!("Status: {}", status_label(task.status)),
    ];
    if let Some(phase) = &task.phase {
        lines.push(format!("Phase: {phase}"));
    }
    if task.investigation {
        lines.push("Investigation: a question to resolve before implementation".into());
    }
    if let Some(reason) = &task.note {
        lines.push(format!("Reason: {reason}"));
    }
    if let Some(finding) = &task.finding {
        lines.push(format!("Recorded finding: {finding}"));
    }
    if !task.evidence.is_empty() {
        lines.push(format!("Recorded evidence:\n{}", task.evidence.join("\n")));
    }
    lines.join("\n\n")
}

/// Stable task numbers survive priority ordering and filtering. Selecting a row opens details;
/// browsing never starts, completes, unblocks or otherwise mutates the agent's plan.
pub fn plan_picker(todo: &TodoList) -> Option<super::overlay::palette::ChoiceSpec> {
    use super::overlay::palette::{ChoiceItem, ChoiceSpec};
    if todo.is_empty() {
        return None;
    }
    let mut ordered: Vec<_> = todo.tasks().iter().enumerate().collect();
    ordered.sort_by_key(|(index, task)| {
        let priority = match task.status {
            Status::InProgress => 0,
            Status::Blocked => 1,
            Status::Pending => 2,
            Status::Completed | Status::Abandoned => 3,
        };
        (priority, *index)
    });
    Some(ChoiceSpec {
        title: "Work plan - select a step for details".into(),
        command: "todo".into(),
        verb: "show",
        remove: None,
        items: ordered
            .into_iter()
            .map(|(index, task)| ChoiceItem {
                value: (index + 1).to_string(),
                label: format!(
                    "{}. {}: {}",
                    index + 1,
                    status_label(task.status),
                    task.content
                ),
                detail: step_detail(task),
                current: task.status == Status::InProgress,
            })
            .collect(),
    })
}

fn header(todo: &TodoList, theme: &Theme) -> Vec<Span<'static>> {
    let (closed, total) = todo.progress();
    let blocked = todo
        .tasks()
        .iter()
        .filter(|t| t.status == Status::Blocked)
        .count();
    let mut counts = format!("  {closed}/{total} done");
    if blocked > 0 {
        counts.push_str(&format!(" {} {blocked} blocked", theme.sym(Sym::Dot)));
    }
    counts.push_str(&format!(" {} /todo inspect plan", theme.sym(Sym::Dot)));
    vec![
        chrome::label(theme, "tasks"),
        Span::styled(counts, theme.fg(Tok::Dim)),
    ]
}

/// One task row. `changed` marks a row the latest todo call moved; closed steps are struck through.
fn task_spans(
    task: &Task,
    indent: &str,
    theme: &Theme,
    spinner: Option<&str>,
    changed: bool,
) -> Vec<Span<'static>> {
    let ascii = theme.ascii();
    let glyph = |sym: Sym, marker: &str| {
        if ascii {
            marker.to_owned()
        } else {
            theme.sym(sym).to_owned()
        }
    };
    let struck = |tok: Tok| theme.fg(tok).add_modifier(Modifier::CROSSED_OUT);
    let (glyph, glyph_style, text_style) = match task.status {
        Status::Completed => (
            glyph(Sym::Check, "[x]"),
            theme.fg(Tok::Success),
            struck(if changed { Tok::Success } else { Tok::Dim }),
        ),
        Status::Abandoned => (
            glyph(Sym::Cross2, "[-]"),
            theme.fg(Tok::Dim),
            struck(Tok::Dim),
        ),
        Status::Blocked => (
            glyph(Sym::Warn, "[!]"),
            theme.fg(Tok::Warning),
            theme.fg(Tok::Warning),
        ),
        Status::Pending => (
            glyph(Sym::RadioOff, "[ ]"),
            theme.fg(Tok::Muted),
            theme.fg(Tok::Text),
        ),
        Status::InProgress => {
            let glyph = match (spinner, ascii) {
                (_, true) => "[~]".to_owned(),
                (Some(frame), false) => frame.to_owned(),
                (None, false) => theme.sym(Sym::Working).to_owned(),
            };
            (
                glyph,
                theme.fg(Tok::Accent),
                theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
            )
        }
    };
    let emphasis = |style: ratatui::style::Style| {
        if changed {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    };
    let mut spans = vec![
        Span::raw(indent.to_owned()),
        Span::styled(glyph, emphasis(glyph_style)),
        Span::raw(" "),
        Span::styled(text::one_line(&task.content), emphasis(text_style)),
    ];
    if let (Some(note), Status::Blocked | Status::Abandoned) = (&task.note, task.status) {
        spans.push(Span::styled(
            format!(" {} {}", theme.sym(Sym::Dot), text::one_line(note)),
            theme.fg(Tok::Dim),
        ));
    }
    spans
}

/// Lists longer than this collapse to the rows around what changed (Ctrl-O shows them all).
const CARD_COLLAPSE_OVER: usize = 12;

/// A todo update in the transcript: the list after the call, rows the call changed emphasized and
/// closed steps struck through, so every update shows what moved instead of a one-line count.
pub fn card_rows(tasks: &[(Task, bool)], width: usize, theme: &Theme, expanded: bool) -> Rows {
    let keep: Vec<bool> = if expanded || tasks.len() <= CARD_COLLAPSE_OVER {
        vec![true; tasks.len()]
    } else {
        let focus: Vec<usize> = tasks
            .iter()
            .enumerate()
            .filter(|(_, (t, changed))| *changed || t.status == Status::InProgress)
            .map(|(i, _)| i)
            .collect();
        (0..tasks.len())
            .map(|i| {
                if focus.is_empty() {
                    i < CARD_COLLAPSE_OVER
                } else {
                    focus.iter().any(|f| i.abs_diff(*f) <= 1)
                }
            })
            .collect()
    };
    let mark = theme.sym(Sym::Ellipsis).to_owned();
    let fit = |spans: Vec<Span<'static>>| Line::from(truncate_spans(spans, width, &mark));
    let more = |n: usize| {
        fit(vec![Span::styled(
            format!("    {mark} {n} more"),
            theme.fg(Tok::Dim),
        )])
    };
    let phased = tasks.iter().any(|(t, _)| t.phase.is_some());
    let (mut out, mut hidden, mut phase): (Rows, usize, Option<&str>) = (Vec::new(), 0, None);
    for ((task, changed), keep) in tasks.iter().zip(keep) {
        if !keep {
            hidden += 1;
            continue;
        }
        if hidden > 0 {
            out.push(more(hidden));
            hidden = 0;
        }
        if phased && (out.is_empty() || task.phase.as_deref() != phase) {
            phase = task.phase.as_deref();
            out.push(fit(vec![
                Span::raw("  "),
                chrome::label(theme, phase.unwrap_or("Other")),
            ]));
        }
        let spans = task_spans(
            task,
            if phased { "    " } else { "  " },
            theme,
            None,
            *changed,
        );
        if expanded {
            // Ctrl-O reveals the entire reason, wrapping rather than silently clipping.
            out.extend(text::wrap(vec![Line::from(spans)], width.max(1)));
        } else {
            out.push(fit(spans));
        }
    }
    if hidden > 0 {
        out.push(more(hidden));
    }
    out
}

/// The new list with each task flagged when the call changed it: a status or note change, a new
/// task, or one that replaced another. Matching is by content, so pruning closed tasks does not
/// flag every shifted row. A first plan flags nothing.
pub fn changes(old: &TodoList, new: &TodoList) -> Vec<(Task, bool)> {
    new.tasks()
        .iter()
        .map(|task| {
            let changed = match old.tasks().iter().find(|t| t.content == task.content) {
                Some(before) => before.status != task.status || before.note != task.note,
                None => !old.is_empty(),
            };
            (task.clone(), changed)
        })
        .collect()
}

#[cfg(test)]
mod tests;
