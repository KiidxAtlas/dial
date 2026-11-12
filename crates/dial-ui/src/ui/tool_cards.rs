//! Tool cards lay out the portable presentation supplied by each tool definition.
//! The shared execution phase owns settlement; expanded cards retain full evidence.
use super::{
    Rows,
    chrome::StateMark,
    markdown,
    markdown::truncate_spans,
    text,
    theme::{Sym, Theme, Tok},
    transcript::{TranscriptCtx, ellipsis, fitted},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

pub use crate::runtime::Phase as ToolState;

/// How a finished call ended, as the controller reported it. A call stopped by someone (denied by
/// the user or a rule, cancelled by an interrupt) is not a failure: it renders muted with its own
/// word instead of the error red of a failed call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    Ok,
    Failed,
    /// Refused by the approval gate (the user, a permission rule or read-only mode).
    Denied,
    /// Withheld by another dispatch gate (an earlier failed preview, a prerequisite).
    Blocked,
    /// Stopped by an interrupt before it finished.
    Cancelled,
}

impl ToolOutcome {
    /// The outcome the controller recorded in the result's execution facts. Approval denials are
    /// the blocked results whose controller-owned message starts with `denied`.
    pub fn of(output: &crate::runtime::tool_output::ToolOutput) -> Self {
        use crate::runtime::execution::ToolStatus;
        match output.execution.status {
            Some(ToolStatus::Aborted) => Self::Cancelled,
            Some(ToolStatus::Blocked) if output.text.starts_with("denied") => Self::Denied,
            Some(ToolStatus::Blocked) => Self::Blocked,
            _ if output.is_error => Self::Failed,
            _ => Self::Ok,
        }
    }

    /// A result known only as success or error.
    pub fn from_error(is_error: bool) -> Self {
        if is_error { Self::Failed } else { Self::Ok }
    }

    /// The lifecycle phase the card settles into.
    pub fn state(self) -> ToolState {
        match self {
            Self::Ok => ToolState::Completed,
            Self::Failed => ToolState::Failed,
            Self::Denied | Self::Blocked => ToolState::Blocked,
            Self::Cancelled => ToolState::Interrupted,
        }
    }
}

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
    /// How the call ended, once it reported back; refines the phase's word and mark.
    pub outcome: Option<ToolOutcome>,
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

impl AgentRow {
    /// One worker from its typed record `d`, whichever path observed it (a status/wait result or
    /// the live runtime node), so both paths name, phase and preview a worker the same way.
    pub(super) fn from_record(
        d: &Value,
        fallback_name: &str,
        state: ToolState,
        elapsed_ms: Option<u64>,
        preview: &str,
    ) -> Self {
        let running = state == ToolState::Running;
        AgentRow {
            name: d["name"].as_str().unwrap_or(fallback_name).to_owned(),
            agent: d["agent"].as_str().unwrap_or("").to_owned(),
            model: d["model"].as_str().unwrap_or("").to_owned(),
            task: d["task"]
                .as_str()
                .or_else(|| d["assignment"]["task"].as_str())
                .unwrap_or("")
                .to_owned(),
            state,
            // Only a tool phase (`tool: read`) names what runs now; other phases are not tools.
            current_tool: d["phase"]
                .as_str()
                .and_then(|p| p.strip_prefix("tool: "))
                .filter(|_| running)
                .map(str::to_owned),
            tools: d["tools"].as_u64().unwrap_or(0),
            steps: d["steps"].as_u64().unwrap_or(0),
            elapsed_ms,
            preview: text::sanitize(preview),
        }
    }
}

// ---- tools ---------------------------------------------------------------------------------

/// The one state table for work rows: [`StateMark`] gives glyph, color and word. `None` marks the
/// states without a shared mark (incomplete, needs input), which keep the warning sign and their
/// own word.
pub(super) fn state_mark(state: ToolState, outcome: Option<ToolOutcome>) -> Option<StateMark> {
    Some(match (state, outcome) {
        (ToolState::Blocked, Some(ToolOutcome::Denied)) => StateMark::Denied,
        (ToolState::Interrupted, Some(ToolOutcome::Cancelled)) => StateMark::Cancelled,
        (ToolState::Running, _) => StateMark::Running,
        (ToolState::Completed, _) => StateMark::Done,
        (ToolState::Answered, _) => StateMark::Answered,
        (ToolState::Failed, _) => StateMark::Failed,
        (ToolState::Interrupted, _) => StateMark::Interrupted,
        (ToolState::Blocked, _) => StateMark::Blocked,
        (ToolState::Incomplete | ToolState::NeedsInput, _) => return None,
    })
}

/// The state's one word (`done`, `denied`, `needs input`, …).
pub(super) fn state_word(state: ToolState, outcome: Option<ToolOutcome>) -> &'static str {
    state_mark(state, outcome).map_or(state.label(), StateMark::word)
}

fn state_tok(state: ToolState, outcome: Option<ToolOutcome>) -> Tok {
    state_mark(state, outcome).map_or(Tok::Warning, StateMark::tok)
}

/// Style of a running call's spinner at `tick`: the accent breathing through the brand colors,
/// matching the status-line spinner. The transcript's render cache re-styles these spans per tick.
pub(super) fn spinner_style(theme: &Theme, tick: u64) -> Style {
    theme
        .fg(Tok::Accent)
        .patch(crate::ui::fx::pulse(theme, tick))
}

/// The icon in a work row's gutter. A settled call is a quiet dot so the answer around it leads;
/// only running, failed and unfinished calls draw the eye.
fn phase_glyph(state: ToolState, ctx: &TranscriptCtx) -> (String, Style) {
    outcome_glyph(state, None, ctx)
}

fn outcome_glyph(
    state: ToolState,
    outcome: Option<ToolOutcome>,
    ctx: &TranscriptCtx,
) -> (String, Style) {
    let theme = ctx.theme;
    match state {
        ToolState::Running => (
            theme.spinner(ctx.tick).to_owned(),
            spinner_style(theme, ctx.tick),
        ),
        ToolState::Completed => (theme.sym(Sym::Dot).to_owned(), theme.fg(Tok::Dim)),
        ToolState::Answered => (theme.sym(Sym::Dot).to_owned(), theme.fg(Tok::Muted)),
        _ => match state_mark(state, outcome) {
            Some(mark) => (mark.glyph(theme).to_owned(), theme.fg(mark.tok())),
            None => (theme.sym(Sym::Warn).to_owned(), theme.fg(Tok::Warning)),
        },
    }
}

/// Calls that load context (skill instructions, tool schemas) rather than act on the project.
/// They settle into the quiet context lane instead of reading like work.
fn is_context(t: &Tool) -> bool {
    matches!(t.name.as_str(), "skill" | "tool_load" | "tool_search")
}

fn tool_glyph(t: &Tool, ctx: &TranscriptCtx) -> (String, Style) {
    if is_context(t) && t.state == ToolState::Completed {
        return (
            super::lanes::context_glyph(ctx.theme).to_owned(),
            ctx.theme.fg(Tok::SectionLearn),
        );
    }
    outcome_glyph(t.state, t.outcome, ctx)
}

pub(super) fn tool_rows(t: &Tool, w: u16, ctx: &TranscriptCtx) -> Rows {
    if w == 0
        || (!ctx.ui.show_tools && matches!(t.state, ToolState::Running | ToolState::Completed))
    {
        return Vec::new();
    }
    // Task state lives in the work panel, not in a second transcript plan. Keep
    // failures and refusals visible; the calls remain recorded in session history.
    if t.name == "todo" && matches!(t.state, ToolState::Running | ToolState::Completed) {
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
    if let Some(rows) = check::check_card(t, w as usize, ctx) {
        return rows;
    }
    if let Some(rows) = irc::irc_card(t, w as usize, ctx) {
        return rows;
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
    let border = theme.fg(Tok::SectionConnect);
    // At tiny widths retain the identity, without overflowing or subtracting past zero.
    if w < 6 {
        return vec![fitted(
            vec![Span::styled(format!("MCP {server}"), fill.patch(border))],
            w,
            theme,
        )];
    }
    let separator = theme.sym(Sym::Dot);
    let mut frame = frame::Frame::new(
        vec![Span::styled(
            format!("MCP {separator} {}", text::one_line(server)),
            border,
        )],
        border,
    );
    frame.fill = fill;
    // The server is already in the frame title, so the inner row names only the action.
    let inner = w.saturating_sub(frame::CHROME_W).max(1);
    frame
        .sections
        .push((None, card_with(t, inner, ctx, text::one_line(action))));
    frame.rows(theme, w)
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
fn change_totals(t: &Tool, theme: &Theme) -> Option<String> {
    let (added, removed) = change_counts(t)?;
    Some(format!("+{added} {}{removed}", theme.sym(Sym::Minus)))
}

/// Lines a settled edit added and removed, from its change preview's totals.
pub(in crate::ui) fn change_counts(t: &Tool) -> Option<(u64, u64)> {
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
    Some((
        added.strip_prefix('+')?.parse().ok()?,
        removed.strip_prefix('-')?.parse().ok()?,
    ))
}

/// What a worker row shows for a running worker once its task call returned: it continues in the
/// background, and its live state belongs to the worker pins, not to frozen transcript rows.
const BACKGROUND: &str = "background";

/// The word a worker row's chip and the card's tally use. `live` is whether the task call that
/// owns the row is still running.
fn agent_word(a: &AgentRow, live: bool) -> &'static str {
    if a.state == ToolState::Running && !live {
        BACKGROUND
    } else {
        state_word(a.state, None)
    }
}

/// Worker counts by displayed state, in one fixed order, leaving out states no worker is in.
fn agent_counts(agents: &[AgentRow], live: bool) -> Vec<(&'static str, usize)> {
    const ORDER: &[&str] = &[
        "running",
        BACKGROUND,
        "failed",
        "interrupted",
        "stopped",
        "denied",
        "blocked",
        "incomplete",
        "needs input",
        "answered",
        "done",
    ];
    let words: Vec<&str> = agents.iter().map(|a| agent_word(a, live)).collect();
    ORDER
        .iter()
        .map(|word| (*word, words.iter().filter(|w| *w == word).count()))
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// Worker counts as header facts. A card whose workers all finished cleanly says nothing: "3 done"
/// under a settled card is just noise.
fn worker_tally(agents: &[AgentRow], live: bool) -> Vec<String> {
    let counts = agent_counts(agents, live);
    if counts.iter().all(|(word, _)| *word == "done") {
        return Vec::new();
    }
    counts
        .into_iter()
        .map(|(word, n)| {
            if word == BACKGROUND {
                format!("{n} in background")
            } else {
                format!("{n} {word}")
            }
        })
        .collect()
}

/// Facts shown at the right edge of the header: size of the change or result, run time, exit code.
fn facts(t: &Tool, theme: &Theme) -> Vec<Fact> {
    let keep = |text: String| Fact { text, drop: 0 };
    let mut out = Vec::new();
    if t.presentation.body == crate::runtime::presentation::BodyStyle::Workers {
        out.extend(
            worker_tally(&t.agents, t.state == ToolState::Running)
                .into_iter()
                .map(keep),
        );
    }
    if let Some(totals) = change_totals(t, theme) {
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
        // the target; interrupted, denied, blocked and incomplete always say what happened.
        out.push(Fact {
            text: state_word(t.state, t.outcome).into(),
            drop: u8::from(t.state == ToolState::Failed),
        });
    }
    match t.elapsed {
        // A running call counts up in whole seconds once it has run for a second.
        Some(secs) if t.state == ToolState::Running && secs >= 1.0 => out.push(Fact {
            text: text::duration_label(secs),
            drop: 2,
        }),
        Some(secs) if t.state != ToolState::Running && secs >= 0.05 => out.push(Fact {
            text: text::duration_label_precise(secs),
            drop: 2,
        }),
        _ => {}
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
    text::wrap_words(line, max.max(1))
}

/// Rows previewing what a `write` created: numbered `+` lines from the call's own content.
fn write_preview(t: &Tool, inner: usize, ctx: &TranscriptCtx) -> Option<(Rows, usize)> {
    let content = text::sanitize(&written_content(t)?);
    let theme = ctx.theme;
    let mark = ellipsis(theme);
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
            let mut spans = vec![Span::styled(
                format!("{:>gutter_w$} ", i + 1),
                theme.fg(Tok::Dim),
            )];
            spans.extend(truncate_spans(
                vec![Span::styled(format!("+ {line}"), theme.fg(Tok::DiffAdded))],
                inner.saturating_sub(gutter_w + 1).max(1),
                &mark,
            ));
            Line::from(spans)
        })
        .collect();
    let hidden = lines.len().saturating_sub(rows.len());
    Some((rows, hidden))
}

/// One tool call: a header row (status glyph in the gutter, bold verb, muted target, facts at the
/// right edge), then, when it says something, the body hanging off a `└` under the verb: the diff
/// for edits, the new lines for writes, output for commands, the last error lines for failures.
/// Heavy calls (commands, code cells, edits, network calls) whose body says something are drawn in
/// a frame instead, see [`framed_card`]. Commands and code cells are framed from the moment they
/// start, so a card never changes shape (or verb) when its first output arrives.
fn card(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Rows {
    if let Some(family) = family(t)
        && w >= frame::MIN_W
        && !quiet(t, ctx)
    {
        let (body, hidden) = body_rows(t, w - frame::CHROME_W, ctx);
        let framed_from_start = matches!(family, Family::Shell | Family::Code)
            && t.presentation.body != crate::runtime::presentation::BodyStyle::Quiet;
        if !body.is_empty() || framed_from_start {
            return framed_card(t, family, body, hidden, w, ctx);
        }
    }
    card_with(t, w, ctx, verb(t))
}

/// The kinds of call that are drawn in a frame once they have a body. Each has its own border.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// Shell commands and background jobs.
    Shell,
    /// Code cells and code intelligence: eval, kernel, lsp, debuggers, disassemblers.
    Code,
    /// File changes.
    Edit,
    /// Calls that leave the machine: web, GitHub, browser.
    Connect,
}

fn family(t: &Tool) -> Option<Family> {
    use crate::runtime::presentation::{BodyStyle, Tone};
    // MCP calls have their own service frame; plans and workers have their own layouts.
    if t.name.starts_with("mcp__")
        || matches!(t.presentation.body, BodyStyle::Plan | BodyStyle::Workers)
    {
        return None;
    }
    match t.name.as_str() {
        "bg" | "ssh" => return Some(Family::Shell),
        "eval" | "kernel" | "lsp" | "ida" | "dap" => return Some(Family::Code),
        name if name.starts_with("dap_") => return Some(Family::Code),
        _ => {}
    }
    match t.presentation.tone {
        Tone::Shell => Some(Family::Shell),
        Tone::Edit => Some(Family::Edit),
        Tone::Connect => Some(Family::Connect),
        Tone::Tools | Tone::Workers => None,
    }
}

impl Family {
    fn border(self) -> Tok {
        match self {
            Family::Shell => Tok::Border,
            Family::Code => Tok::SectionTools,
            Family::Edit => Tok::BorderAccent,
            Family::Connect => Tok::SectionConnect,
        }
    }
}

/// A settled lookup or passing check says nothing more than its header until expanded.
fn quiet(t: &Tool, ctx: &TranscriptCtx) -> bool {
    !ctx.tools_expanded
        && t.state == ToolState::Completed
        && t.presentation.body == crate::runtime::presentation::BodyStyle::Quiet
}

/// What a call's body shows at `inner` cells: the last error lines for failures (errors end with
/// their cause), the diff for edits, the new lines for writes, otherwise its output. Returns the
/// rows and how many source lines were hidden after them.
fn body_rows(t: &Tool, inner: usize, ctx: &TranscriptCtx) -> (Rows, usize) {
    if t.state == ToolState::Failed {
        output_rows(t, inner, ctx, true)
    } else if let Some(diff) = diff_rows(t, inner, ctx) {
        diff
    } else if t.presentation.body == crate::runtime::presentation::BodyStyle::Write {
        write_preview(t, inner, ctx).unwrap_or_else(|| output_rows(t, inner, ctx, false))
    } else {
        output_rows(t, inner, ctx, false)
    }
}

/// `╭─ · $ cargo test ─╮`, the body, then `╰─ exit 101 · 1.20s ─╯`. The status glyph and verb
/// lead the top edge, the facts sit in the bottom edge, and a failed call's border turns red.
/// A shell command reads as a prompt (`$ cmd`) rather than under the `Run` verb.
fn framed_card(
    t: &Tool,
    family: Family,
    body: Rows,
    hidden: usize,
    w: usize,
    ctx: &TranscriptCtx,
) -> Rows {
    let theme = ctx.theme;
    let failed = t.state == ToolState::Failed;
    let (glyph, glyph_style) = tool_glyph(t, ctx);
    let verb_text = verb(t);
    let verb_text = if family == Family::Shell && verb_text == "Run" {
        "$".to_owned()
    } else {
        verb_text
    };
    let verb_style = if failed {
        theme.fg(Tok::Error).add_modifier(Modifier::BOLD)
    } else {
        theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD)
    };
    let full_target = target(t);
    // The edge keeps two corner cells, a rule cell and a space on each side of its label.
    let fixed = text::width(&glyph) + 1 + text::width(&verb_text) + 1;
    let room = w.saturating_sub(6 + fixed);
    let target_text = fit_target(&full_target, room, theme.ascii());
    let mut head = vec![
        Span::styled(glyph, glyph_style),
        Span::raw(" "),
        Span::styled(verb_text, verb_style),
    ];
    if !target_text.is_empty() {
        head.push(Span::raw(" "));
        head.push(Span::styled(target_text.clone(), theme.fg(Tok::Muted)));
    }
    let border = theme.fg(if failed { Tok::Error } else { family.border() });
    let mut frame = frame::Frame::new(head, border);
    let inner = w - frame::CHROME_W;
    if ctx.tools_expanded && target_text != full_target {
        frame.sections.push((
            None,
            text::wrap_words(&full_target, inner)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, theme.fg(Tok::Muted))))
                .collect(),
        ));
    }
    let mut body = body;
    // A command framed from its start holds one quiet row until output arrives, so the card reads
    // as running rather than as an empty box, and the first output line takes that row's place.
    if body.is_empty() && t.state == ToolState::Running {
        body.push(Line::from(Span::styled(
            format!("waiting for output{}", theme.sym(Sym::Ellipsis)),
            theme.fg(Tok::Dim),
        )));
    }
    if hidden > 0 {
        body.push(Line::from(hidden_hint(hidden, ctx.expand_key, theme)));
    }
    // A divider is only worth its row when it separates the body from the full target above.
    let label = (!frame.sections.is_empty()).then_some(if failed { "Error" } else { "Output" });
    frame.sections.push((label.map(str::to_owned), body));
    let sep = format!(" {} ", theme.sym(Sym::Dot));
    let mut facts = facts(t, theme)
        .into_iter()
        .map(|f| f.text)
        .collect::<Vec<_>>();
    // A running card says so on its edge, with its clock once it has one, so it never reads as
    // an empty box.
    if t.state == ToolState::Running {
        facts.insert(0, "running".to_owned());
    }
    let facts = facts.join(&sep);
    if !facts.is_empty() {
        frame.foot = vec![Span::styled(
            facts,
            theme.fg(if failed { Tok::Error } else { Tok::Dim }),
        )];
    }
    frame.rows(theme, w)
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
    let want = text::width(&full_target).min(TARGET_MIN);
    let (facts_text, room) = fit_facts(facts(t, theme), w, left_fixed, want, &sep);
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
    if quiet(t, ctx) {
        return rows;
    }
    if t.presentation.body == crate::runtime::presentation::BodyStyle::Workers
        && !t.agents.is_empty()
    {
        if w >= WORKERS_CARD_MIN {
            return workers_card(t, w, ctx);
        }
        rows.extend(agent_tree(
            &t.agents,
            t.state == ToolState::Running,
            "   ",
            w,
            ctx,
        ));
        return rows;
    }
    let inner = w.saturating_sub(GUTTER + 2).max(1);
    let (body, hidden) = body_rows(t, inner, ctx);
    let hook_style = theme.fg(if t.state == ToolState::Failed {
        Tok::Error
    } else {
        Tok::Dim
    });
    let hook = theme.sym(Sym::Hook);
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

/// Joins the facts that fit on a `w`-cell row whose fixed left part takes `left_fixed` cells,
/// giving up the most droppable first while the target would get fewer than `want` cells.
/// Returns the joined facts and the room left for the target.
fn fit_facts(
    mut facts: Vec<Fact>,
    w: usize,
    left_fixed: usize,
    want: usize,
    sep: &str,
) -> (String, usize) {
    loop {
        let joined = facts
            .iter()
            .map(|f| f.text.as_str())
            .collect::<Vec<_>>()
            .join(sep);
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
    }
}

/// The group label (`Read`, `Search`, `Find`, `Skill`) a settled lookup folds into, so
/// consecutive lookups read as one tree instead of a column of near-identical rows.
pub(super) fn lookup_group(
    state: ToolState,
    presentation: &crate::runtime::presentation::Presentation,
) -> Option<String> {
    let label = text::one_line(&presentation.label);
    (state == ToolState::Completed
        && presentation.body == crate::runtime::presentation::BodyStyle::Quiet
        && matches!(label.as_str(), "Read" | "Search" | "Find" | "Skill"))
    .then_some(label)
}

/// Whether a call still running may settle into the `label` group.
pub(super) fn may_join(
    state: ToolState,
    presentation: &crate::runtime::presentation::Presentation,
    label: &str,
) -> bool {
    state == ToolState::Running
        && presentation.body == crate::runtime::presentation::BodyStyle::Quiet
        && text::one_line(&presentation.label) == label
}

/// `·  Read (3)` and one branch per call: its target, and its own facts at the right edge.
/// A run of skill loads folds by skill instead, see [`skill_rows`].
pub(super) fn lookup_tree(tools: &[&Tool], w: u16, ctx: &TranscriptCtx) -> Rows {
    let w = w as usize;
    let Some(first) = tools.first().filter(|_| w > 0) else {
        return Vec::new();
    };
    if tools.iter().all(|t| t.name == "skill") {
        return skill_rows(tools, w, ctx);
    }
    let theme = ctx.theme;
    let (glyph, glyph_style) = phase_glyph(ToolState::Completed, ctx);
    let mut head = gutter_span(glyph, glyph_style);
    head.push(Span::styled(
        verb(first),
        theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD),
    ));
    head.push(Span::styled(
        format!(" ({})", tools.len()),
        theme.fg(Tok::Dim),
    ));
    let mut rows = vec![fitted(head, w, theme)];
    let (branch, last) = (theme.sym(Sym::TreeBranch), theme.sym(Sym::TreeLast));
    let sep = format!(" {} ", theme.sym(Sym::Dot));
    for (i, t) in tools.iter().enumerate() {
        let rail = format!(
            "{}{} ",
            " ".repeat(GUTTER),
            if i + 1 == tools.len() { last } else { branch }
        );
        let full = target(t);
        let want = text::width(&full).min(TARGET_MIN);
        let (facts_text, room) = fit_facts(facts(t, theme), w, text::width(&rail), want, &sep);
        let mut row = vec![
            Span::styled(rail, theme.fg(Tok::Border)),
            Span::styled(fit_target(&full, room, theme.ascii()), theme.fg(Tok::Muted)),
        ];
        if !facts_text.is_empty() {
            let used: usize = row.iter().map(|s| text::width(&s.content)).sum();
            let right_w = text::width(&facts_text);
            row.push(Span::raw(
                " ".repeat(w.saturating_sub(used + right_w).max(1)),
            ));
            row.push(Span::styled(facts_text, theme.fg(Tok::Dim)));
        }
        rows.push(fitted(row, w, theme));
    }
    rows
}

/// The skill a `skill` call loaded and the reference file it read, if any, from its arguments.
fn skill_load(t: &Tool) -> (String, bool) {
    let input = serde_json::from_str::<Value>(&t.input).ok();
    let field = |key: &str| {
        input
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .map(text::one_line)
            .filter(|s| !s.is_empty())
    };
    let name = field("name").unwrap_or_else(|| target(t));
    (name, field("file").is_some())
}

/// A run of skill loads, one row per skill with the count of its reference files:
/// `◆  Skill   rust-best-practices · 2 references`. Several skills hang off a `Skill (N)` head.
/// Each row keeps the single call's layout, so a folded run reads like the context line it replaces.
fn skill_rows(tools: &[&Tool], w: usize, ctx: &TranscriptCtx) -> Rows {
    let dot = ctx.theme.sym(Sym::Dot);
    let mut skills: Vec<(String, usize)> = Vec::new();
    for t in tools {
        let (name, reference) = skill_load(t);
        let at = match skills.iter().position(|(n, _)| *n == name) {
            Some(at) => at,
            None => {
                skills.push((name, 0));
                skills.len() - 1
            }
        };
        skills[at].1 += usize::from(reference);
    }
    let label = |(name, refs): &(String, usize)| match refs {
        0 => name.clone(),
        1 => format!("{name} {dot} 1 reference"),
        n => format!("{name} {dot} {n} references"),
    };
    let mut row = tools[0].clone();
    row.elapsed = None;
    if let [only] = skills.as_slice() {
        row.presentation.target = label(only);
        return card_with(&row, w, ctx, verb(&row));
    }
    row.presentation.target = String::new();
    let mut rows = card_with(&row, w, ctx, format!("{} ({})", verb(&row), skills.len()));
    let (branch, last) = (ctx.theme.sym(Sym::TreeBranch), ctx.theme.sym(Sym::TreeLast));
    for (i, skill) in skills.iter().enumerate() {
        let rail = format!(
            "{}{} ",
            " ".repeat(GUTTER),
            if i + 1 == skills.len() { last } else { branch }
        );
        rows.push(fitted(
            vec![
                Span::styled(rail, ctx.theme.fg(Tok::Border)),
                Span::styled(label(skill), ctx.theme.fg(Tok::Muted)),
            ],
            w,
            ctx.theme,
        ));
    }
    rows
}
/// Narrowest width that frames a workers call; below it the tree hangs off the gutter row.
const WORKERS_CARD_MIN: usize = 40;

/// A workers call as a framed card: the call's mark, verb and target on the top edge, the worker
/// tree inside, and a chip per state with the call's time on the bottom edge. The outline is red
/// when a worker failed, the accent while any still runs, and the plain border once all settled.
fn workers_card(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let live = t.state == ToolState::Running;
    let (glyph, glyph_style) = tool_glyph(t, ctx);
    let failed =
        t.state == ToolState::Failed || t.agents.iter().any(|a| a.state == ToolState::Failed);
    let busy = t.agents.iter().any(|a| a.state == ToolState::Running);
    let border = if failed {
        Tok::Error
    } else if busy {
        Tok::BorderAccent
    } else {
        Tok::Border
    };
    let mut head = vec![
        Span::styled(glyph, glyph_style),
        Span::raw(" "),
        Span::styled(
            verb(t),
            theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD),
        ),
    ];
    let target = target(t);
    if !target.is_empty() {
        head.push(Span::raw(" "));
        head.push(Span::styled(target, theme.fg(Tok::Muted)));
    }
    let mut card = frame::Frame::new(head, theme.fg(border));
    let inner = w - frame::CHROME_W;
    card.sections
        .push((None, agent_tree(&t.agents, live, "", inner, ctx)));

    // Every state gets a chip here, done included: the edge is the card's tally.
    let mut foot: Vec<Span<'static>> = Vec::new();
    for (word, n) in agent_counts(&t.agents, live) {
        let tok = match word {
            // Same colors as each worker's own chip: settled work is quiet, live work is accent.
            "done" => Tok::Dim,
            BACKGROUND => Tok::Muted,
            "running" => Tok::Accent,
            "failed" => Tok::Error,
            _ => Tok::Warning,
        };
        let label = if word == BACKGROUND {
            format!("{n} in background")
        } else {
            format!("{n} {word}")
        };
        if !foot.is_empty() {
            foot.push(Span::raw(" "));
        }
        foot.push(frame::chip(theme, &label, tok));
    }
    if let Some(secs) = t.elapsed.filter(|s| *s >= 0.05) {
        foot.push(Span::styled(
            format!(" {} ", theme.sym(Sym::Dot)),
            theme.fg(Tok::Dim),
        ));
        foot.push(Span::styled(
            text::duration_label_precise(secs),
            theme.fg(Tok::Dim),
        ));
    }
    card.foot = foot;
    card.rows(theme, w)
}

/// The agents a `task` call spawned, as a tree: per agent a status row (glyph, name, type, model,
/// assignment, tool count and time at the right), then one hook row under it with the tool it is
/// running now, or, once settled, the first line of what it reported. `live` is whether the task
/// call is still running: once it returned, a worker still running is drawn as a static
/// "continuing in background" row, because these rows freeze in scrollback. `lead` indents the
/// tree (the gutter when unframed, nothing inside a frame).
fn agent_tree(agents: &[AgentRow], live: bool, lead: &str, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let mark = ellipsis(theme);
    let ascii = theme.ascii();
    let (branch, last) = (theme.sym(Sym::TreeBranch), theme.sym(Sym::TreeLast));
    let pipe = format!("{} ", theme.sym(Sym::TreeStem));
    let hook = theme.sym(Sym::Dot);
    let rail = theme.fg(Tok::Border);
    let mut rows = Vec::new();
    for (i, a) in agents.iter().enumerate() {
        let is_last = i + 1 == agents.len();
        let background = a.state == ToolState::Running && !live;
        let (glyph, glyph_style) = if background {
            (theme.sym(Sym::Pending).to_owned(), theme.fg(Tok::Muted))
        } else {
            phase_glyph(a.state, ctx)
        };
        let mut facts = Vec::new();
        // The explicit chip below carries phase; right-side facts are counts and time only.
        if a.tools > 0 {
            facts.push(plural(a.tools as usize, "tool"));
        } else if a.state == ToolState::Running && a.steps > 0 {
            facts.push(format!("step {}", a.steps));
        }
        if let Some(ms) = a.elapsed_ms {
            facts.push(text::duration_label(ms as f64 / 1000.0));
        }
        let facts = facts.join(&format!(" {} ", theme.sym(Sym::Dot)));
        let mut left = vec![
            Span::styled(
                format!("{lead}{} ", if is_last { last } else { branch }),
                rail,
            ),
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
        left.push(Span::raw(" "));
        let chip_tok = match a.state {
            _ if background => Tok::Muted,
            ToolState::Completed => Tok::Dim,
            state => state_tok(state, None),
        };
        left.push(frame::chip(theme, agent_word(a, live), chip_tok));
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
            // Same quiet token as a tool card's facts.
            row.push(Span::styled(facts, theme.fg(Tok::Dim)));
        }
        rows.push(fitted(row, w, theme));

        let detail = match (&a.current_tool, a.state) {
            // The `background` chip already says it; a preview, when there is one, is news.
            _ if background && a.preview.is_empty() => None,
            _ if background => Some((a.preview.clone(), theme.fg(Tok::Dim))),
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
            let indent = format!("{lead}{}  ", if is_last { "  " } else { pipe.as_str() });
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

/// Narrowest width that frames a plan; below it the list hangs off the gutter row.
const PLAN_CARD_MIN: usize = 40;
/// Cells of the plan's progress bar.
const PLAN_BAR: usize = 8;

/// A todo call: the list as it now stands with this call's changes emphasized. Wide enough, it is a
/// card with a progress bar and the count on the top edge and blocked steps on the bottom edge;
/// the outline is green once every step closed and amber while one is blocked.
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
    let summary = if tasks.is_empty() {
        "no tasks".to_owned()
    } else if closed == tasks.len() {
        format!("all {} done", tasks.len())
    } else {
        format!("{closed}/{} done", tasks.len())
    };
    if w >= PLAN_CARD_MIN && !tasks.is_empty() {
        use crate::agent::workflow::todo::Status;
        let blocked = tasks
            .iter()
            .filter(|(task, _)| task.status == Status::Blocked)
            .count();
        let filled = (closed * PLAN_BAR).div_ceil(tasks.len()).min(PLAN_BAR);
        let (full, empty) = if theme.ascii() {
            ("#", "-")
        } else {
            ("▰", "▱")
        };
        let border = if blocked > 0 {
            Tok::Warning
        } else if closed == tasks.len() {
            Tok::Success
        } else {
            Tok::BorderAccent
        };
        let mut card = frame::Frame::new(
            vec![
                Span::styled(glyph, glyph_style),
                Span::raw(" "),
                Span::styled("Plan", verb_style),
                Span::raw(" "),
                Span::styled(full.repeat(filled), theme.fg(Tok::Success)),
                Span::styled(empty.repeat(PLAN_BAR - filled), theme.fg(Tok::Dim)),
                Span::raw(" "),
                Span::styled(summary, theme.fg(Tok::Muted)),
            ],
            theme.fg(border),
        );
        let inner = w - frame::CHROME_W;
        card.sections.push((
            None,
            super::tasks::card_rows(tasks, inner, theme, ctx.tools_expanded),
        ));
        if blocked > 0 {
            card.foot = vec![frame::chip(
                theme,
                &format!("{blocked} blocked"),
                Tok::Warning,
            )];
        }
        return card.rows(theme, w);
    }
    let mut spans = gutter_span(glyph, glyph_style);
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
        ctx.ui.tool_output_lines.max(1)
    };
    // The header already names a single edited file and its `+a −b` totals.
    let header_target = target(t);
    let single_file = items.iter().filter(|i| matches!(i, Row::File(_))).count() == 1;
    let totals_in_header = change_totals(t, theme).is_some();
    let code_w = inner.saturating_sub(gutter + 1).max(1);
    // Rows per source line, so the hidden count names lines rather than wrapped rows.
    let mut all: Vec<Rows> = Vec::new();
    for item in items {
        match item {
            Row::File(path) if single_file && same_path(&header_target, path) => {}
            Row::Note(text) if totals_in_header && text.starts_with("updated (") => {}
            Row::File(path) => all.push(text::wrap(
                vec![Line::from(Span::styled(
                    path.to_owned(),
                    theme.fg(Tok::Accent),
                ))],
                inner.max(1),
            )),
            Row::Note(text) => all.push(
                text::wrap_words(text, inner.max(1))
                    .into_iter()
                    .map(|row| Line::from(Span::styled(row, theme.fg(Tok::Dim))))
                    .collect(),
            ),
            Row::Gap => all.push(vec![Line::from(Span::styled(
                format!("{:>gutter$}", theme.sym(Sym::Ellipsis)),
                theme.fg(Tok::Dim),
            ))]),
            Row::Line(sign, n, text) => {
                let style = match sign {
                    '+' => theme.fg(Tok::DiffAdded),
                    '-' => theme.fg(Tok::DiffRemoved),
                    _ => theme.fg(Tok::DiffContext),
                };
                let number = format!("{:>gutter$} ", n.map_or(String::new(), |n| n.to_string()));
                let code = vec![Line::from(Span::styled(format!("{sign}{text}"), style))];
                // Wrapped code hangs under the code column, never under the line numbers.
                let rows = text::wrap(code, code_w)
                    .into_iter()
                    .enumerate()
                    .map(|(k, piece)| {
                        let lead = if k == 0 {
                            number.clone()
                        } else {
                            " ".repeat(gutter + 1)
                        };
                        let mut spans = vec![Span::styled(lead, theme.fg(Tok::Dim))];
                        spans.extend(piece.spans);
                        Line::from(spans)
                    })
                    .collect();
                all.push(rows);
            }
        }
    }
    Some(take_lines(all, limit, false))
}

/// Whole source lines (each given as its wrapped rows) that fit in `limit` rows, taken from the
/// start, or from the end for `tail`, and how many source lines are not shown in full. A first line
/// taller than `limit` shows its first (or, for `tail`, last) `limit` rows.
fn take_lines(lines: Vec<Rows>, limit: usize, tail: bool) -> (Rows, usize) {
    let total = lines.len();
    let mut kept: Vec<Rows> = Vec::new();
    let mut used = 0usize;
    let mut partial = false;
    let ordered: Box<dyn Iterator<Item = Rows>> = if tail {
        Box::new(lines.into_iter().rev())
    } else {
        Box::new(lines.into_iter())
    };
    for mut line in ordered {
        if used.saturating_add(line.len()) <= limit {
            used += line.len();
            kept.push(line);
            continue;
        }
        if kept.is_empty() {
            if tail {
                line.drain(..line.len() - limit);
            } else {
                line.truncate(limit);
            }
            kept.push(line);
            partial = true;
        }
        break;
    }
    let full = kept.len() - usize::from(partial);
    if tail {
        kept.reverse();
    }
    (kept.into_iter().flatten().collect(), total - full)
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

/// Tool output wrapped to `inner` cells and capped at the configured tool output length. `tail`
/// keeps the last lines instead of the first (a failure's cause is at its end) and puts the hint
/// for the earlier lines above them; otherwise returns the rows and how many source lines follow.
fn output_rows(t: &Tool, inner: usize, ctx: &TranscriptCtx, tail: bool) -> (Rows, usize) {
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
        ctx.ui.tool_output_lines.max(1)
    };
    let lines: Vec<Rows> = body
        .trim_start_matches('\n')
        .split('\n')
        .map(|line| {
            // Output wraps at word boundaries and keeps its indentation; edit previews are code.
            if diffy {
                text::wrap(vec![Line::from(edit_spans(theme, line))], inner.max(1))
            } else {
                let style = if err {
                    theme.fg(Tok::Error)
                } else {
                    theme.fg(Tok::ToolOutput)
                };
                wrap_indented(line, inner)
                    .into_iter()
                    .map(|text| Line::from(Span::styled(text, style)))
                    .collect()
            }
        })
        .collect();
    // Hiding a single line costs the same row as showing it.
    let total: usize = lines.iter().map(Vec::len).sum();
    let limit = if total == limit.saturating_add(1) {
        total
    } else {
        limit
    };
    let (rows, hidden) = take_lines(lines, limit, tail);
    if tail && hidden > 0 {
        let mut out = vec![Line::from(more_hint(
            hidden,
            "earlier",
            ctx.expand_key,
            theme,
        ))];
        out.extend(rows);
        return (out, 0);
    }
    (rows, hidden)
}

pub(super) fn hidden_hint(hidden: usize, expand_key: &str, theme: &Theme) -> Span<'static> {
    more_hint(hidden, "more", expand_key, theme)
}

/// `… 3 more lines (Ctrl+O expands)`. The shortcut is the user's own binding, so it is named as the
/// keymap shows it.
fn more_hint(hidden: usize, which: &str, expand_key: &str, theme: &Theme) -> Span<'static> {
    Span::styled(
        format!(
            "{} {hidden} {which} line{} ({expand_key} expands)",
            ellipsis(theme),
            if hidden == 1 { "" } else { "s" }
        ),
        theme.fg(Tok::Dim),
    )
}

mod check;
pub(in crate::ui) use check::{Verdict, command as check_command, verdict};
pub(in crate::ui) mod frame;
mod irc;
pub(super) use irc::incoming_notice;

#[cfg(test)]
mod tests;
