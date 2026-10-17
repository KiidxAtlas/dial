//! The live task panel: the agent's todo list pinned above the composer while any task is open, so
//! the plan and the current step stay visible instead of scrolling away with the tool lines.
use super::markdown::truncate_spans;
use super::text;
use super::theme::{Sym, Theme, Tok};
use super::{Rows, chrome};
use crate::agent::todo::{Status, Task, TodoList};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Fewer rows than this cannot show a header, the focus and both overflow markers.
const MIN_ROWS: usize = 4;

/// Panel rows exactly fitted to `width` and at most `max_rows` tall; empty when nothing is open or
/// it cannot fit. `spinner` animates the in-progress task while a turn runs.
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
    let tasks = todo.tasks();
    let phased = tasks.iter().any(|t| t.phase.is_some());
    // (task index, row); phase headings have no task.
    let mut entries: Vec<(Option<usize>, Vec<Span<'static>>)> = Vec::with_capacity(tasks.len() * 2);
    for (i, task) in tasks.iter().enumerate() {
        if phased && (i == 0 || tasks[i - 1].phase != task.phase) {
            let name = task.phase.as_deref().unwrap_or("Other");
            entries.push((
                None,
                vec![
                    Span::raw("  "),
                    Span::styled(name.to_owned(), theme.fg(Tok::Muted)),
                ],
            ));
        }
        entries.push((
            Some(i),
            task_spans(
                task,
                if phased { "    " } else { "  " },
                theme,
                spinner,
                false,
            ),
        ));
    }

    let body = max_rows - 1;
    let total = entries.len();
    let (mut lo, mut hi) = (0, total);
    if total > body {
        // Keep the active task (or the first open one) in view with one row of context above it; an
        // overflow marker takes the place of the first and last rows it replaces.
        let focus = entries
            .iter()
            .position(|(i, _)| i.is_some_and(|i| tasks[i].status == Status::InProgress))
            .or_else(|| {
                entries
                    .iter()
                    .position(|(i, _)| i.is_some_and(|i| tasks[i].status.is_open()))
            })
            .unwrap_or(0);
        let context = if body > 3 { 2 } else { 1 };
        let start = focus.saturating_sub(context).min(total - body);
        (lo, hi) = (start, start + body);
        if lo > 0 {
            lo += 1;
        }
        if hi < total {
            hi -= 1;
        }
    }
    let hidden = |range: &[(Option<usize>, Vec<Span<'static>>)]| {
        range.iter().filter(|(i, _)| i.is_some()).count()
    };
    let (earlier, later) = (hidden(&entries[..lo]), hidden(&entries[hi..]));

    let mark = theme.sym(Sym::Ellipsis).to_owned();
    let fit = |spans: Vec<Span<'static>>| Line::from(truncate_spans(spans, width, &mark));
    let more = |n: usize, word: &str| {
        fit(vec![Span::styled(
            format!("  {mark} {n} {word}"),
            theme.fg(Tok::Dim),
        )])
    };
    let mut out = Vec::with_capacity(max_rows);
    out.push(fit(header(todo, theme)));
    if lo > 0 {
        out.push(more(earlier, "earlier"));
    }
    out.extend(entries.drain(lo..hi).map(|(_, spans)| fit(spans)));
    if hi < total {
        out.push(more(later, "more"));
    }
    out
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
        counts.push_str(&format!(
            " {} {blocked} blocked — /todo details",
            theme.sym(Sym::Dot)
        ));
    }
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
