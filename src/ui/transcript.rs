//! The conversation as blocks (user, assistant, reasoning, tools, notices, errors, turn stats) that
//! render to styled rows. The transcript also owns the block lifecycle that makes inline rendering
//! work: a block is *active* while it streams (or a tool is pending) and *settled* afterwards;
//! settled rows go to the terminal's scrollback exactly once, in order, never past an active block;
//! the active tail is repainted every frame, and when it grows taller than the live region its
//! stable head is *spilled* to scrollback early.
//!
//! Spilling a streaming block: only rows that cannot change any more are spilled, i.e. rows of
//! source text before a blank line that is outside a code fence and is followed by a new
//! column-0 block (so the preceding block, list included, is closed). The block remembers how much
//! *source* it spilled (`Spill::src`) and how many rows that was at the width it was rendered at.
//! On finalization only the remainder is emitted. If the terminal width changes in between, the
//! spilled source prefix is re-rendered at the new width and its row count becomes the new offset,
//! so nothing is duplicated or dropped; rows already in scrollback keep their old wrapping (the
//! terminal owns them). Residual edge case: a markdown construct whose earlier rows change when
//! later text arrives (for example a list that turns loose after a spill) is guarded by comparing the
//! spilled rows with the block's current rendering before spilling, but a change *after* the spill
//! cannot be undone, so the remainder is taken by row count and may repeat or skip a row.
use super::{
    Rows, markdown,
    markdown::truncate_spans,
    text,
    theme::{Sym, Theme, Tok},
};
use crate::llm::model_config::UiConfig;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolState {
    Pending,
    Ok,
    Err,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnStats {
    /// Wall-clock time of the turn's end, already formatted (`2026-10-01 10:54:32`).
    pub clock: String,
    pub elapsed_secs: f64,
    pub prompt_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_per_second: Option<f64>,
}

#[derive(Clone, Copy)]
pub struct TranscriptCtx<'a> {
    pub theme: &'a Theme,
    pub ui: &'a UiConfig,
    /// Animation frame (~12 fps) for pending spinners.
    pub tick: u64,
    pub tools_expanded: bool,
}

struct Tool {
    id: String,
    name: String,
    summary: String,
    input: String,
    state: ToolState,
    output: String,
    elapsed: Option<f64>,
    /// For a `todo` call: the list after it, each task flagged when this call changed it.
    todo: Option<Vec<(crate::agent::todo::Task, bool)>>,
}

enum Kind {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(Tool),
    Notice { title: String, text: String },
    Error(String),
    Stats(TurnStats),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prose {
    Assistant,
    Thinking,
}

/// How much of a streaming block was already handed to scrollback.
#[derive(Default)]
struct Spill {
    /// Bytes of the block's text that were spilled (always a line start).
    src: usize,
    /// Rows those bytes make at `key`.
    rows: usize,
    /// `(width, show_thinking)` that `rows` was measured at.
    key: Option<(u16, bool)>,
}

struct Block {
    kind: Kind,
    active: bool,
    spill: Spill,
    markdown: std::cell::RefCell<StreamingRows>,
}

impl Block {
    fn new(kind: Kind, active: bool) -> Self {
        Block {
            kind,
            active,
            spill: Spill::default(),
            markdown: std::cell::RefCell::new(StreamingRows::default()),
        }
    }

    fn prose(&self) -> Option<(Prose, &str)> {
        match &self.kind {
            Kind::Assistant(t) => Some((Prose::Assistant, t.as_str())),
            Kind::Thinking(t) => Some((Prose::Thinking, t.as_str())),
            _ => None,
        }
    }

    fn render(&self, w: u16, ctx: &TranscriptCtx) -> Rows {
        match &self.kind {
            Kind::User(t) => user_rows(t, w, ctx),
            Kind::Assistant(t) => self.markdown.borrow_mut().render(t, w, ctx.theme),
            Kind::Thinking(t) => prose_rows(Prose::Thinking, t, w, ctx),
            Kind::Tool(t) => tool_rows(t, w, ctx),
            Kind::Notice { title, text } => notice_rows(title, text, w, ctx),
            Kind::Error(t) => error_rows(t, w, ctx),
            Kind::Stats(s) => stats_rows(s, w, ctx),
        }
    }

    /// Rows of this block already in scrollback because of a spill (re-measured on width change).
    fn emitted(&mut self, w: u16, ctx: &TranscriptCtx) -> usize {
        if self.spill.src == 0 {
            return 0;
        }
        let key = (w, ctx.ui.show_thinking);
        if self.spill.key != Some(key) {
            let rows = self.measure_prefix(w, ctx);
            self.spill.rows = rows;
            self.spill.key = Some(key);
        }
        self.spill.rows
    }

    fn measure_prefix(&self, w: u16, ctx: &TranscriptCtx) -> usize {
        self.prose().map_or(0, |(kind, text)| {
            prose_rows(kind, &text[..self.spill.src.min(text.len())], w, ctx).len()
        })
    }

    /// Latest point at which more of this block's head can be spilled: `(source offset, rows)`,
    /// only if those rows are exactly the head of the block's current rendering `full`.
    fn spill_point(
        &self,
        w: u16,
        ctx: &TranscriptCtx,
        full: &Rows,
        skip: usize,
    ) -> Option<(usize, usize)> {
        let (kind, text) = self.prose()?;
        if kind == Prose::Thinking && !ctx.ui.show_thinking {
            return None;
        }
        for &cut in stable_cuts(text, self.spill.src).iter().rev().take(4) {
            let prefix = prose_rows(kind, &text[..cut], w, ctx);
            let p = prefix.len();
            if p > skip && p <= full.len() && full[..p] == prefix[..] {
                return Some((cut, p));
            }
        }
        None
    }
}

/// Cache only complete top-level blocks. References and HTML can retroactively change earlier
/// blocks, so those documents deliberately use the full parser.
#[derive(Default)]
struct StreamingRows {
    key: Option<(u16, Theme)>,
    cut: usize,
    prefix: Rows,
}

impl StreamingRows {
    fn render(&mut self, source: &str, width: u16, theme: &Theme) -> Rows {
        if self
            .key
            .as_ref()
            .is_none_or(|(w, t)| *w != width || t != theme)
            || self.cut > source.len()
        {
            self.key = Some((width, theme.clone()));
            self.cut = 0;
            self.prefix.clear();
        }
        if source.contains(['[', '<']) || width == 0 {
            self.cut = 0;
            self.prefix.clear();
            return markdown::render(source, width, theme, false);
        }
        let tail = &source[self.cut..];
        let cut = stable_cuts(tail, 0).into_iter().rev().find(|&cut| {
            tail[cut..]
                .trim_start_matches(['\n', '\r'])
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
        });
        if let Some(cut) = cut {
            let rows = markdown::render(&tail[..cut], width, theme, false);
            if !rows.is_empty() {
                if !self.prefix.is_empty() {
                    self.prefix.push(blank_row());
                }
                self.prefix.extend(rows);
            }
            self.cut += cut;
        }
        let mut rows = self.prefix.clone();
        let tail = markdown::render(&source[self.cut..], width, theme, false);
        if !rows.is_empty() && !tail.is_empty() {
            rows.push(blank_row());
        }
        rows.extend(tail);
        rows
    }
}

/// Byte offsets of blank lines (after `after`) that end a block which can no longer change: outside
/// a code fence, and followed by a column-0 non-list line.
fn stable_cuts(text: &str, after: usize) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut offset = 0;
    let mut pending: Option<usize> = None;
    let mut prev_blank = true;
    let mut seen_content = false;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let body = line.trim_end_matches(['\n', '\r']);
        let t = body.trim_start();
        if let Some((ch, n)) = fence {
            let run = t.chars().take_while(|c| *c == ch).count();
            if run >= n && t[run * ch.len_utf8()..].trim().is_empty() {
                fence = None;
            }
            prev_blank = false;
            continue;
        }
        if t.is_empty() {
            if line.ends_with('\n') && !prev_blank && seen_content {
                pending = Some(start);
            }
            prev_blank = true;
            continue;
        }
        if let Some(at) = pending.take()
            && !body.starts_with([' ', '\t'])
            && !is_list_marker(body)
            && at > after
        {
            cuts.push(at);
        }
        if let Some(ch) = t.chars().next().filter(|c| matches!(c, '`' | '~')) {
            let run = t.chars().take_while(|c| *c == ch).count();
            if run >= 3 {
                fence = Some((ch, run));
            }
        }
        prev_blank = false;
        seen_content = true;
    }
    cuts
}

fn is_list_marker(line: &str) -> bool {
    let t = line.trim_start();
    if matches!(t, "-" | "*" | "+")
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
    {
        return true;
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") "))
}

// ---------------------------------------------------------------------------------------------
// block renderers

fn blank_row() -> Line<'static> {
    Line::default()
}

fn ellipsis(theme: &Theme) -> String {
    theme.sym(Sym::Ellipsis).to_owned()
}

fn fitted(spans: Vec<Span<'static>>, w: usize, theme: &Theme) -> Line<'static> {
    Line::from(truncate_spans(spans, w, &ellipsis(theme)))
}

/// One row filled to exactly `w` cells with `style` as its background.
fn tinted_row(content: &str, w: usize, style: Style) -> Line<'static> {
    let content = text::clip(content, w);
    Line::from(Span::styled(text::pad(&content, w), style))
}

fn user_rows(raw: &str, w: u16, ctx: &TranscriptCtx) -> Rows {
    let w = w as usize;
    let clean = text::sanitize(raw);
    let body = clean.trim();
    if w == 0 || body.is_empty() {
        return Vec::new();
    }
    let style = ctx.theme.on(Tok::UserText, Tok::UserBg);
    let mut rows = vec![tinted_row("", w, style)];
    if w < 3 {
        rows.push(tinted_row(&body.replace('\n', " "), w, style));
    } else {
        let inner = w - 2;
        for line in text::wrap_words(body, inner) {
            rows.push(tinted_row(
                &format!(" {} ", text::pad(&text::clip(&line, inner), inner)),
                w,
                style,
            ));
        }
    }
    rows.push(tinted_row("", w, style));
    rows
}

fn prose_rows(kind: Prose, raw: &str, w: u16, ctx: &TranscriptCtx) -> Rows {
    match kind {
        Prose::Assistant => markdown::render(raw, w, ctx.theme, false),
        Prose::Thinking => thinking_rows(raw, w, ctx),
    }
}

fn thinking_rows(raw: &str, w: u16, ctx: &TranscriptCtx) -> Rows {
    let w = w as usize;
    let clean = text::sanitize(raw);
    let body = clean.trim();
    if w == 0 || body.is_empty() {
        return Vec::new();
    }
    let style = ctx.theme.fg(Tok::ThinkingText);
    if !ctx.ui.show_thinking {
        let n = body.lines().filter(|l| !l.trim().is_empty()).count();
        let label = format!(
            "{} reasoning ({n} line{})",
            ctx.theme.sym(Sym::Pointer),
            if n == 1 { "" } else { "s" }
        );
        return vec![fitted(vec![Span::styled(label, style)], w, ctx.theme)];
    }
    text::wrap_words(body, w)
        .into_iter()
        .map(|row| Line::from(Span::styled(text::clip(&row, w), style)))
        .collect()
}

fn notice_rows(title: &str, body: &str, w: u16, ctx: &TranscriptCtx) -> Rows {
    let w = w as usize;
    let theme = ctx.theme;
    let title = text::one_line(title);
    let clean = text::sanitize(body);
    let body = clean.trim();
    if w == 0 || (title.trim().is_empty() && body.is_empty()) {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let indent = if title.trim().is_empty() { 0 } else { 2 };
    if indent > 0 {
        let head = vec![
            Span::styled(
                format!("{} ", theme.sym(Sym::Bullet)),
                theme.fg(Tok::Accent),
            ),
            Span::styled(
                title.trim().to_owned(),
                theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
            ),
        ];
        rows.push(fitted(head, w, theme));
    }
    if !body.is_empty() {
        let room = w.saturating_sub(indent).max(1);
        for row in text::wrap_words(body, room) {
            let mut spans = Vec::new();
            if indent > 0 && w > indent {
                spans.push(Span::raw(" ".repeat(indent)));
            }
            spans.push(Span::styled(row, theme.fg(Tok::Muted)));
            rows.push(fitted(spans, w, theme));
        }
    }
    rows
}

fn error_rows(body: &str, w: u16, ctx: &TranscriptCtx) -> Rows {
    let w = w as usize;
    let clean = text::sanitize(body);
    let body = clean.trim();
    if w == 0 || body.is_empty() {
        return Vec::new();
    }
    let theme = ctx.theme;
    let style = theme.fg(Tok::Error);
    let glyph = format!("{} ", theme.sym(Sym::Cross2));
    let gw = text::width(&glyph);
    let room = w.saturating_sub(gw).max(1);
    text::wrap_words(body, room)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 {
                glyph.clone()
            } else {
                " ".repeat(gw)
            };
            fitted(
                vec![Span::styled(lead, style), Span::styled(row, style)],
                w,
                theme,
            )
        })
        .collect()
}

fn compact_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}K", n as f64 / 1_000.0),
        10_000..1_000_000 => format!("{}K", (n as f64 / 1_000.0).round() as u64),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn stats_rows(s: &TurnStats, w: u16, ctx: &TranscriptCtx) -> Rows {
    if !ctx.ui.show_turn_stats || w == 0 {
        return Vec::new();
    }
    let theme = ctx.theme;
    let mut parts: Vec<String> = Vec::new();
    let clock = text::one_line(&s.clock);
    if !clock.trim().is_empty() {
        parts.push(clock.trim().to_owned());
    }
    parts.push(format!("{} {:.1}s", theme.sym(Sym::Delta), s.elapsed_secs));
    let mut tokens = Vec::new();
    if let Some(n) = s.prompt_tokens {
        tokens.push(format!("{}{}", theme.sym(Sym::ArrowUp), compact_count(n)));
    }
    if let Some(n) = s.output_tokens {
        tokens.push(format!("{}{}", theme.sym(Sym::ArrowDown), compact_count(n)));
    }
    if let Some(rate) = s.tokens_per_second {
        let icon = theme.sym(Sym::IconRate);
        tokens.push(if icon.is_empty() {
            format!("{rate:.1} tok/s")
        } else {
            format!("{icon}{rate:.1}/s")
        });
    }
    if !tokens.is_empty() {
        parts.push(tokens.join(" "));
    }
    vec![fitted(
        vec![Span::styled(parts.join("  "), theme.fg(Tok::Dim))],
        w as usize,
        theme,
    )]
}

// ---- tools ---------------------------------------------------------------------------------

fn tool_title(t: &Tool) -> String {
    // A hashline patch is titled by the files it touches, not by its first operation rows.
    if t.name == "edit" {
        let paths: Vec<&str> = t
            .summary
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix('[')?
                    .strip_suffix(']')?
                    .rsplit_once('#')
                    .map(|(path, _)| path)
            })
            .collect();
        if !paths.is_empty() {
            return text::one_line(&paths.join(", "));
        }
    }
    let summary = text::one_line(&t.summary);
    if !summary.trim().is_empty() {
        return summary.trim().to_owned();
    }
    serde_json::from_str::<serde_json::Value>(&t.input)
        .ok()
        .and_then(|v| {
            ["command", "path", "pattern", "query"]
                .iter()
                .find_map(|k| v.get(*k).and_then(|x| x.as_str()).map(text::one_line))
        })
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn capitalize(name: &str) -> String {
    let name = text::one_line(name);
    let mut chars = name.chars();
    chars.next().map_or(String::new(), |c| {
        c.to_uppercase().collect::<String>() + chars.as_str()
    })
}

fn tool_glyph(t: &Tool, ctx: &TranscriptCtx) -> (String, Style) {
    let theme = ctx.theme;
    match t.state {
        ToolState::Pending => (theme.spinner(ctx.tick).to_owned(), theme.fg(Tok::Accent)),
        ToolState::Ok => (theme.sym(Sym::Check).to_owned(), theme.fg(Tok::Success)),
        ToolState::Err => (theme.sym(Sym::Cross2).to_owned(), theme.fg(Tok::Error)),
    }
}

fn tool_rows(t: &Tool, w: u16, ctx: &TranscriptCtx) -> Rows {
    if w == 0 || (!ctx.ui.show_tools && t.state != ToolState::Err) {
        return Vec::new();
    }
    match t.name.as_str() {
        "todo" if t.todo.is_some() && t.state == ToolState::Ok => todo_card(t, w as usize, ctx),
        // A passing automatic check is one line; a failing one keeps the full box with its diagnostics.
        "bash" if t.state == ToolState::Ok && t.summary.starts_with("Verify ") => {
            compact_tool(t, w as usize, ctx)
        }
        "bash" | "edit" | "write" | "ast_edit" if w as usize >= BOX_MIN => {
            tool_box(t, w as usize, ctx)
        }
        _ => compact_tool(t, w as usize, ctx),
    }
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
    let mut spans = vec![
        Span::styled(glyph, glyph_style),
        Span::raw(" "),
        Span::styled("Todo".to_owned(), theme.fg(Tok::ToolTitle)),
    ];
    let title = tool_title(t);
    if !title.is_empty() {
        spans.push(Span::styled(format!(" {title}"), theme.fg(Tok::ToolOutput)));
    }
    spans.push(Span::styled(
        format!("  {} {closed}/{} done", theme.sym(Sym::Dot), tasks.len()),
        theme.fg(Tok::Dim),
    ));
    let mut rows = vec![fitted(spans, w, theme)];
    rows.extend(super::tasks::card_rows(tasks, w, theme, ctx.tools_expanded));
    rows
}

/// Edit results show at least this many diff rows before collapsing, whatever `tool_output_lines` says.
const DIFF_MIN_ROWS: usize = 20;

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
        ctx.ui.tool_output_lines.max(DIFF_MIN_ROWS)
    };
    let mut rows: Rows = Vec::new();
    let mut total = 0;
    for item in items {
        let spans = match item {
            Row::File(path) => vec![Span::styled(path.to_owned(), theme.fg(Tok::Accent))],
            Row::Note(text) => vec![Span::styled(text.to_owned(), theme.fg(Tok::Dim))],
            Row::Gap => vec![Span::styled(
                format!("{:>gutter$}", theme.sym(Sym::Ellipsis)),
                theme.fg(Tok::Dim),
            )],
            Row::Line(sign, n, text) => {
                let style = match sign {
                    '+' => theme.fg(Tok::DiffAdded),
                    '-' => theme.fg(Tok::DiffRemoved),
                    _ => theme.fg(Tok::DiffContext),
                };
                vec![
                    Span::styled(
                        format!("{:>gutter$} ", n.map_or(String::new(), |n| n.to_string())),
                        theme.fg(Tok::Dim),
                    ),
                    Span::styled(format!("{sign}{text}"), style),
                ]
            }
        };
        for wrapped in text::wrap(vec![Line::from(spans)], inner.max(1)) {
            total += 1;
            if rows.len() < limit {
                rows.push(wrapped);
            }
        }
    }
    let hidden = total - rows.len();
    Some((rows, hidden))
}

const BOX_MIN: usize = 12;
const MAX_TITLE_ROWS: usize = 4;

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

/// Tool output wrapped to `inner` cells and capped; returns the visible rows and how many were hidden.
fn output_rows(t: &Tool, inner: usize, ctx: &TranscriptCtx) -> (Rows, usize) {
    let clean = text::sanitize(&t.output);
    let body = clean.trim_end();
    if body.trim().is_empty() {
        return (Vec::new(), 0);
    }
    let theme = ctx.theme;
    let err = t.state == ToolState::Err;
    let diffy = !err && matches!(t.name.as_str(), "edit" | "write" | "ast_edit");
    let limit = if ctx.tools_expanded {
        usize::MAX
    } else {
        ctx.ui.tool_output_lines
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
        for wrapped in text::wrap(vec![Line::from(spans)], inner.max(1)) {
            total += 1;
            if rows.len() < limit {
                rows.push(wrapped);
            }
        }
    }
    let hidden = total - rows.len();
    (rows, hidden)
}

fn hidden_hint(hidden: usize, theme: &Theme) -> Span<'static> {
    Span::styled(
        format!(
            "{} {hidden} more line{} (Ctrl-O expands)",
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

fn tool_box(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let bg_tok = match t.state {
        ToolState::Pending => Tok::ToolPendingBg,
        ToolState::Ok => Tok::ToolSuccessBg,
        ToolState::Err => Tok::ToolErrorBg,
    };
    let bg = theme.bg(bg_tok);
    let border = if t.state == ToolState::Err {
        theme.fg(Tok::Error)
    } else {
        theme.fg(Tok::Border)
    }
    .patch(bg);
    let (glyph, glyph_style) = tool_glyph(t, ctx);
    let (v, h) = (theme.sym(Sym::BoxV).to_owned(), theme.sym(Sym::BoxH));
    let inner = w - 4;
    let mark = ellipsis(theme);

    let row = |spans: Vec<Span<'static>>| -> Line<'static> {
        let spans = truncate_spans(spans, inner, &mark);
        let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
        let mut out = vec![Span::styled(format!("{v} "), border.patch(bg))];
        out.extend(spans.into_iter().map(|mut s| {
            s.style = s.style.patch(bg);
            s
        }));
        out.push(Span::styled(" ".repeat(inner - used + 1), bg));
        out.push(Span::styled(v.clone(), border));
        Line::from(out)
    };
    let rule = |left: &str, label: &str, right: &str| -> Line<'static> {
        let label_w = text::width(label);
        let fill = w - 2 - label_w;
        Line::from(Span::styled(
            format!("{left}{label}{}{right}", h.repeat(fill)),
            border,
        ))
    };

    let mut rows = vec![rule(theme.sym(Sym::BoxTl), "", theme.sym(Sym::BoxTr))];

    // title (+ state glyph on the first row)
    let title = tool_title(t);
    let title = match t.name.as_str() {
        "bash" => format!("$ {title}"),
        _ if title.is_empty() => capitalize(&t.name),
        _ => title,
    };
    let glyph_w = text::width(&glyph);
    let room = inner.saturating_sub(glyph_w + 1).max(1);
    let mut title_rows: Vec<String> = text::wrap_words(&title, room)
        .into_iter()
        .map(|r| text::clip(&r, room))
        .collect();
    if !ctx.tools_expanded && title_rows.len() > MAX_TITLE_ROWS {
        title_rows.truncate(MAX_TITLE_ROWS);
        let last = title_rows.pop().unwrap_or_default();
        title_rows.push(format!(
            "{}{mark}",
            text::clip(&last, room.saturating_sub(text::width(&mark)))
        ));
    }
    let title_style = theme.fg(Tok::ToolTitle);
    for (i, line) in title_rows.into_iter().enumerate() {
        let mut spans = vec![Span::styled(line.clone(), title_style)];
        if i == 0 {
            let gap = inner.saturating_sub(text::width(&line) + glyph_w);
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(glyph.clone(), glyph_style));
        }
        rows.push(row(spans));
    }

    let (body, hidden, label) = match diff_rows(t, inner, ctx).filter(|_| t.state == ToolState::Ok)
    {
        Some((body, hidden)) => (body, hidden, "Changes"),
        None => {
            let (body, hidden) = output_rows(t, inner, ctx);
            (body, hidden, "Output")
        }
    };
    if !body.is_empty() {
        rows.push(rule(
            theme.sym(Sym::TeeL),
            &format!("{h}{h} {label} "),
            theme.sym(Sym::TeeR),
        ));
        rows.extend(body.into_iter().map(|l| row(l.spans)));
        if hidden > 0 {
            rows.push(row(vec![hidden_hint(hidden, theme)]));
        }
    } else if hidden > 0 {
        rows.push(row(vec![hidden_hint(hidden, theme)]));
    }
    if let (Some(secs), true) = (t.elapsed, t.state != ToolState::Pending) {
        let (open, close) = if theme.ascii() {
            ("<", ">")
        } else {
            ("⟨", "⟩")
        };
        rows.push(row(vec![Span::styled(
            format!("{open}Wall: {}{close}", elapsed_label(secs)),
            theme.fg(Tok::Dim),
        )]));
    }
    rows.push(rule(theme.sym(Sym::BoxBl), "", theme.sym(Sym::BoxBr)));
    rows
}

fn compact_tool(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let (glyph, glyph_style) = tool_glyph(t, ctx);
    let mut spans = vec![
        Span::styled(glyph, glyph_style),
        Span::raw(" "),
        Span::styled(
            capitalize(if t.name.is_empty() { "tool" } else { &t.name }),
            theme.fg(Tok::ToolTitle),
        ),
    ];
    let title = tool_title(t);
    if !title.is_empty() {
        spans.push(Span::styled(format!(" {title}"), theme.fg(Tok::ToolOutput)));
    }
    if t.state == ToolState::Ok {
        let clean = text::sanitize(&t.output);
        let footer = |l: &str| {
            l == "(no output)"
                || l.starts_with("[exit code: 0]")
                || l.starts_with("[complete output:")
        };
        if let Some(first) = clean
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !footer(l))
        {
            spans.push(Span::styled(
                format!("  {} {first}", theme.sym(Sym::Dot)),
                theme.fg(Tok::Dim),
            ));
        }
    }
    let mut rows = vec![fitted(spans, w, theme)];
    if t.state == ToolState::Err {
        let indent = 2.min(w.saturating_sub(1));
        let (body, hidden) = output_rows(t, w.saturating_sub(indent).max(1), ctx);
        for line in body {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.extend(line.spans);
            rows.push(fitted(spans, w, theme));
        }
        if hidden > 0 {
            rows.push(fitted(
                vec![Span::raw(" ".repeat(indent)), hidden_hint(hidden, theme)],
                w,
                theme,
            ));
        }
    }
    rows
}

// ---------------------------------------------------------------------------------------------
// the transcript

pub struct Transcript {
    blocks: Vec<Block>,
    /// First block whose rows have not been handed out through `take_settled`.
    cursor: usize,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Transcript {
            blocks: Vec::new(),
            cursor: 0,
        }
    }

    /// A streaming reasoning/answer block ends as soon as anything else happens.
    fn close_streaming_text(&mut self) {
        for block in self.blocks[self.cursor..].iter_mut() {
            if block.active && block.prose().is_some() {
                block.active = false;
            }
        }
    }

    fn push(&mut self, kind: Kind, active: bool) {
        self.blocks.push(Block::new(kind, active));
    }

    pub fn push_user(&mut self, text: &str) {
        self.close_streaming_text();
        self.push(Kind::User(text.to_owned()), false);
    }

    pub fn assistant_delta(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(Block {
            kind: Kind::Assistant(buf),
            active: true,
            ..
        }) = self.blocks.last_mut()
        {
            buf.push_str(text);
            return;
        }
        self.close_streaming_text();
        self.push(Kind::Assistant(text.to_owned()), true);
    }

    pub fn thinking_delta(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(Block {
            kind: Kind::Thinking(buf),
            active: true,
            ..
        }) = self.blocks.last_mut()
        {
            buf.push_str(text);
            return;
        }
        self.close_streaming_text();
        self.push(Kind::Thinking(text.to_owned()), true);
    }

    pub fn tool_start(&mut self, id: &str, name: &str, summary: &str, input: &str) {
        self.close_streaming_text();
        self.push(
            Kind::Tool(Tool {
                id: id.to_owned(),
                name: name.to_owned(),
                summary: summary.to_owned(),
                input: input.to_owned(),
                state: ToolState::Pending,
                output: String::new(),
                elapsed: None,
                todo: None,
            }),
            true,
        );
    }

    /// Append a live chunk to a pending card; full output is retained separately in artifacts.
    pub fn tool_delta(&mut self, id: &str, text: &str) {
        let Some(tool) =
            self.blocks[self.cursor..]
                .iter_mut()
                .rev()
                .find_map(|b| match &mut b.kind {
                    Kind::Tool(t) if t.id == id && t.state == ToolState::Pending => Some(t),
                    _ => None,
                })
        else {
            return;
        };
        tool.output.push_str(text);
        const LIMIT: usize = 16 * 1024;
        if tool.output.len() > LIMIT {
            let mut cut = tool.output.len() - LIMIT;
            while !tool.output.is_char_boundary(cut) {
                cut += 1;
            }
            tool.output.drain(..cut);
        }
    }

    /// Attach the task list a pending `todo` call produced, so its card shows what changed.
    pub fn tool_todo(&mut self, id: &str, tasks: Vec<(crate::agent::todo::Task, bool)>) {
        if let Some(tool) =
            self.blocks[self.cursor..]
                .iter_mut()
                .rev()
                .find_map(|b| match &mut b.kind {
                    Kind::Tool(t) if t.id == id && t.state == ToolState::Pending => Some(t),
                    _ => None,
                })
        {
            tool.todo = Some(tasks);
        }
    }

    pub fn tool_end(&mut self, id: &str, output: &str, is_error: bool, elapsed_secs: f64) {
        let state = if is_error {
            ToolState::Err
        } else {
            ToolState::Ok
        };
        let found = self.blocks[self.cursor..]
            .iter_mut()
            .rev()
            .find_map(|b| match &mut b.kind {
                Kind::Tool(t) if t.id == id && t.state == ToolState::Pending => {
                    Some((&mut b.active, t))
                }
                _ => None,
            });
        match found {
            Some((active, tool)) => {
                tool.state = state;
                tool.output = output.to_owned();
                tool.elapsed = Some(elapsed_secs);
                *active = false;
            }
            None => {
                self.close_streaming_text();
                self.push(
                    Kind::Tool(Tool {
                        id: id.to_owned(),
                        name: "tool".into(),
                        summary: String::new(),
                        input: String::new(),
                        state,
                        output: output.to_owned(),
                        elapsed: Some(elapsed_secs),
                        todo: None,
                    }),
                    false,
                );
            }
        }
    }

    pub fn notice(&mut self, title: &str, text: &str) {
        self.close_streaming_text();
        self.push(
            Kind::Notice {
                title: title.to_owned(),
                text: text.to_owned(),
            },
            false,
        );
    }

    pub fn error(&mut self, text: &str) {
        self.close_streaming_text();
        self.push(Kind::Error(text.to_owned()), false);
    }

    pub fn turn_stats(&mut self, stats: TurnStats) {
        self.close_streaming_text();
        self.push(Kind::Stats(stats), false);
    }

    /// Finalize every streaming block; a tool that never reported back is shown as interrupted.
    pub fn finish_turn(&mut self) {
        for block in self.blocks[self.cursor..].iter_mut() {
            if let Kind::Tool(t) = &mut block.kind
                && t.state == ToolState::Pending
            {
                t.state = ToolState::Err;
                if t.output.is_empty() {
                    t.output = "interrupted".into();
                }
            }
            block.active = false;
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.blocks[self.cursor..].iter().any(|b| b.active)
    }

    /// Rows of finalized blocks not handed out yet, each row exactly once; stops at the first block
    /// that is still active. Every block that has rows ends with one blank separator row. A block that
    /// was partly spilled by [`live`](Self::live) contributes only its remainder.
    pub fn take_settled(&mut self, width: u16, ctx: &TranscriptCtx) -> Rows {
        let mut out = Vec::new();
        while let Some(block) = self.blocks.get_mut(self.cursor) {
            if block.active {
                break;
            }
            let rows = block.render(width, ctx);
            let skip = block.emitted(width, ctx);
            let had_rows = !rows.is_empty() || skip > 0;
            out.extend(rows.into_iter().skip(skip));
            if had_rows {
                out.push(blank_row());
            }
            self.cursor += 1;
        }
        out
    }

    /// `(spilled, live)`: rows that must be committed to scrollback now (the stable head of the
    /// streaming block when the tail is taller than `max_rows`, each row returned once) and the rows
    /// to repaint, at most `max_rows`.
    pub fn live(&mut self, width: u16, ctx: &TranscriptCtx, max_rows: usize) -> (Rows, Rows) {
        let Some((first, rest)) = self.blocks[self.cursor..].split_first_mut() else {
            return (Vec::new(), Vec::new());
        };
        let head = first.render(width, ctx);
        let mut skip = first.emitted(width, ctx);
        let had_head = !head.is_empty() || skip > 0;
        let rest_rows: Vec<Rows> = rest.iter().map(|b| b.render(width, ctx)).collect();

        let assemble = |head: &Rows, skip: usize| -> Rows {
            let mut out: Rows = head.iter().skip(skip).cloned().collect();
            let mut any = had_head;
            for piece in &rest_rows {
                if piece.is_empty() {
                    continue;
                }
                if any {
                    out.push(blank_row());
                }
                out.extend(piece.iter().cloned());
                any = true;
            }
            out
        };

        let mut display = assemble(&head, skip);
        let mut spilled = Vec::new();
        if display.len() > max_rows
            && first.active
            && let Some((src, rows)) = first.spill_point(width, ctx, &head, skip)
        {
            spilled = head[skip..rows].to_vec();
            first.spill = Spill {
                src,
                rows,
                key: Some((width, ctx.ui.show_thinking)),
            };
            skip = rows;
            display = assemble(&head, skip);
        }
        if display.len() > max_rows {
            display.drain(..display.len() - max_rows);
        }
        (spilled, display)
    }

    /// Everything rendered from scratch (settled, spilled and active), for a display-reset replay.
    pub fn all_rows(&self, width: u16, ctx: &TranscriptCtx) -> Rows {
        let mut out = Vec::new();
        for block in &self.blocks {
            let rows = block.render(width, ctx);
            if !rows.is_empty() {
                out.extend(rows);
                out.push(blank_row());
            }
        }
        out
    }

    /// What has already been handed to scrollback (settled blocks plus the spilled head of the
    /// streaming one), rendered afresh at `width`. After a display reset, commit this and keep
    /// using `take_settled` / `live` unchanged: nothing is repeated and nothing is lost.
    pub fn committed_rows(&self, width: u16, ctx: &TranscriptCtx) -> Rows {
        let mut out = Vec::new();
        for (i, block) in self.blocks.iter().enumerate() {
            if i > self.cursor {
                break;
            }
            let rows = block.render(width, ctx);
            if i < self.cursor {
                if !rows.is_empty() {
                    out.extend(rows);
                    out.push(blank_row());
                }
            } else if block.spill.src > 0 {
                let n = match block.spill.key {
                    Some(key) if key == (width, ctx.ui.show_thinking) => block.spill.rows,
                    _ => block.measure_prefix(width, ctx),
                };
                out.extend(rows.into_iter().take(n));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Glyphs;

    #[test]
    fn streaming_prefix_matches_full_render_at_every_delta() {
        for source in [
            "Hello **world**.\n\nAnother paragraph.\n\nFinal.",
            "- first\n- second\n\nParagraph\n\nNext",
            "> quoted\n>\n> more\n\nParagraph",
            "```rust\nlet n = 1;\n\n// comment\n```\n\nAfter.\n\nMore",
            "| A | B |\n|---|---|\n| 1 | 2 |\n\nParagraph\n\nNext",
            "A [reference][id].\n\n[id]: https://example.com\n",
            "Plain prefix\n\nAnother paragraph\n\n[link]: https://example.com\n",
            "日本語 text\n\nAnother é paragraph",
        ] {
            for width in [20, 60] {
                let mut cache = StreamingRows::default();
                let theme = Theme::dark();
                for end in (0..=source.len()).filter(|&n| source.is_char_boundary(n)) {
                    assert_eq!(
                        cache.render(&source[..end], width, &theme),
                        markdown::render(&source[..end], width, &theme, false),
                        "at {end}, width {width}: {source:?}"
                    );
                }
            }
        }
        let source = "A **styled prefix**.\n\nAnother paragraph.\n\nFinal.";
        let mut cache = StreamingRows::default();
        cache.render(source, 20, &Theme::dark());
        for theme in [Theme::light(), Theme::mono()] {
            for width in [10, 80] {
                assert_eq!(
                    cache.render(source, width, &theme),
                    markdown::render(source, width, &theme, false)
                );
            }
        }
    }

    #[test]
    #[ignore = "manual streaming rendering measurement"]
    fn streaming_render_measurement() {
        let paragraph = "A streamed answer with **styled words**, a number 42, and ordinary prose that wraps across terminal rows.\n\n";
        let source = paragraph.repeat(200);
        let source = &source[..20_000];
        let theme = Theme::dark();
        let before = std::time::Instant::now();
        for end in (40..=source.len()).step_by(40) {
            std::hint::black_box(markdown::render(&source[..end], 100, &theme, false));
        }
        let full = before.elapsed();
        let mut cache = StreamingRows::default();
        let before = std::time::Instant::now();
        for end in (40..=source.len()).step_by(40) {
            std::hint::black_box(cache.render(&source[..end], 100, &theme));
        }
        eprintln!(
            "20KB / 40-byte deltas: full={full:?}, cached={:?}",
            before.elapsed()
        );
    }

    struct Env {
        theme: Theme,
        ui: UiConfig,
    }

    impl Env {
        fn new() -> Self {
            Env {
                theme: Theme::dark(),
                ui: UiConfig::default(),
            }
        }
        fn ctx(&self) -> TranscriptCtx<'_> {
            TranscriptCtx {
                theme: &self.theme,
                ui: &self.ui,
                tick: 0,
                tools_expanded: false,
            }
        }
    }

    fn plain(rows: &Rows) -> Vec<String> {
        rows.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn text_of(rows: &Rows) -> String {
        plain(rows).join("\n")
    }

    fn assert_fits(rows: &Rows, w: u16) {
        for row in rows {
            assert!(
                text::line_width(row) <= w as usize,
                "{:?} exceeds {w}",
                plain(&vec![row.clone()])
            );
        }
    }

    fn bgs(row: &Line<'static>) -> Vec<Option<ratatui::style::Color>> {
        row.spans.iter().map(|s| s.style.bg).collect()
    }

    // ---- lifecycle ----

    #[test]
    fn settled_blocks_are_handed_out_once_in_order() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.push_user("hello");
        t.notice("Note", "first");
        t.error("second");
        let rows = t.take_settled(40, &ctx);
        let text = text_of(&rows);
        assert!(text.find("hello").unwrap() < text.find("first").unwrap());
        assert!(text.find("first").unwrap() < text.find("second").unwrap());
        assert!(
            rows.last().unwrap().spans.is_empty(),
            "block ends with a blank separator"
        );
        assert!(t.take_settled(40, &ctx).is_empty());
    }

    #[test]
    fn every_block_ends_with_exactly_one_blank_row() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.notice("", "one");
        t.notice("", "two");
        assert_eq!(plain(&t.take_settled(20, &ctx)), ["one", "", "two", ""]);
    }

    #[test]
    fn active_block_holds_back_everything_after_it() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.tool_start("a", "bash", "sleep 1", "");
        t.notice("", "after");
        assert!(t.is_streaming());
        assert!(
            t.take_settled(40, &ctx).is_empty(),
            "the pending tool blocks the notice"
        );
        t.tool_end("a", "done", false, 1.0);
        let text = text_of(&t.take_settled(40, &ctx));
        assert!(text.find("sleep 1").unwrap() < text.find("after").unwrap());
        assert!(!t.is_streaming());
    }

    #[test]
    fn assistant_streams_into_one_block_and_rerenders_as_it_grows() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.assistant_delta("Hello **wor");
        let (spilled, live) = t.live(40, &ctx, 20);
        assert!(spilled.is_empty());
        assert_eq!(plain(&live), ["Hello **wor"]);
        t.assistant_delta("ld** again");
        let (_, live) = t.live(40, &ctx, 20);
        assert_eq!(plain(&live), ["Hello world again"]);
        assert!(
            live[0]
                .spans
                .iter()
                .any(|s| s.content.contains("world")
                    && s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(t.take_settled(40, &ctx).is_empty(), "still streaming");
        t.finish_turn();
        assert_eq!(plain(&t.take_settled(40, &ctx)), ["Hello world again", ""]);
    }

    #[test]
    fn a_new_kind_of_block_ends_the_streaming_one() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.thinking_delta("pondering");
        t.assistant_delta("answer");
        let settled = text_of(&t.take_settled(40, &ctx));
        assert!(settled.contains("pondering") && !settled.contains("answer"));
        t.tool_start("1", "read", "a.txt", "");
        let mid = text_of(&t.take_settled(40, &ctx));
        assert!(
            mid.contains("answer") && !mid.contains("Read"),
            "the pending tool blocks only itself: {mid}"
        );
        t.tool_end("1", "x", false, 0.1);
        assert!(text_of(&t.take_settled(40, &ctx)).contains("Read"));
    }

    #[test]
    fn finish_turn_marks_unfinished_tools_interrupted() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.tool_start("x", "bash", "sleep 100", "");
        t.finish_turn();
        let text = text_of(&t.take_settled(40, &ctx));
        assert!(text.contains("interrupted"), "{text}");
    }

    // ---- user / reasoning / stats / errors ----

    #[test]
    fn user_block_is_a_full_width_tinted_block_without_a_label() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.push_user("hello there, this is a fairly long message that wraps");
        let rows = t.take_settled(20, &ctx);
        let body: Vec<_> = rows.iter().take(rows.len() - 1).collect();
        assert!(body.len() >= 4);
        let tint = env.theme.bg(Tok::UserBg).bg;
        for row in &body {
            assert_eq!(text::line_width(row), 20);
            assert!(row.spans.iter().all(|s| s.style.bg == tint));
        }
        assert!(plain(&rows)[1].starts_with(" hello"));
        assert!(!text_of(&rows).to_lowercase().contains("you"));
    }

    #[test]
    fn user_text_is_sanitized() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.push_user("hi \x1b]0;pwned\x07 there\x1b[2J");
        let text = text_of(&t.take_settled(30, &ctx));
        assert!(text.contains("hi  there") && !text.contains("pwned") && !text.contains('\x1b'));
    }

    #[test]
    fn reasoning_is_muted_italic_prose_or_collapses() {
        let mut env = Env::new();
        let mut t = Transcript::new();
        t.thinking_delta("first thought\nsecond thought\n\nthird");
        t.finish_turn();
        let rows = t.all_rows(40, &env.ctx());
        assert!(plain(&rows).contains(&"first thought".to_owned()));
        assert_eq!(rows[0].spans[0].style, env.theme.fg(Tok::ThinkingText));
        env.ui.show_thinking = false;
        let rows = t.all_rows(40, &env.ctx());
        assert_eq!(plain(&rows), ["› reasoning (3 lines)", ""]);
    }

    #[test]
    fn stats_row_is_dim_and_gated_by_setting() {
        let mut env = Env::new();
        let mut t = Transcript::new();
        t.turn_stats(TurnStats {
            clock: "2026-10-01 10:54:32".into(),
            elapsed_secs: 26.84,
            prompt_tokens: Some(11_000),
            output_tokens: Some(59),
            tokens_per_second: Some(2.2),
        });
        let rows = t.all_rows(80, &env.ctx());
        let line = &plain(&rows)[0];
        assert!(
            line.starts_with("2026-10-01 10:54:32  Δ 26.8s  ↑11K ↓59"),
            "{line}"
        );
        assert!(line.contains("2.2/s"), "{line}");
        assert_eq!(rows[0].spans[0].style, env.theme.fg(Tok::Dim));
        env.ui.show_turn_stats = false;
        assert!(t.all_rows(80, &env.ctx()).is_empty());
        let mut t2 = Transcript::new();
        t2.turn_stats(TurnStats {
            clock: String::new(),
            elapsed_secs: 1.0,
            prompt_tokens: None,
            output_tokens: None,
            tokens_per_second: None,
        });
        env.ui.show_turn_stats = true;
        assert_eq!(plain(&t2.all_rows(80, &env.ctx()))[0], "Δ 1.0s");
    }

    #[test]
    fn errors_and_notices_wrap_with_hanging_indent() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.error("connection refused while talking to the model server");
        t.notice("Heads up", "something noteworthy happened here");
        let rows = t.all_rows(24, &env.ctx());
        assert_fits(&rows, 24);
        let text = plain(&rows);
        assert!(text[0].starts_with("✗ connection"));
        assert!(text[1].starts_with("  "));
        assert_eq!(rows[0].spans[0].style.fg, env.theme.fg(Tok::Error).fg);
        assert!(text.iter().any(|l| l.starts_with("● Heads up")));
    }

    // ---- tools ----

    #[test]
    fn bash_tool_is_a_tinted_box_with_title_output_divider_and_footer() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "ls -la", "");
        t.tool_end("1", "total 0\nfile.txt", false, 0.06);
        let rows = t.take_settled(40, &ctx);
        let text = plain(&rows);
        assert!(text[0].starts_with('╭') && text[0].ends_with('╮'));
        assert!(text[1].contains("$ ls -la") && text[1].contains('✓'));
        assert!(text[2].starts_with("├── Output ") && text[2].ends_with('┤'));
        assert_eq!(text[3], format!("│ {} │", text::pad("total 0", 36)));
        assert!(text[5].contains("⟨Wall: 0.06s⟩"), "{text:?}");
        assert!(text[6].starts_with('╰') && text[6].ends_with('╯'));
        assert!(rows.iter().take(7).all(|r| text::line_width(r) == 40));
        let tint = env.theme.bg(Tok::ToolSuccessBg).bg;
        assert!(rows[3].spans.iter().any(|s| s.style.bg == tint));
    }

    #[test]
    fn tool_tint_follows_state() {
        let env = Env::new();
        let render = |end: Option<bool>| {
            let mut t = Transcript::new();
            t.tool_start("1", "bash", "x", "");
            if let Some(err) = end {
                t.tool_end("1", "out", err, 1.0);
            }
            t.all_rows(40, &env.ctx())
        };
        let pending = render(None);
        let ok = render(Some(false));
        let err = render(Some(true));
        let tint = |rows: &Rows| bgs(&rows[1]).into_iter().flatten().next();
        assert_eq!(tint(&pending), env.theme.bg(Tok::ToolPendingBg).bg);
        assert_eq!(tint(&ok), env.theme.bg(Tok::ToolSuccessBg).bg);
        assert_eq!(tint(&err), env.theme.bg(Tok::ToolErrorBg).bg);
        assert!(plain(&err)[1].contains('✗'));
        let body = err
            .iter()
            .flat_map(|r| &r.spans)
            .find(|s| s.content.contains("out"))
            .unwrap();
        assert_eq!(body.style.fg, env.theme.fg(Tok::Error).fg);
    }

    #[test]
    fn pending_spinner_animates_with_tick() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "build", "");
        t.tool_start("2", "read", "a.rs", "");
        let frame = |tick: u64| {
            let ctx = TranscriptCtx { tick, ..env.ctx() };
            plain(&t.all_rows(40, &ctx))
        };
        let (a, b) = (frame(0), frame(1));
        assert_ne!(a, b);
        assert!(a[1].contains(env.theme.spinner(0)) && b[1].contains(env.theme.spinner(1)));
        let compact = a.iter().find(|l| l.contains("Read")).unwrap();
        assert!(compact.starts_with(env.theme.spinner(0)), "{compact}");
        assert!(
            b.iter()
                .any(|l| l.starts_with(env.theme.spinner(1)) && l.contains("Read"))
        );
    }

    #[test]
    fn long_output_is_capped_with_a_hint_until_expanded() {
        let env = Env::new();
        let mut t = Transcript::new();
        let output = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        t.tool_start("1", "bash", "seq 20", "");
        t.tool_end("1", &output, false, 2.0);
        let capped = text_of(&t.all_rows(50, &env.ctx()));
        assert!(
            capped.contains("line 8") && !capped.contains("line 9"),
            "{capped}"
        );
        assert!(
            capped.contains("… 12 more lines (Ctrl-O expands)"),
            "{capped}"
        );
        let expanded = text_of(&t.all_rows(
            50,
            &TranscriptCtx {
                tools_expanded: true,
                ..env.ctx()
            },
        ));
        assert!(expanded.contains("line 20") && !expanded.contains("more lines"));
    }

    #[test]
    fn output_lines_setting_controls_the_cap() {
        let mut env = Env::new();
        env.ui.tool_output_lines = 2;
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "x", "");
        t.tool_end("1", "a\nb\nc", false, 1.0);
        let text = text_of(&t.all_rows(40, &env.ctx()));
        assert!(
            text.contains(" b ") && !text.contains(" c ") && text.contains("1 more line (Ctrl-O"),
            "{text}"
        );
    }

    #[test]
    fn edit_output_colors_diff_lines_and_hashline_previews() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "edit", "src/main.rs", "");
        t.tool_end("1", "[src/main.rs#AB12]\nupdated (+1 -1 lines)\n@@ -1 +1 @@\n-old line\n+new line\n3:context", false, 0.5);
        let rows = t.all_rows(60, &env.ctx());
        assert!(plain(&rows)[1].contains("src/main.rs") && !plain(&rows)[1].contains('$'));
        let style = |needle: &str| {
            rows.iter()
                .flat_map(|r| &r.spans)
                .find(|s| s.content.contains(needle))
                .unwrap()
                .style
                .fg
        };
        assert_eq!(style("+new line"), env.theme.fg(Tok::DiffAdded).fg);
        assert_eq!(style("-old line"), env.theme.fg(Tok::DiffRemoved).fg);
        assert_eq!(style("#AB12"), env.theme.fg(Tok::Accent).fg);
        assert_eq!(style("3:"), env.theme.fg(Tok::Dim).fg);
        assert_eq!(style("context"), env.theme.fg(Tok::ToolOutput).fg);
        assert!(text_of(&rows).contains("Wall: 0.50s"));
    }

    #[test]
    fn edit_cards_show_the_numbered_diff_not_the_model_facing_preview() {
        let env = Env::new();
        let mut t = Transcript::new();
        let patch = "[src/a.rs#2F12]\nPUT 2.=2:\n+    new();\n";
        t.tool_start("1", "edit", patch, patch);
        let output = "[src/a.rs#EF7D]\nupdated (+1 -1 lines)\nremoved lines not present in your new text (old numbering):\n  2:     old();\n1:fn a() {\n2:    new();\n3:}\n\nchange preview (+1 -1 lines)\n@@ -1,3 +1,3 @@\n fn a() {\n-    old();\n+    new();\n }\n";
        t.tool_end("1", output, false, 0.1);
        let rows = t.all_rows(60, &env.ctx());
        let text = plain(&rows);
        assert!(
            text[1].contains("src/a.rs") && !text[1].contains("PUT"),
            "titled by the file it touches: {text:?}"
        );
        let body: Vec<&str> = text
            .iter()
            .map(|r| r.trim_matches(|c: char| c == '│' || c == ' '))
            .collect();
        for expected in [
            "src/a.rs",
            "updated (+1 -1 lines)",
            "1  fn a() {",
            "2 -    old();",
            "2 +    new();",
            "3  }",
        ] {
            assert!(
                body.contains(&expected),
                "{expected:?} missing from {body:?}"
            );
        }
        assert!(
            !text_of(&rows).contains("removed lines not present")
                && !text_of(&rows).contains("1:fn a()"),
            "model-facing parts are hidden"
        );
        let style = |needle: &str| {
            rows.iter()
                .flat_map(|r| &r.spans)
                .find(|s| s.content.contains(needle))
                .unwrap()
                .style
                .fg
        };
        assert_eq!(style("+    new();"), env.theme.fg(Tok::DiffAdded).fg);
        assert_eq!(style("-    old();"), env.theme.fg(Tok::DiffRemoved).fg);
        assert!(text_of(&rows).contains("Changes"));
    }

    #[test]
    fn todo_calls_render_the_list_with_their_changes() {
        use crate::agent::todo::TodoList;
        let env = Env::new();
        let mut before = TodoList::new();
        before.call(
            &serde_json::json!({"op": "set", "tasks": [{"content": "read"}, {"content": "fix"}]}),
        );
        let mut after = before.clone();
        after.call(&serde_json::json!({"op": "done"}));
        let mut t = Transcript::new();
        t.tool_start("1", "todo", "done", "");
        t.tool_todo("1", crate::ui::tasks::changes(&before, &after));
        t.tool_end(
            "1",
            "Completed 1. read. Started 2. fix.\n1/2 done\n[x] 1. read\n[~] 2. fix",
            false,
            0.0,
        );
        assert_eq!(
            plain(&t.all_rows(60, &env.ctx()))[..3],
            ["✓ Todo done  · 1/2 done", "  ✓ read", "  ● fix"]
        );
    }

    #[test]
    fn write_error_output_is_not_colored_as_a_diff() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "write", "a.txt", "");
        t.tool_end("1", "-bad: refused", true, 0.1);
        let rows = t.all_rows(40, &env.ctx());
        let span = rows
            .iter()
            .flat_map(|r| &r.spans)
            .find(|s| s.content.contains("-bad"))
            .unwrap();
        assert_eq!(span.style.fg, env.theme.fg(Tok::Error).fg);
    }

    #[test]
    fn title_falls_back_to_the_call_arguments() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "", r#"{"command":"echo hi"}"#);
        t.tool_start("2", "write", "", r#"{"path":"out/a.txt"}"#);
        let text = text_of(&t.all_rows(50, &env.ctx()));
        assert!(
            text.contains("$ echo hi") && text.contains("out/a.txt"),
            "{text}"
        );
    }

    #[test]
    fn long_commands_wrap_in_the_title_and_are_clipped_to_four_rows() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "bash", &"word ".repeat(80), "");
        let rows = t.all_rows(30, &env.ctx());
        assert_fits(&rows, 30);
        // top, 4 title rows, bottom (+ blank)
        assert_eq!(rows.len(), 7, "{:?}", plain(&rows));
        assert!(plain(&rows)[4].contains('…'));
    }

    #[test]
    fn simple_tools_are_compact_rows() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "read", "notes.txt", "");
        t.tool_end("1", "\nfirst real line\nsecond", false, 0.2);
        let rows = t.all_rows(60, &env.ctx());
        assert_eq!(plain(&rows), ["✓ Read notes.txt  · first real line", ""]);
        assert_eq!(rows[0].spans[0].style.fg, env.theme.fg(Tok::Success).fg);
        assert_eq!(rows[0].spans[2].style.fg, env.theme.fg(Tok::ToolTitle).fg);
    }

    #[test]
    fn failed_simple_tool_shows_its_error_underneath() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "grep", "foo", "");
        t.tool_end("1", "regex parse error\nunclosed group", true, 0.2);
        let text = plain(&t.all_rows(40, &env.ctx()));
        assert_eq!(text[0], "✗ Grep foo");
        assert_eq!(text[1], "  regex parse error");
        assert_eq!(text[2], "  unclosed group");
    }

    #[test]
    fn show_tools_false_hides_everything_but_errors() {
        let mut env = Env::new();
        env.ui.show_tools = false;
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "ok-cmd", "");
        t.tool_end("1", "fine", false, 0.1);
        t.tool_start("2", "read", "hidden.txt", "");
        t.tool_end("2", "x", false, 0.1);
        t.tool_start("3", "bash", "bad-cmd", "");
        t.tool_end("3", "boom", true, 0.1);
        t.tool_start("4", "read", "missing.txt", "");
        t.tool_end("4", "no such file", true, 0.1);
        t.tool_start("5", "bash", "still-running", "");
        let text = text_of(&t.all_rows(50, &env.ctx()));
        for hidden in ["ok-cmd", "hidden.txt", "still-running"] {
            assert!(!text.contains(hidden), "{hidden} should be hidden\n{text}");
        }
        for shown in ["bad-cmd", "boom", "missing.txt", "no such file"] {
            assert!(text.contains(shown), "{shown} should be visible\n{text}");
        }
    }

    #[test]
    fn hidden_tools_leave_no_stray_blank_rows() {
        let mut env = Env::new();
        env.ui.show_tools = false;
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.notice("", "a");
        t.tool_start("1", "read", "x", "");
        t.tool_end("1", "y", false, 0.1);
        t.notice("", "b");
        assert_eq!(plain(&t.take_settled(20, &ctx)), ["a", "", "b", ""]);
    }

    #[test]
    fn tool_end_for_unknown_id_is_not_lost() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_end("zzz", "orphan output", false, 0.3);
        assert!(text_of(&t.all_rows(40, &env.ctx())).contains("orphan output"));
    }

    #[test]
    fn parallel_tools_pair_results_by_id() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("a", "bash", "first", "");
        t.tool_start("b", "bash", "second", "");
        t.tool_end("b", "B-out", false, 0.1);
        t.tool_end("a", "A-out", false, 0.1);
        let text = text_of(&t.all_rows(40, &env.ctx()));
        assert!(text.find("first").unwrap() < text.find("A-out").unwrap());
        assert!(text.find("A-out").unwrap() < text.find("second").unwrap());
        assert!(text.find("second").unwrap() < text.find("B-out").unwrap());
    }

    #[test]
    fn narrow_widths_never_overflow() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.push_user("日本語 hello world");
        t.thinking_delta("hmm 🎉 thinking");
        t.assistant_delta("# Head\n\n| a | b |\n|--|--|\n| 1 | 2 |\n\n```rust\nlet x = 1;\n```\n");
        t.tool_start("1", "bash", "echo 日本語", "");
        t.tool_end("1", "日本語日本語\nline", false, 1.0);
        t.tool_start("2", "read", "path/to/some/file.rs", "");
        t.tool_end("2", "x", true, 1.0);
        t.notice("title", "body text");
        t.error("an error");
        t.turn_stats(TurnStats {
            clock: "now".into(),
            elapsed_secs: 1.0,
            prompt_tokens: Some(1),
            output_tokens: Some(2),
            tokens_per_second: Some(3.0),
        });
        t.finish_turn();
        for w in 1..=80u16 {
            assert_fits(&t.all_rows(w, &env.ctx()), w);
        }
        let ascii = Theme::dark().with_glyphs(Glyphs::Ascii);
        let ctx = TranscriptCtx {
            theme: &ascii,
            ..env.ctx()
        };
        for row in plain(&t.all_rows(60, &ctx)) {
            assert!(
                row.chars()
                    .all(|c| c.is_ascii() || text::width(&c.to_string()) == 2),
                "{row:?}"
            );
        }
    }

    // ---- spill ----

    fn long_markdown(paragraphs: usize) -> Vec<String> {
        (1..=paragraphs)
            .map(|i| format!("Paragraph <{i}> has some words in it that wrap around.\n\n"))
            .collect()
    }

    #[test]
    fn nothing_spills_while_the_tail_fits() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.assistant_delta("one\n\ntwo\n\nthree");
        let (spilled, live) = t.live(40, &ctx, 20);
        assert!(spilled.is_empty());
        assert_eq!(plain(&live), ["one", "", "two", "", "three"]);
    }

    #[test]
    fn stable_head_spills_once_and_the_remainder_completes_it() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        let mut committed: Rows = Vec::new();
        for chunk in long_markdown(12) {
            t.assistant_delta(&chunk);
            committed.extend(t.take_settled(30, &ctx));
            let (spilled, live) = t.live(30, &ctx, 6);
            assert!(live.len() <= 6);
            committed.extend(spilled);
        }
        assert!(committed.len() > 6, "something must have spilled");
        // spilled paragraphs never reappear in the live tail
        let (_, live) = t.live(30, &ctx, 6);
        let committed_text = text_of(&committed);
        for i in 1..=12 {
            let token = format!("<{i}>");
            assert!(
                !(text_of(&live).contains(&token) && committed_text.contains(&token)),
                "{token} shown twice"
            );
        }
        t.finish_turn();
        committed.extend(t.take_settled(30, &ctx));
        assert_eq!(
            plain(&committed),
            plain(&t.all_rows(30, &ctx)),
            "spilled + settled == everything, once"
        );
    }

    #[test]
    fn spill_never_cuts_an_open_code_fence() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.assistant_delta("intro\n\nnext para\n\n```rust\nlet a = 1;\n\nlet b = 2;\n");
        let mut committed: Rows = Vec::new();
        let (spilled, live) = t.live(30, &ctx, 5);
        committed.extend(spilled);
        let shown = text_of(&committed);
        assert!(
            !shown.contains("let a"),
            "code inside an open fence is not stable: {shown}"
        );
        assert!(text_of(&live).contains("let b = 2;"));
        t.assistant_delta("```\n\nafter\n");
        t.finish_turn();
        committed.extend(t.take_settled(30, &ctx));
        assert_eq!(plain(&committed), plain(&t.all_rows(30, &ctx)));
    }

    #[test]
    fn list_followed_by_blank_is_not_spilled_until_the_next_block_starts() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.assistant_delta("- a\n- b\n\n");
        assert!(
            t.live(20, &ctx, 1).0.is_empty(),
            "the next item could still make the list loose"
        );
        t.assistant_delta("- c\n\nplain text\n");
        let mut committed = t.live(20, &ctx, 1).0;
        t.finish_turn();
        committed.extend(t.take_settled(20, &ctx));
        assert_eq!(plain(&committed), plain(&t.all_rows(20, &ctx)));
    }

    #[test]
    fn spill_is_exactly_once_across_many_frames() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        let mut committed: Rows = Vec::new();
        let doc: String = long_markdown(30).concat() + "tail";
        for piece in doc.as_bytes().chunks(7) {
            t.assistant_delta(std::str::from_utf8(piece).unwrap());
            let (spilled, live) = t.live(24, &ctx, 5);
            committed.extend(spilled);
            assert!(live.len() <= 5);
            // calling again with no new input must not spill the same rows again
            assert!(t.live(24, &ctx, 5).0.is_empty());
        }
        t.finish_turn();
        committed.extend(t.take_settled(24, &ctx));
        assert_eq!(plain(&committed), plain(&t.all_rows(24, &ctx)));
    }

    #[test]
    fn width_change_after_partial_spill_keeps_what_was_spilled() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        let doc = long_markdown(10).concat();
        t.assistant_delta(&doc);
        t.assistant_delta("the <unfinished> last paragraph with many words to wrap");
        let (spilled, _) = t.live(40, &ctx, 6);
        assert!(!spilled.is_empty());
        // narrower terminal: nothing already spilled is repeated, nothing is dropped
        let (more, live) = t.live(17, &ctx, 6);
        assert!(live.len() <= 6);
        let mut everything = spilled;
        everything.extend(more);
        t.finish_turn();
        everything.extend(t.take_settled(17, &ctx));
        let all = text_of(&everything);
        for i in 1..=10 {
            assert_eq!(
                all.matches(&format!("<{i}>")).count(),
                1,
                "paragraph {i} must appear exactly once:\n{all}"
            );
        }
        assert_eq!(all.matches("<unfinished>").count(), 1);
    }

    #[test]
    fn partially_spilled_block_is_not_duplicated_by_committed_rows_replay() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.push_user("question");
        let mut committed = t.take_settled(30, &ctx);
        t.assistant_delta(&long_markdown(10).concat());
        t.assistant_delta("still typing");
        committed.extend(t.live(30, &ctx, 5).0);
        let replay = t.committed_rows(30, &ctx);
        assert_eq!(plain(&replay), plain(&committed));
        assert!(!text_of(&replay).contains("still typing"));
    }

    #[test]
    fn pending_tool_before_stream_prevents_spilling_out_of_order() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "build", "");
        t.assistant_delta(&long_markdown(10).concat());
        let (spilled, live) = t.live(30, &ctx, 6);
        assert!(
            spilled.is_empty(),
            "the pending tool must be committed first"
        );
        assert!(live.len() <= 6);
    }

    #[test]
    fn live_with_zero_rows_or_nothing_active_is_empty() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        assert_eq!(t.live(30, &ctx, 10), (vec![], vec![]));
        t.assistant_delta("text");
        let (spilled, live) = t.live(30, &ctx, 0);
        assert!(spilled.is_empty() && live.is_empty());
    }

    #[test]
    fn long_reasoning_spills_like_prose() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        let mut committed = Rows::new();
        for i in 1..=15 {
            t.thinking_delta(&format!("thought number {i} goes on for a while\n\n"));
            committed.extend(t.live(26, &ctx, 4).0);
        }
        assert!(!committed.is_empty());
        t.finish_turn();
        committed.extend(t.take_settled(26, &ctx));
        assert_eq!(plain(&committed), plain(&t.all_rows(26, &ctx)));
    }
}
