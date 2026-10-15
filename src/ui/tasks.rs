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
mod tests {
    use super::*;
    use serde_json::json;

    fn plain(rows: &Rows) -> Vec<String> {
        rows.iter()
            .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn list(tasks: serde_json::Value) -> TodoList {
        let mut todo = TodoList::new();
        assert!(!todo.call(&json!({"op": "set", "tasks": tasks})).is_error);
        todo
    }

    #[test]
    fn expanded_blocker_keeps_the_full_reason_with_bounded_width() {
        let mut todo = list(json!([{"content":"Implement"}]));
        let reason = "Wrong interpreter. Use Python 3.14 with uv run, preserve the code, then rerun the existing checks. END-OF-REASON";
        todo.call(&json!({"op":"block","reason":reason}));
        let cards = changes(&TodoList::new(), &todo);
        let expanded = card_rows(&cards, 30, &Theme::dark(), true);
        assert!(expanded.iter().all(|line| line.width() <= 30));
        assert!(plain(&expanded).join("").contains("END-OF-REASON"));
        assert!(todo.render().contains(reason));
        assert!(
            !plain(&card_rows(&cards, 30, &Theme::dark(), false))
                .join("")
                .contains("END-OF-REASON")
        );
    }

    #[test]
    fn hidden_when_nothing_is_open_or_too_small() {
        let theme = Theme::dark();
        assert!(rows(&TodoList::new(), 80, &theme, None, 10).is_empty());
        assert!(
            rows(
                &list(json!([{"content": "a", "status": "completed"}])),
                80,
                &theme,
                None,
                10
            )
            .is_empty()
        );
        assert!(
            rows(
                &list(json!([{"content": "a"}])),
                80,
                &theme,
                None,
                MIN_ROWS - 1
            )
            .is_empty()
        );
    }

    #[test]
    fn shows_progress_status_glyphs_and_blockers() {
        let theme = Theme::dark();
        let mut todo = list(
            json!([{"content": "read", "status": "completed"}, {"content": "fix"}, {"content": "ship"}]),
        );
        todo.call(&json!({"op": "block", "index": 3, "reason": "no creds"}));
        let text = plain(&rows(&todo, 60, &theme, Some("⠋"), 10));
        assert_eq!(
            text,
            [
                "TASKS  1/3 done · 1 blocked — /todo details",
                "  ✓ read",
                "  ⠋ fix",
                "  ⚠ ship · no creds"
            ]
        );
    }

    #[test]
    fn long_lists_window_around_the_active_task_and_fit_the_width() {
        let theme = Theme::dark();
        let tasks: Vec<_> = (1..=12).map(|i| json!({"content": format!("task {i} with a fairly long description"), "status": if i < 7 { "completed" } else { "pending" }})).collect();
        let todo = list(json!(tasks));
        let rows = rows(&todo, 30, &theme, None, 6);
        let text = plain(&rows);
        assert_eq!(text.len(), 6, "{text:?}");
        assert!(
            text[1].contains("2 earlier")
                && text[3].contains("task 7")
                && text[5].contains("4 more"),
            "{text:?}"
        );
        assert!(rows.iter().all(|r| text::line_width(r) <= 30));
    }

    #[test]
    fn phases_become_headings() {
        let theme = Theme::dark();
        let todo = list(
            json!([{"content": "read", "phase": "Investigate"}, {"content": "fix", "phase": "Implement"}]),
        );
        let text = plain(&rows(&todo, 60, &theme, None, 10));
        assert_eq!(
            text,
            [
                "TASKS  0/2 done",
                "  Investigate",
                "    ● read",
                "  Implement",
                "    ○ fix"
            ]
        );
    }

    #[test]
    fn a_card_flags_what_the_call_changed_and_strikes_closed_steps() {
        let theme = Theme::dark();
        let before = list(json!([{"content": "read"}, {"content": "fix"}, {"content": "test"}]));
        assert!(
            changes(&TodoList::new(), &before)
                .iter()
                .all(|(_, changed)| !changed),
            "a first plan flags nothing"
        );
        let mut after = before.clone();
        after.call(&json!({"op": "done"}));
        let flagged = changes(&before, &after);
        assert_eq!(
            flagged.iter().map(|(_, c)| *c).collect::<Vec<_>>(),
            [true, true, false],
            "closed step and the step it started"
        );
        let rows = card_rows(&flagged, 60, &theme, false);
        assert_eq!(plain(&rows), ["  ✓ read", "  ● fix", "  ○ test"]);
        let read = rows[0].spans.iter().find(|s| s.content == "read").unwrap();
        assert!(
            read.style
                .add_modifier
                .contains(Modifier::CROSSED_OUT | Modifier::BOLD),
            "the closed step is struck through and emphasized"
        );
        let test = rows[2].spans.iter().find(|s| s.content == "test").unwrap();
        assert!(!test.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn long_cards_collapse_to_the_change_unless_expanded() {
        let theme = Theme::dark();
        let tasks: Vec<_> = (1..=30).map(|i| json!({"content": format!("t{i}"), "status": if i < 20 { "completed" } else { "pending" }})).collect();
        let before = list(json!(tasks));
        let mut after = before.clone();
        after.call(&json!({"op": "done"}));
        let text = plain(&card_rows(&changes(&before, &after), 60, &theme, false));
        assert_eq!(
            text,
            [
                "    … 1 more",
                "  ✓ t19",
                "  ✓ t20",
                "  ● t21",
                "  ○ t22",
                "    … 8 more"
            ]
        );
        assert_eq!(
            card_rows(&changes(&before, &after), 60, &theme, true).len(),
            13
        );
    }
}
