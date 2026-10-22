//! Tool cards lay out the portable presentation supplied by each tool definition.
//! The shared execution phase owns settlement; expanded cards retain full evidence.
use super::{
    Rows, markdown,
    markdown::truncate_spans,
    text,
    theme::{Sym, Theme, Tok},
    transcript::{TranscriptCtx, ellipsis, fitted},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

pub use crate::runtime::Phase as ToolState;

/// One tool call as the transcript tracks it.
#[derive(Clone)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub presentation: crate::runtime::presentation::Presentation,
    /// The call's arguments as JSON text (may be empty for orphan results).
    pub input: String,
    pub state: ToolState,
    pub output: String,
    /// Latest in-place status of a pending tool (a `ToolEvent` of kind `progress`, e.g. a `gh run
    /// watch` snapshot). Replaced on every event, never accumulated; shown instead of `output`.
    pub progress: Option<String>,
    pub elapsed: Option<f64>,
    /// For a `todo` call: the list after it, each task flagged when this call changed it.
    pub todo: Option<Vec<(crate::agent::workflow::todo::Task, bool)>>,
    /// For a `task` call: the subagents it spawned, as last observed.
    pub agents: Vec<AgentRow>,
}

/// One spawned subagent as its task card shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentRow {
    pub name: String,
    /// Agent type (`scout`, `reviewer`, …); empty or `task` for the default worker.
    pub agent: String,
    pub model: String,
    /// What it was asked to do, one line.
    pub task: String,
    pub state: ToolState,
    /// Tool running right now, while running.
    pub current_tool: Option<String>,
    pub tools: u64,
    pub steps: u64,
    /// Wall time: final once settled, elapsed so far while running.
    pub elapsed_ms: Option<u64>,
    /// First line of the result once settled, or the latest progress text while running.
    pub preview: String,
}

// ---- tools ---------------------------------------------------------------------------------

/// The icon in a work row's gutter. A settled call is a quiet dot so the answer around it leads;
/// only running, failed and unfinished calls draw the eye.
fn phase_glyph(state: ToolState, ctx: &TranscriptCtx) -> (String, Style) {
    let theme = ctx.theme;
    match state {
        // A running call breathes through the brand colors, matching the status-line spinner.
        ToolState::Running => (
            theme.spinner(ctx.tick).to_owned(),
            theme
                .fg(Tok::Accent)
                .patch(crate::ui::fx::pulse(theme, ctx.tick)),
        ),
        ToolState::Completed => (theme.sym(Sym::Dot).to_owned(), theme.fg(Tok::Dim)),
        ToolState::Failed => (theme.sym(Sym::Cross2).to_owned(), theme.fg(Tok::Error)),
        ToolState::Interrupted => (
            if theme.ascii() { "-" } else { "⊘" }.to_owned(),
            theme.fg(Tok::Muted),
        ),
        ToolState::Incomplete | ToolState::Blocked | ToolState::NeedsInput => {
            (theme.sym(Sym::Warn).to_owned(), theme.fg(Tok::Warning))
        }
    }
}

/// Calls that load context (skill instructions, tool schemas) rather than act on the project.
/// They settle into the quiet context lane instead of reading like work.
fn is_context(t: &Tool) -> bool {
    matches!(t.name.as_str(), "skill" | "tool_load" | "tool_search")
}

fn tool_glyph(t: &Tool, ctx: &TranscriptCtx) -> (String, Style) {
    if is_context(t) && t.state == ToolState::Completed {
        let glyph = if ctx.theme.ascii() { "*" } else { "◆" };
        return (glyph.to_owned(), ctx.theme.fg(Tok::SectionLearn));
    }
    phase_glyph(t.state, ctx)
}

pub(super) fn tool_rows(t: &Tool, w: u16, ctx: &TranscriptCtx) -> Rows {
    if w == 0
        || (!ctx.ui.show_tools && matches!(t.state, ToolState::Running | ToolState::Completed))
    {
        return Vec::new();
    }
    if let Some((server, action)) = t
        .name
        .strip_prefix("mcp__")
        .and_then(|name| name.split_once("__"))
        && !server.is_empty()
        && !action.is_empty()
    {
        return mcp_card(t, server, action, w as usize, ctx);
    }
    if t.presentation.body == crate::runtime::presentation::BodyStyle::Plan
        && t.todo.is_some()
        && t.state == ToolState::Completed
    {
        todo_card(t, w as usize, ctx)
    } else {
        card(t, w as usize, ctx)
    }
}

/// MCP is a service boundary, not a status: keep the status glyph and evidence inside an
/// identifiable service frame even after the call settles. Native calls remain quiet rows.
fn mcp_card(t: &Tool, server: &str, action: &str, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let fill = theme.bg(Tok::PanelBg);
    let border = theme.fg(Tok::SectionConnect).patch(fill);
    let (tl, tr, bl, br, h, v) = if theme.ascii() {
        ("+", "+", "+", "+", "-", "|")
    } else {
        ("╭", "╮", "╰", "╯", "─", "│")
    };
    // At tiny widths retain the identity, without overflowing or subtracting past zero.
    if w < 6 {
        return vec![fitted(
            vec![Span::styled(format!("MCP {server}"), border)],
            w,
            theme,
        )];
    }
    let inner = w - 4;
    let separator = if theme.ascii() { ":" } else { "·" };
    let title = text::clip(
        &format!(" MCP {separator} {} ", text::one_line(server)),
        w - 2,
    );
    let mut rows = vec![Line::from(Span::styled(
        format!("{tl}{title}{}{tr}", h.repeat(w - 2 - text::width(&title))),
        border,
    ))];
    // The server is already in the frame title, so the inner row names only the action.
    for mut row in card_with(t, inner, ctx, text::one_line(action)) {
        let used = text::line_width(&row);
        let mut spans = vec![Span::styled(format!("{v} "), border)];
        for span in &mut row.spans {
            span.style = fill.patch(row.style).patch(span.style);
        }
        spans.extend(row.spans);
        spans.push(Span::styled(" ".repeat(inner.saturating_sub(used)), fill));
        spans.push(Span::styled(format!(" {v}"), border));
        rows.push(Line::from(spans));
    }
    rows.push(Line::from(Span::styled(
        format!("{bl}{}{br}", h.repeat(w - 2)),
        border,
    )));
    rows
}

/// Width of the verb column, so targets line up down the transcript.
const VERB_W: usize = 7;
/// Cells of the icon gutter: the glyph, then the content column every transcript block shares.
pub(super) const GUTTER: usize = 3;
/// Cells of target a header keeps before it gives up optional facts (timing, a redundant status).
const TARGET_MIN: usize = 16;

/// The glyph padded to the gutter.
fn gutter_span(glyph: String, style: Style) -> Vec<Span<'static>> {
    let pad = GUTTER.saturating_sub(text::width(&glyph)).max(1);
    vec![Span::styled(glyph, style), Span::raw(" ".repeat(pad))]
}

/// The human verb a call is listed under, and whether it is an automatic check.
fn verb(t: &Tool) -> String {
    text::one_line(&t.presentation.label)
}

fn target(t: &Tool) -> String {
    text::one_line(&t.presentation.target)
}

/// The `content` a `write` call creates, for a preview when the result carries no diff.
fn written_content(t: &Tool) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&t.input)
        .ok()?
        .get("content")?
        .as_str()
        .map(str::to_owned)
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// One fact at the header's right edge. `drop` ranks how readily it gives way to the target when
/// the row is narrow: 0 never, higher first.
struct Fact {
    text: String,
    drop: u8,
}

/// The `+a −b` totals an edit's change preview reports, when it carries them.
fn change_totals(t: &Tool, ascii: bool) -> Option<String> {
    if t.state == ToolState::Failed {
        return None;
    }
    let totals = t.output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("change preview (")?
            .strip_suffix(" lines)")
            .map(str::to_owned)
    })?;
    let (added, removed) = totals.split_once(' ')?;
    if !added.starts_with('+') || !removed.starts_with('-') {
        return None;
    }
    let minus = if ascii { "-" } else { "−" };
    Some(format!("{added} {minus}{}", &removed[1..]))
}

/// Facts shown at the right edge of the header: size of the change or result, run time, exit code.
fn facts(t: &Tool, theme: &Theme) -> Vec<Fact> {
    let keep = |text: String| Fact { text, drop: 0 };
    let mut out = Vec::new();
    if t.presentation.body == crate::runtime::presentation::BodyStyle::Workers
        && !t.agents.is_empty()
    {
        let running = t
            .agents
            .iter()
            .filter(|a| a.state == ToolState::Running)
            .count();
        let failed = t
            .agents
            .iter()
            .filter(|a| a.state == ToolState::Failed)
            .count();
        if running > 0 {
            out.push(keep(format!("{running} running")));
        }
        if failed > 0 {
            out.push(keep(format!("{failed} failed")));
        }
    }
    if let Some(totals) = change_totals(t, theme.ascii()) {
        out.push(keep(totals));
    }
    // Tool-owned facts may join parts with `·`; the ASCII preset keeps its own separator.
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    out.extend(
        t.presentation
            .facts
            .iter()
            .map(|fact| keep(text::one_line(fact).replace(" · ", &dot))),
    );
    let exit_code = out.iter().any(|f| f.text.starts_with("exit "));
    if !matches!(t.state, ToolState::Running | ToolState::Completed)
        // An exit code already says the command failed; the gutter's ✗ says it again.
        && !(t.state == ToolState::Failed && exit_code)
    {
        // Only failure has an unmistakable glyph of its own (✗), so only its word can yield to
        // the target; interrupted, blocked and incomplete always say what happened.
        out.push(Fact {
            text: t.state.label().into(),
            drop: u8::from(t.state == ToolState::Failed),
        });
    }
    if let Some(secs) = t
        .elapsed
        .filter(|s| *s >= 0.05 && t.state != ToolState::Running)
    {
        out.push(Fact {
            text: elapsed_label(secs),
            drop: 2,
        });
    }
    out
}

/// Paths are identified by their end, so they lose their start; prose loses its end.
fn fit_target(target: &str, room: usize, ascii: bool) -> String {
    if room == 0 {
        return String::new();
    }
    let path_like = target.contains('/') && !target.contains(char::is_whitespace);
    if path_like {
        text::ellipsize_start(target, room, ascii)
    } else {
        text::ellipsize(target, room, ascii)
    }
}

/// Whether two spellings name the same file (one may be relative to the other).
fn same_path(a: &str, b: &str) -> bool {
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

/// One line of plain output word-wrapped to `max` cells with its indentation kept: continuation
/// rows hang under the line's own text, so compiler carets and nested output stay aligned.
fn wrap_indented(line: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let body = line.trim_start_matches(' ');
    if body.is_empty() {
        return vec![String::new()];
    }
    let indent = (line.len() - body.len()).min(max / 2);
    let pad = " ".repeat(indent);
    text::wrap_words(body, max - indent)
        .into_iter()
        .map(|row| format!("{pad}{row}"))
        .collect()
}

/// Rows previewing what a `write` created: numbered `+` lines from the call's own content.
fn write_preview(t: &Tool, inner: usize, ctx: &TranscriptCtx) -> Option<(Rows, usize)> {
    let content = written_content(t)?;
    let theme = ctx.theme;
    let lines: Vec<&str> = content.lines().collect();
    let gutter_w = lines.len().max(1).to_string().len();
    let limit = if ctx.tools_expanded {
        usize::MAX
    } else {
        ctx.ui.tool_output_lines.max(1)
    };
    let rows: Rows = lines
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, line)| {
            Line::from(vec![
                Span::styled(format!("{:>gutter_w$} ", i + 1), theme.fg(Tok::Dim)),
                Span::styled(
                    text::ellipsize(
                        &format!("+ {line}"),
                        inner.saturating_sub(gutter_w + 1).max(1),
                        theme.ascii(),
                    ),
                    theme.fg(Tok::DiffAdded),
                ),
            ])
        })
        .collect();
    let hidden = lines.len().saturating_sub(rows.len());
    Some((rows, hidden))
}

/// One tool call: a header row (status glyph in the gutter, bold verb, muted target, facts at the
/// right edge), then, when it says something, the body hanging off a `└` under the verb: the diff
/// for edits, the new lines for writes, output for commands, the first error lines for failures.
fn card(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Rows {
    card_with(t, w, ctx, verb(t))
}

/// A card whose verb column reads `verb_text` (an MCP call's service-local action).
fn card_with(t: &Tool, w: usize, ctx: &TranscriptCtx, verb_text: String) -> Rows {
    let theme = ctx.theme;
    let mark = ellipsis(theme);
    let (glyph, glyph_style) = tool_glyph(t, ctx);
    let verb_style = if t.state == ToolState::Failed {
        theme.fg(Tok::Error).add_modifier(Modifier::BOLD)
    } else if is_context(t) {
        theme.fg(Tok::Muted)
    } else {
        theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD)
    };
    let full_target = target(t);
    let sep = format!(" {} ", theme.sym(Sym::Dot));
    let lead = gutter_span(glyph, glyph_style);
    let lead_w: usize = lead.iter().map(|s| text::width(&s.content)).sum();
    let left_fixed = lead_w + VERB_W.max(text::width(&verb_text)) + 1;
    // Narrow rows give up timing, then a status the gutter glyph already shows, before the target.
    let mut facts = facts(t, theme);
    let want = text::width(&full_target).min(TARGET_MIN);
    let (facts_text, room) = loop {
        let joined = facts
            .iter()
            .map(|f| f.text.as_str())
            .collect::<Vec<_>>()
            .join(&sep);
        let right_w = text::width(&joined);
        let room = w.saturating_sub(left_fixed + right_w + if right_w > 0 { 2 } else { 0 });
        let droppable = facts
            .iter()
            .enumerate()
            .filter(|(_, f)| f.drop > 0)
            .max_by_key(|(i, f)| (f.drop, *i))
            .map(|(i, _)| i);
        match droppable {
            Some(i) if room < want => {
                facts.remove(i);
            }
            _ => break (joined, room),
        }
    };
    let right_w = text::width(&facts_text);
    let target_text = fit_target(&full_target, room, theme.ascii());
    let target_clipped = full_target != target_text;
    let mut head = lead;
    head.push(Span::styled(
        format!("{verb_text:<width$}", width = VERB_W),
        verb_style,
    ));
    head.push(Span::raw(" "));
    head.push(Span::styled(target_text, theme.fg(Tok::Muted)));
    if right_w > 0 {
        let used: usize = head.iter().map(|s| text::width(&s.content)).sum();
        head.push(Span::raw(
            " ".repeat(w.saturating_sub(used + right_w).max(1)),
        ));
        let style = if t.state == ToolState::Failed {
            theme.fg(Tok::Error)
        } else {
            theme.fg(Tok::Dim)
        };
        head.push(Span::styled(facts_text, style));
    }
    let mut rows = vec![fitted(head, w, theme)];
    if ctx.tools_expanded && target_clipped {
        for line in text::wrap_words(&full_target, w.saturating_sub(GUTTER).max(1)) {
            rows.push(fitted(
                vec![
                    Span::raw(" ".repeat(GUTTER)),
                    Span::styled(line, theme.fg(Tok::Muted)),
                ],
                w,
                theme,
            ));
        }
    }

    // The body: nothing for reads, searches and passing checks; otherwise what changed or printed.
    let quiet = !ctx.tools_expanded
        && t.state == ToolState::Completed
        && t.presentation.body == crate::runtime::presentation::BodyStyle::Quiet;
    if quiet {
        return rows;
    }
    if t.presentation.body == crate::runtime::presentation::BodyStyle::Workers
        && !t.agents.is_empty()
    {
        rows.extend(agent_tree(&t.agents, w, ctx));
        return rows;
    }
    let inner = w.saturating_sub(GUTTER + 2).max(1);
    let (body, hidden) = if t.state == ToolState::Failed {
        output_rows(t, inner, ctx, Some(COMPACT_ERROR_ROWS))
    } else if let Some(diff) = diff_rows(t, inner, ctx) {
        diff
    } else if t.presentation.body == crate::runtime::presentation::BodyStyle::Write {
        write_preview(t, inner, ctx).unwrap_or_default()
    } else {
        output_rows(t, inner, ctx, None)
    };
    let hook_style = theme.fg(if t.state == ToolState::Failed {
        Tok::Error
    } else {
        Tok::Dim
    });
    let hook = if theme.ascii() { "`" } else { "└" };
    let mut first = true;
    let mut rail = || {
        let glyph = if std::mem::take(&mut first) {
            hook
        } else {
            " "
        };
        Span::styled(format!("{}{glyph} ", " ".repeat(GUTTER)), hook_style)
    };
    for line in body {
        let mut spans = vec![rail()];
        spans.extend(truncate_spans(line.spans, inner, &mark));
        rows.push(fitted(spans, w, theme));
    }
    if hidden > 0 {
        rows.push(fitted(
            vec![rail(), hidden_hint(hidden, ctx.expand_key, theme)],
            w,
            theme,
        ));
    }
    rows
}

fn duration_label(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0 => format!("{:.1}s", ms as f64 / 1000.0),
        1..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// The agents a `task` call spawned, as a tree: per agent a status row (glyph, name, type, model,
/// assignment, tool count and time at the right), then one hook row under it with the tool it is
/// running now, or, once settled, the first line of what it reported.
fn agent_tree(agents: &[AgentRow], w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let mark = ellipsis(theme);
    let ascii = theme.ascii();
    let (branch, last, pipe, hook) = if ascii {
        ("|-", "`-", "| ", "`")
    } else {
        ("├─", "└─", "│ ", "·")
    };
    let rail = theme.fg(Tok::Border);
    let mut rows = Vec::new();
    for (i, a) in agents.iter().enumerate() {
        let is_last = i + 1 == agents.len();
        let (glyph, glyph_style) = phase_glyph(a.state, ctx);
        let mut facts = Vec::new();
        if !matches!(a.state, ToolState::Running | ToolState::Completed) {
            facts.push(a.state.label().into());
        }
        if a.tools > 0 {
            facts.push(plural(a.tools as usize, "tool"));
        } else if a.state == ToolState::Running && a.steps > 0 {
            facts.push(format!("step {}", a.steps));
        }
        if let Some(ms) = a.elapsed_ms {
            facts.push(duration_label(ms));
        }
        let facts = facts.join(&format!(" {} ", theme.sym(Sym::Dot)));
        let mut left = vec![
            Span::styled(format!("   {} ", if is_last { last } else { branch }), rail),
            Span::styled(glyph, glyph_style),
            Span::raw(" "),
            Span::styled(
                text::one_line(&a.name),
                if a.state == ToolState::Failed {
                    theme.fg(Tok::Error).add_modifier(Modifier::BOLD)
                } else {
                    theme.fg(Tok::Text).add_modifier(Modifier::BOLD)
                },
            ),
        ];
        // The kind is only worth a badge when the name does not already say it (`scout#1`).
        let kind = a.agent.trim();
        if !kind.is_empty()
            && kind != "task"
            && kind != "worker"
            && !a.name.trim().starts_with(kind)
        {
            left.push(Span::styled(format!(" [{kind}]"), theme.fg(Tok::Dim)));
        }
        if ctx.tools_expanded && !a.model.is_empty() {
            left.push(Span::styled(
                format!(" {} {}", theme.sym(Sym::Dot), text::one_line(&a.model)),
                theme.fg(Tok::Muted),
            ));
        }
        // What each worker is doing is the point of the card, so the task is always shown.
        if !a.task.is_empty() {
            left.push(Span::styled("  ", Style::default()));
            left.push(Span::styled(text::one_line(&a.task), theme.fg(Tok::Muted)));
        }
        let right_w = text::width(&facts);
        let left = truncate_spans(left, w.saturating_sub(right_w + 2).max(1), &mark);
        let left_w: usize = left.iter().map(|s| text::width(&s.content)).sum();
        let mut row = left;
        if right_w > 0 {
            row.push(Span::raw(
                " ".repeat(w.saturating_sub(left_w + right_w).max(1)),
            ));
            row.push(Span::styled(facts, theme.fg(Tok::Muted)));
        }
        rows.push(fitted(row, w, theme));

        let detail = match (&a.current_tool, a.state) {
            (Some(tool), ToolState::Running) => Some((text::one_line(tool), theme.fg(Tok::Muted))),
            // Between tool calls a running worker is waiting on its model.
            (None, ToolState::Running) if a.preview.is_empty() => {
                Some(("thinking".to_owned(), theme.fg(Tok::Dim)))
            }
            (_, ToolState::Failed) if !a.preview.is_empty() => {
                Some((a.preview.clone(), theme.fg(Tok::Error)))
            }
            (_, _) if !a.preview.is_empty() => Some((a.preview.clone(), theme.fg(Tok::Dim))),
            _ => None,
        };
        if let Some((detail, style)) = detail {
            let indent = format!("   {}  ", if is_last { "  " } else { pipe });
            let lead = format!("{indent}{hook} ");
            let room = w.saturating_sub(text::width(&lead)).max(1);
            let detail = text::sanitize(&detail);
            let lines = if ctx.tools_expanded {
                text::wrap_words(&detail, room)
            } else {
                vec![text::ellipsize(&text::one_line(&detail), room, ascii)]
            };
            // The hook marks where the report starts; wrapped rows hang under its text.
            let hang = format!("{indent}{} ", " ".repeat(text::width(hook)));
            for (n, line) in lines.into_iter().enumerate() {
                let rail_text = if n == 0 { lead.clone() } else { hang.clone() };
                rows.push(fitted(
                    vec![Span::styled(rail_text, rail), Span::styled(line, style)],
                    w,
                    theme,
                ));
            }
        }
    }
    rows
}

/// A todo call: one status line, then the list as it now stands with this call's changes emphasized.
fn todo_card(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let tasks = t.todo.as_deref().unwrap_or_default();
    let (glyph, glyph_style) = tool_glyph(t, ctx);
    let closed = tasks
        .iter()
        .filter(|(task, _)| !task.status.is_open())
        .count();
    // Same columns as every other card: status glyph, verb, then what it is about.
    let verb_style = theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD);
    let mut spans = gutter_span(glyph, glyph_style);
    let summary = if tasks.is_empty() {
        "no tasks".to_owned()
    } else if closed == tasks.len() {
        format!("all {} done", tasks.len())
    } else {
        format!("{closed}/{} done", tasks.len())
    };
    spans.extend([
        Span::styled(format!("{:<width$}", "Plan", width = VERB_W), verb_style),
        Span::raw(" "),
        Span::styled(summary, theme.fg(Tok::Muted)),
    ]);
    let mut rows = vec![fitted(spans, w, theme)];
    // The list's own two-cell indent plus one puts each task glyph under the verb.
    let shift = GUTTER.saturating_sub(2);
    for row in super::tasks::card_rows(tasks, w.saturating_sub(shift), theme, ctx.tools_expanded) {
        let mut spans = vec![Span::raw(" ".repeat(shift))];
        spans.extend(row.spans);
        rows.push(Line::from(spans));
    }
    rows
}

/// The changes an `edit`/`write`/`ast_edit` result reports, as a numbered, coloured diff: per file its
/// path and summary, then each hunk. The model-facing parts (anchored preview rows, the removed-lines
/// report) are left out. `None` when the result carries no change preview.
fn diff_rows(t: &Tool, inner: usize, ctx: &TranscriptCtx) -> Option<(Rows, usize)> {
    let output = text::sanitize(&t.output);
    if !output.contains("change preview (") {
        return None;
    }
    let theme = ctx.theme;
    enum Row<'a> {
        File(&'a str),
        Note(&'a str),
        Gap,
        Line(char, Option<usize>, &'a str),
    }
    let mut items = Vec::new();
    let (mut in_diff, mut old, mut new, mut widest) = (false, 0usize, 0usize, 1usize);
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("@@ -") {
            // `@@ -a,b +c,d @@`: rows count on from a (old) and c (new).
            let mut nums = rest
                .split([',', ' ', '+'])
                .filter_map(|n| n.parse::<usize>().ok());
            let (a, _, c) = (
                nums.next().unwrap_or(1),
                nums.next(),
                nums.next().unwrap_or(1),
            );
            if in_diff && !matches!(items.last(), Some(Row::File(_) | Row::Note(_))) {
                items.push(Row::Gap);
            }
            (old, new, in_diff) = (a, c, true);
            continue;
        }
        if in_diff {
            let (sign, rest) = line.split_at(line.chars().next().map_or(0, char::len_utf8));
            match sign {
                "+" => {
                    items.push(Row::Line('+', Some(new), rest));
                    widest = widest.max(new);
                    new += 1;
                    continue;
                }
                "-" => {
                    items.push(Row::Line('-', Some(old), rest));
                    widest = widest.max(old);
                    old += 1;
                    continue;
                }
                " " => {
                    items.push(Row::Line(' ', Some(new), rest));
                    widest = widest.max(new);
                    old += 1;
                    new += 1;
                    continue;
                }
                _ => in_diff = false,
            }
        }
        if line.starts_with("change preview (")
            || line.starts_with("[final newline")
            || line.starts_with("[diff preview truncated")
        {
            if !line.starts_with("change preview (") {
                items.push(Row::Note(line));
            }
        } else if line.starts_with('[') && line.ends_with(']') && line.contains('#') {
            items.push(Row::File(
                line.trim_matches(['[', ']'])
                    .rsplit_once('#')
                    .map_or(line, |(path, _)| path),
            ));
        } else if [
            "updated (",
            "moved from ",
            "replaced ",
            "overwrote ",
            "created ",
            "deleted ",
        ]
        .iter()
        .any(|p| line.starts_with(p))
            || line.starts_with("- ")
            || line == "Warnings:"
        {
            items.push(Row::Note(line));
        }
    }
    let gutter = widest.to_string().len();
    let limit = if ctx.tools_expanded {
        usize::MAX
    } else {
        ctx.ui.tool_output_lines
    };
    // The header already names a single edited file and its `+a −b` totals.
    let header_target = target(t);
    let single_file = items.iter().filter(|i| matches!(i, Row::File(_))).count() == 1;
    let totals_in_header = change_totals(t, theme.ascii()).is_some();
    let code_w = inner.saturating_sub(gutter + 1).max(1);
    let mut all: Rows = Vec::new();
    for item in items {
        match item {
            Row::File(path) if single_file && same_path(&header_target, path) => {}
            Row::Note(text) if totals_in_header && text.starts_with("updated (") => {}
            Row::File(path) => all.extend(text::wrap(
                vec![Line::from(Span::styled(
                    path.to_owned(),
                    theme.fg(Tok::Accent),
                ))],
                inner.max(1),
            )),
            Row::Note(text) => all.extend(
                text::wrap_words(text, inner.max(1))
                    .into_iter()
                    .map(|row| Line::from(Span::styled(row, theme.fg(Tok::Dim)))),
            ),
            Row::Gap => all.push(Line::from(Span::styled(
                format!("{:>gutter$}", theme.sym(Sym::Ellipsis)),
                theme.fg(Tok::Dim),
            ))),
            Row::Line(sign, n, text) => {
                let style = match sign {
                    '+' => theme.fg(Tok::DiffAdded),
                    '-' => theme.fg(Tok::DiffRemoved),
                    _ => theme.fg(Tok::DiffContext),
                };
                let number = format!("{:>gutter$} ", n.map_or(String::new(), |n| n.to_string()));
                let code = vec![Line::from(Span::styled(format!("{sign}{text}"), style))];
                // Wrapped code hangs under the code column, never under the line numbers.
                for (k, piece) in text::wrap(code, code_w).into_iter().enumerate() {
                    let lead = if k == 0 {
                        number.clone()
                    } else {
                        " ".repeat(gutter + 1)
                    };
                    let mut spans = vec![Span::styled(lead, theme.fg(Tok::Dim))];
                    spans.extend(piece.spans);
                    all.push(Line::from(spans));
                }
            }
        }
    }
    let hidden = all.len().saturating_sub(limit);
    all.truncate(limit);
    Some((all, hidden))
}

/// Diff-aware spans for one line of `edit` / `write` output: `+`/`-` hunks, `@@`, the `[path#TAG]`
/// header of a hashline preview and its `N:` line numbers.
fn edit_spans(theme: &Theme, line: &str) -> Vec<Span<'static>> {
    let plain = theme.fg(Tok::ToolOutput);
    if line.starts_with('+') || line.starts_with('-') || line.starts_with("@@") {
        return vec![Span::styled(
            line.to_owned(),
            markdown::diff_style(theme, line),
        )];
    }
    if line.starts_with('[') && line.ends_with(']') && line.contains('#') {
        return vec![Span::styled(line.to_owned(), theme.fg(Tok::Accent))];
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && line[digits..].starts_with(':') {
        return vec![
            Span::styled(line[..=digits].to_owned(), theme.fg(Tok::Dim)),
            Span::styled(line[digits + 1..].to_owned(), plain),
        ];
    }
    vec![Span::styled(line.to_owned(), plain)]
}

/// The bookkeeping lines the shell appends for the model (`[exit code: N]`, `[complete output:
/// artifact://…]`). The card shows the exit status in its footer instead of as output.
fn split_trailer(output: &str) -> String {
    output
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.starts_with("[exit code: ") && !line.starts_with("[complete output: artifact://")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tool output wrapped to `inner` cells and capped at `limit` rows (the configured tool output
/// length when `None`); returns the visible rows and how many were hidden.
fn output_rows(t: &Tool, inner: usize, ctx: &TranscriptCtx, limit: Option<usize>) -> (Rows, usize) {
    let source = match (&t.progress, t.state) {
        (Some(progress), ToolState::Running) => progress.as_str(),
        _ if !ctx.tools_expanded => t.presentation.preview.as_deref().unwrap_or(&t.output),
        _ => t.output.as_str(),
    };
    let source = split_trailer(source);
    let clean = text::sanitize(&source);
    let body = clean.trim_end();
    if body.trim().is_empty() {
        return (Vec::new(), 0);
    }
    let theme = ctx.theme;
    let err = t.state == ToolState::Failed;
    let diffy = !err
        && matches!(
            t.presentation.body,
            crate::runtime::presentation::BodyStyle::Diff
                | crate::runtime::presentation::BodyStyle::Write
        );
    let limit = if ctx.tools_expanded {
        usize::MAX
    } else {
        limit.unwrap_or(ctx.ui.tool_output_lines)
    };
    let mut rows: Rows = Vec::new();
    let mut total = 0;
    for line in body.trim_start_matches('\n').split('\n') {
        let spans = if diffy {
            edit_spans(theme, line)
        } else {
            vec![Span::styled(
                line.to_owned(),
                if err {
                    theme.fg(Tok::Error)
                } else {
                    theme.fg(Tok::ToolOutput)
                },
            )]
        };
        // Output wraps at word boundaries and keeps its indentation; edit previews are code.
        let wrapped_rows: Rows = if diffy {
            text::wrap(vec![Line::from(spans)], inner.max(1))
        } else {
            let style = spans.first().map(|s| s.style).unwrap_or_default();
            wrap_indented(line, inner)
                .into_iter()
                .map(|text| Line::from(Span::styled(text, style)))
                .collect()
        };
        for wrapped in wrapped_rows {
            total += 1;
            if rows.len() < limit {
                rows.push(wrapped);
            }
        }
    }
    let hidden = total - rows.len();
    (rows, hidden)
}

pub(super) fn hidden_hint(hidden: usize, expand_key: &str, theme: &Theme) -> Span<'static> {
    // The shortcut is the user's own binding, so it is named as the keymap shows it.
    Span::styled(
        format!(
            "{} {hidden} more line{} ({expand_key} expands)",
            ellipsis(theme),
            if hidden == 1 { "" } else { "s" }
        ),
        theme.fg(Tok::Dim),
    )
}

fn elapsed_label(secs: f64) -> String {
    if secs < 10.0 {
        format!("{secs:.2}s")
    } else if secs < 100.0 {
        format!("{secs:.1}s")
    } else {
        format!("{secs:.0}s")
    }
}

/// Rows of a compact (one-line) tool's error shown before Ctrl+O expands it.
const COMPACT_ERROR_ROWS: usize = 2;

#[cfg(test)]
mod tests;
