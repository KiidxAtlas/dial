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
    tool_cards::{AgentRow, Tool, ToolState, tool_rows},
};
use crate::llm::model_config::UiConfig;
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use serde_json::Value;
use std::sync::Arc;

// Tool lifecycle and card data are shared with the specialized card renderer.

#[derive(Clone, Debug, PartialEq)]
pub struct TurnStats {
    /// Wall-clock time of the turn's end, already formatted (`2026-10-01 10:54:32`).
    pub clock: String,
    pub elapsed_secs: f64,
    pub prompt_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_per_second: Option<f64>,
    /// How the turn ended, when the agent reported it; filled from [`Transcript::completion`].
    pub outcome: Option<TurnOutcome>,
}

/// The agent's own account of how a turn ended.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    /// `completed`, `incomplete`, `failed`, `interrupted`, …
    pub status: String,
    pub changed_files: usize,
    pub open_tasks: usize,
}

#[derive(Clone, Copy)]
pub struct TranscriptCtx<'a> {
    pub theme: &'a Theme,
    pub ui: &'a UiConfig,
    /// Animation frame (~12 fps) for pending spinners.
    pub tick: u64,
    pub tools_expanded: bool,
    /// Label of the expand/collapse shortcut, as the keymap shows it (for example `Ctrl+O`).
    pub expand_key: &'a str,
}

enum Kind {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(Tool),
    Notice {
        title: String,
        text: String,
    },
    /// A typed controller event, drawn in its lane (context line, alert, or divider).
    Event(crate::agent::ui_event::UiEvent),
    Error(String),
    Image(super::images::Thumbnail),
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
    shared: Option<crate::runtime::Body>,
    revision: std::cell::Cell<u64>,
    rendered: std::cell::RefCell<Option<(RenderKey, u64, std::sync::Arc<Rows>)>>,
}

#[derive(PartialEq)]
struct RenderKey {
    width: u16,
    theme: Theme,
    thinking: bool,
    tools: bool,
    expanded: bool,
    tool_lines: usize,
    turn_stats: bool,
    revision: u64,
    shared_revision: u64,
    active: bool,
}

impl Block {
    fn new(kind: Kind, active: bool) -> Self {
        Block {
            kind,
            active,
            spill: Spill::default(),
            markdown: std::cell::RefCell::new(StreamingRows::default()),
            shared: None,
            revision: std::cell::Cell::new(0),
            rendered: std::cell::RefCell::new(None),
        }
    }

    fn prose(&self) -> Option<(Prose, std::borrow::Cow<'_, str>)> {
        match &self.kind {
            Kind::Assistant(t) => Some((
                Prose::Assistant,
                self.shared.as_ref().map_or_else(
                    || std::borrow::Cow::Borrowed(t.as_str()),
                    |b| std::borrow::Cow::Owned(b.read(|n| n.result.text.clone())),
                ),
            )),
            Kind::Thinking(t) => Some((
                Prose::Thinking,
                self.shared.as_ref().map_or_else(
                    || std::borrow::Cow::Borrowed(t.as_str()),
                    |b| std::borrow::Cow::Owned(b.read(|n| n.result.text.clone())),
                ),
            )),
            _ => None,
        }
    }

    fn render(&self, w: u16, ctx: &TranscriptCtx) -> std::sync::Arc<Rows> {
        let key = RenderKey {
            width: w,
            theme: ctx.theme.clone(),
            thinking: ctx.ui.show_thinking,
            tools: ctx.ui.show_tools,
            expanded: ctx.tools_expanded,
            tool_lines: ctx.ui.tool_output_lines,
            turn_stats: ctx.ui.show_turn_stats,
            revision: self.revision.get(),
            shared_revision: self
                .shared
                .as_ref()
                .map_or(0, crate::runtime::Body::revision),
            active: self.active,
        };
        {
            let mut slot = self.rendered.borrow_mut();
            if let Some((old, tick, rows)) = slot.as_mut()
                && *old == key
            {
                let spinning = matches!(&self.kind, Kind::Tool(t) if t.state == ToolState::Running);
                if spinning && *tick != ctx.tick {
                    // The cache is the only owner here, so the spinner glyph is patched in place.
                    let old_glyph = ctx.theme.spinner(*tick);
                    if let Some(span) = std::sync::Arc::make_mut(rows)
                        .iter_mut()
                        .flat_map(|r| r.spans.iter_mut())
                        .find(|s| s.content == old_glyph)
                    {
                        span.content = ctx.theme.spinner(ctx.tick).to_owned().into();
                    }
                    *tick = ctx.tick;
                }
                return std::sync::Arc::clone(rows);
            }
        }
        let rows = match &self.kind {
            Kind::User(t) => Arc::new(user_rows(t, w, ctx)),
            Kind::Assistant(t) => match &self.shared {
                Some(b) => b.read(|n| {
                    self.markdown
                        .borrow_mut()
                        .render(&n.result.text, w, ctx.theme)
                }),
                None => self.markdown.borrow_mut().render(t, w, ctx.theme),
            },
            Kind::Thinking(t) => Arc::new(match &self.shared {
                Some(b) => b.read(|n| prose_rows(Prose::Thinking, &n.result.text, w, ctx)),
                None => prose_rows(Prose::Thinking, t, w, ctx),
            }),
            Kind::Tool(t) => Arc::new(match &self.shared {
                Some(b) => b.read(|n| {
                    let mut view = t.clone();
                    view.output = n.result.text.clone();
                    if let Some(presentation) = &n.presentation {
                        view.presentation = presentation.clone();
                    }
                    view.state = n.result.phase;
                    tool_rows(&view, w, ctx)
                }),
                None => tool_rows(t, w, ctx),
            }),
            Kind::Notice { title, text } => Arc::new(super::notice::rows(title, text, w, ctx)),
            Kind::Event(event) => Arc::new(super::lanes::rows(event, w, ctx)),
            Kind::Error(t) => Arc::new(error_rows(t, w, ctx)),
            Kind::Image(image) => Arc::new(image.rows(w, ctx.theme)),
            Kind::Stats(s) => Arc::new(stats_rows(s, w, ctx)),
        };
        *self.rendered.borrow_mut() = Some((key, ctx.tick, Arc::clone(&rows)));
        rows
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
            let mut end = self.spill.src.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            prose_rows(kind, &text[..end], w, ctx).len()
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
        for &cut in stable_cuts(&text, self.spill.src).iter().rev().take(4) {
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
    /// `source[..cut]` as already rendered into `prefix`; a source that no longer starts with it
    /// (for example a settled body that differs from the streamed text) is rendered afresh.
    head: String,
    prefix: std::sync::Arc<Rows>,
}

impl StreamingRows {
    fn render(&mut self, source: &str, width: u16, theme: &Theme) -> std::sync::Arc<Rows> {
        if self
            .key
            .as_ref()
            .is_none_or(|(w, t)| *w != width || t != theme)
            || self.cut > source.len()
            || !source.starts_with(self.head.as_str())
        {
            self.key = Some((width, theme.clone()));
            self.cut = 0;
            self.head.clear();
            self.prefix = std::sync::Arc::new(Vec::new());
        }
        if source.lines().any(markdown::has_reference_definition) {
            self.cut = 0;
            self.head.clear();
            self.prefix = std::sync::Arc::new(Vec::new());
            return std::sync::Arc::new(markdown::render(source, width, theme, false));
        }
        if source[self.cut..].contains(['[', '<']) || width == 0 {
            let rendered = markdown::render(&source[self.cut..], width, theme, false);
            return join_rows(&self.prefix, rendered);
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
                // Grown in place: the prefix is only shared with rows already handed out.
                let prefix = std::sync::Arc::make_mut(&mut self.prefix);
                if !prefix.is_empty() {
                    prefix.push(blank_row());
                }
                prefix.extend(rows);
            }
            self.head.push_str(&tail[..cut]);
            self.cut += cut;
        }
        let tail = markdown::render(&source[self.cut..], width, theme, false);
        join_rows(&self.prefix, tail)
    }
}

/// `prefix` followed by `tail`, separated by a blank row when both are non-empty. Without a tail the
/// shared prefix is returned as is.
fn join_rows(prefix: &std::sync::Arc<Rows>, tail: Rows) -> std::sync::Arc<Rows> {
    if tail.is_empty() {
        return std::sync::Arc::clone(prefix);
    }
    if prefix.is_empty() {
        return std::sync::Arc::new(tail);
    }
    let mut rows = Vec::with_capacity(prefix.len() + 1 + tail.len());
    rows.extend(prefix.iter().cloned());
    rows.push(blank_row());
    rows.extend(tail);
    std::sync::Arc::new(rows)
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

pub(super) fn ellipsis(theme: &Theme) -> String {
    theme.sym(Sym::Ellipsis).to_owned()
}

pub(super) fn fitted(spans: Vec<Span<'static>>, w: usize, theme: &Theme) -> Line<'static> {
    Line::from(truncate_spans(spans, w, &ellipsis(theme)))
}

fn user_rows(raw: &str, w: u16, ctx: &TranscriptCtx) -> Rows {
    let w = w as usize;
    let clean = text::sanitize(raw);
    let body = clean.trim();
    if w == 0 || body.is_empty() {
        return Vec::new();
    }
    let theme = ctx.theme;
    let bar = Span::styled(theme.sym(Sym::Quote).to_owned(), theme.fg(Tok::Accent));
    let text_style = theme.fg(Tok::UserText);
    let mut rows = Vec::new();
    if w < 4 {
        let flat = body.replace('\n', " ");
        rows.push(fitted(
            vec![bar, Span::raw(" "), Span::styled(flat, text_style)],
            w,
            theme,
        ));
    } else {
        let inner = w - 4;
        for (i, line) in text::wrap_words(body, inner).into_iter().enumerate() {
            let marker = if i == 0 { theme.sym(Sym::Prompt) } else { " " };
            rows.push(Line::from(vec![
                bar.clone(),
                Span::raw(" "),
                Span::styled(format!("{marker} "), theme.fg(Tok::Accent)),
                Span::styled(text::clip(&line, inner), text_style),
            ]));
        }
    }
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
    let prefix = format!("{} ", ctx.theme.sym(Sym::Quote));
    let lead = text::width(&prefix);
    if w <= lead {
        return vec![fitted(
            vec![Span::styled(body.to_owned(), style)],
            w,
            ctx.theme,
        )];
    }
    text::wrap_words(body, w - lead)
        .into_iter()
        .map(|row| {
            Line::from(vec![
                Span::styled(prefix.clone(), ctx.theme.fg(Tok::Border)),
                Span::styled(text::clip(&row, w - lead), style),
            ])
        })
        .collect()
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

/// The turn's closing rule: how it ended, what it changed, how long it took and what it cost, with
/// the follow-up commands at the right edge when files changed.
fn stats_rows(s: &TurnStats, w: u16, ctx: &TranscriptCtx) -> Rows {
    let timing = ctx.ui.show_turn_stats;
    if w == 0 || (!timing && s.outcome.is_none()) {
        return Vec::new();
    }
    let theme = ctx.theme;
    let w = w as usize;
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let rule = theme.sym(Sym::BoxH).to_owned();
    let mut left: Vec<Span<'static>> = vec![Span::styled(
        format!("{rule}{rule} "),
        theme.fg(Tok::Border),
    )];
    let push = |left: &mut Vec<Span<'static>>, text: String, style| {
        if left.len() > 1 {
            left.push(Span::styled(dot.clone(), theme.fg(Tok::Dim)));
        }
        left.push(Span::styled(text, style));
    };
    let mut changed = 0;
    if let Some(o) = &s.outcome {
        let phase = crate::runtime::Phase::from_status(&o.status);
        let word = phase.label();
        let (tok, mark) = match phase {
            crate::runtime::Phase::Completed => (Tok::Success, Sym::Check),
            crate::runtime::Phase::Failed => (Tok::Error, Sym::Cross2),
            crate::runtime::Phase::Interrupted => (Tok::Muted, Sym::Cross2),
            crate::runtime::Phase::Running => (Tok::Accent, Sym::Working),
            _ => (Tok::Warning, Sym::Warn),
        };
        // The outcome leads with its mark so the end of a turn is findable at a glance.
        // ASCII marks (`+`, `x`) read as arithmetic in a rule, so the word stands alone there.
        let label = if theme.ascii() {
            word.to_owned()
        } else {
            format!("{} {word}", theme.sym(mark))
        };
        push(&mut left, label, theme.fg(tok).add_modifier(Modifier::BOLD));
        changed = o.changed_files;
        if changed > 0 {
            push(
                &mut left,
                format!(
                    "{changed} file{} changed",
                    if changed == 1 { "" } else { "s" }
                ),
                theme.fg(Tok::Text),
            );
        }
        if o.open_tasks > 0 {
            push(
                &mut left,
                format!("{} open tasks", o.open_tasks),
                theme.fg(Tok::Warning),
            );
        }
    }
    if timing {
        push(
            &mut left,
            format!("{:.1}s", s.elapsed_secs),
            theme.fg(Tok::Muted),
        );
        if let Some(n) = s.prompt_tokens {
            push(
                &mut left,
                format!("{} in", compact_count(n)),
                theme.fg(Tok::Muted),
            );
        }
        if let Some(n) = s.output_tokens {
            push(
                &mut left,
                format!("{} out", compact_count(n)),
                theme.fg(Tok::Muted),
            );
        }
    }
    let right: Vec<Span<'static>> = if changed > 0 {
        vec![
            Span::styled("/review-file", theme.fg(Tok::Accent)),
            Span::styled(dot.clone(), theme.fg(Tok::Dim)),
            Span::styled("/undo", theme.fg(Tok::Accent)),
        ]
    } else {
        Vec::new()
    };
    let left_w: usize = left.iter().map(|s| text::width(&s.content)).sum();
    let right_w: usize = right.iter().map(|s| text::width(&s.content)).sum();
    if left_w + right_w + 4 <= w {
        let fill = w - left_w - right_w - if right_w > 0 { 2 } else { 0 };
        left.push(Span::styled(
            format!(" {}", rule.repeat(fill.saturating_sub(1))),
            theme.fg(Tok::Border),
        ));
        if right_w > 0 {
            left.push(Span::raw(" "));
            left.extend(right);
            left.push(Span::raw(" "));
        }
    }
    vec![fitted(left, w, theme)]
}

// ---------------------------------------------------------------------------------------------
// the transcript

pub struct Transcript {
    blocks: Vec<Block>,
    /// First block whose rows have not been handed out through `take_settled`.
    cursor: usize,
    /// Whether the last block handed to scrollback was a tool call; `None` before any. The blank
    /// separator after it is owed until the next block shows whether it belongs to the same group.
    owed_gap: Option<bool>,
    /// How the running turn ended, waiting for the closing stats row that shows it.
    outcome: Option<TurnOutcome>,
}

/// Blocks are separated by one blank row, except consecutive members of the work group (tool
/// calls and the one-line context facts beside them), which read as one group.
fn gap_between(prev_is_tool: bool, next_is_tool: bool) -> bool {
    !(prev_is_tool && next_is_tool)
}

impl Block {
    /// Whether this block belongs to the work group: a tool call, or a context/alert line.
    /// Dividers stand apart.
    fn is_tool(&self) -> bool {
        match &self.kind {
            Kind::Tool(_) => true,
            Kind::Event(event) => event.kind.lane() != crate::agent::ui_event::Lane::Divider,
            _ => false,
        }
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    fn invalidate_tail(&self) {
        for block in &self.blocks[self.cursor..] {
            block.revision.set(block.revision.get().saturating_add(1));
        }
    }

    pub fn bind_prose(&mut self, body: &crate::runtime::Body) {
        self.bind_prose_state(body, false);
    }
    /// Recovered observations are already settled; retain their shared body and interrupted phase.
    pub fn restore_prose(&mut self, body: &crate::runtime::Body) {
        self.bind_prose_state(body, true);
    }
    fn bind_prose_state(&mut self, body: &crate::runtime::Body, recovered: bool) {
        if let Some(block) = self.blocks[self.cursor..]
            .iter_mut()
            .find(|b| b.shared.as_ref().is_some_and(|shared| shared.same(body)))
        {
            block.active = body.read(|n| n.result.phase == crate::runtime::Phase::Running);
            return;
        }
        let kind = body.read(|n| {
            if !recovered && n.result.phase != crate::runtime::Phase::Running {
                return None;
            }
            match n.kind.as_str() {
                "assistant" => Some(Kind::Assistant(String::new())),
                "reasoning" => Some(Kind::Thinking(String::new())),
                _ => None,
            }
        });
        let Some(kind) = kind else { return };
        self.close_streaming_text();
        let mut block = Block::new(
            kind,
            body.read(|node| node.result.phase == crate::runtime::Phase::Running),
        );
        block.shared = Some(body.clone());
        self.blocks.push(block);
    }

    pub fn bind_tool(&mut self, id: &str, body: crate::runtime::Body) {
        if let Some(block) = self.blocks[self.cursor..]
            .iter_mut()
            .find(|b| matches!(&b.kind,Kind::Tool(t) if t.id==id))
        {
            block.shared = Some(body);
        }
    }
    pub fn new() -> Self {
        Transcript {
            blocks: Vec::new(),
            cursor: 0,
            owed_gap: None,
            outcome: None,
        }
    }

    /// A new model request begins: whatever prose streamed so far is a finished message, so the next
    /// reply starts its own block instead of continuing the previous sentence.
    pub fn end_text(&mut self) {
        self.invalidate_tail();
        self.close_streaming_text();
    }

    /// Local prose ends when another block starts; shared prose follows its Body's phase.
    fn close_streaming_text(&mut self) {
        for block in self.blocks[self.cursor..].iter_mut() {
            if block.active
                && block.shared.is_none()
                && matches!(&block.kind, Kind::Assistant(_) | Kind::Thinking(_))
            {
                block.active = false;
            }
        }
    }

    fn push(&mut self, kind: Kind, active: bool) {
        // A pending job panel keeps its place only until something else is printed; holding
        // later blocks back for a long job would squeeze the conversation out of the live area.
        for block in self.blocks[self.cursor..].iter_mut() {
            if block.active && matches!(block.kind, Kind::Notice { .. }) {
                block.active = false;
                block.revision.set(block.revision.get().saturating_add(1));
            }
        }
        self.blocks.push(Block::new(kind, active));
    }

    pub fn push_user(&mut self, text: &str) {
        self.close_streaming_text();
        self.push(Kind::User(text.to_owned()), false);
    }

    pub fn assistant_delta(&mut self, text: &str) {
        self.invalidate_tail();
        if text.is_empty() {
            return;
        }
        if let Some(Block {
            kind: Kind::Assistant(buf),
            active: true,
            shared: None,
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
        self.invalidate_tail();
        if text.is_empty() {
            return;
        }
        if let Some(Block {
            kind: Kind::Thinking(buf),
            active: true,
            shared: None,
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
                presentation: crate::agent::legacy_tool_presentation(name, summary, input),
                input: input.to_owned(),
                state: ToolState::Running,
                output: String::new(),
                progress: None,
                elapsed: None,
                todo: None,
                agents: Vec::new(),
            }),
            true,
        );
    }

    /// Append a live chunk to a pending card; full output is retained separately in artifacts.
    pub fn tool_delta(&mut self, id: &str, text: &str) {
        if self.blocks[self.cursor..]
            .iter()
            .any(|b| b.shared.is_some() && matches!(&b.kind,Kind::Tool(t) if t.id==id))
        {
            return;
        }
        self.invalidate_tail();
        let Some(tool) =
            self.blocks[self.cursor..]
                .iter_mut()
                .rev()
                .find_map(|b| match &mut b.kind {
                    Kind::Tool(t) if t.id == id && t.state == ToolState::Running => Some(t),
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

    /// Replace the in-place status of the pending card for tool `name` with its latest `progress`
    /// event. A snapshot, not a log: each event overwrites the last, so the card stays one bounded
    /// block however long the tool runs.
    pub fn tool_progress(&mut self, name: &str, text: &str) {
        self.invalidate_tail();
        let Some(tool) =
            self.blocks[self.cursor..]
                .iter_mut()
                .rev()
                .find_map(|b| match &mut b.kind {
                    Kind::Tool(t) if t.name == name && t.state == ToolState::Running => Some(t),
                    _ => None,
                })
        else {
            return;
        };
        const LIMIT: usize = 16 * 1024;
        let mut cut = text.len().saturating_sub(LIMIT);
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        tool.progress = Some(text[cut..].to_owned());
    }

    /// Attach the task list a pending `todo` call produced, so its card shows what changed.
    pub fn tool_todo(&mut self, id: &str, tasks: Vec<(crate::agent::todo::Task, bool)>) {
        self.invalidate_tail();
        if let Some(tool) =
            self.blocks[self.cursor..]
                .iter_mut()
                .rev()
                .find_map(|b| match &mut b.kind {
                    Kind::Tool(t) if t.id == id && t.state == ToolState::Running => Some(t),
                    _ => None,
                })
        {
            tool.todo = Some(tasks);
        }
    }

    pub fn tool_presentation(
        &mut self,
        id: &str,
        view: &crate::runtime::presentation::Presentation,
    ) {
        self.invalidate_tail();
        if let Some(tool) = self
            .blocks
            .iter_mut()
            .rev()
            .find_map(|b| match &mut b.kind {
                Kind::Tool(t) if t.id == id => Some(t),
                _ => None,
            })
        {
            tool.presentation = view.clone();
        }
    }

    pub fn tool_end(&mut self, id: &str, output: &str, is_error: bool, elapsed_secs: f64) {
        self.invalidate_tail();
        let state = if is_error {
            ToolState::Failed
        } else {
            ToolState::Completed
        };
        let found = self.blocks[self.cursor..]
            .iter_mut()
            .rev()
            .find_map(|b| match &mut b.kind {
                Kind::Tool(t) if t.id == id && t.state == ToolState::Running => {
                    Some((&mut b.active, t, b.shared.is_some()))
                }
                _ => None,
            });
        match found {
            Some((active, tool, shared)) => {
                tool.state = state;
                if !shared {
                    tool.output = output.to_owned();
                }
                if !shared {
                    let args = serde_json::from_str(&tool.input).unwrap_or(Value::Null);
                    let mut view =
                        crate::agent::legacy_tool_result(&tool.name, &args, output, is_error);
                    if view.target.is_empty() || view.target == "null" {
                        view.target = tool.presentation.target.clone();
                    }
                    tool.presentation = view;
                }
                tool.elapsed = Some(elapsed_secs);
                tool.progress = None;
                *active = false;
            }
            None => {
                self.close_streaming_text();
                self.push(
                    Kind::Tool(Tool {
                        id: id.to_owned(),
                        name: "tool".into(),
                        summary: String::new(),
                        presentation: crate::agent::legacy_tool_presentation("tool", "", ""),
                        input: String::new(),
                        state,
                        output: output.to_owned(),
                        progress: None,
                        elapsed: Some(elapsed_secs),
                        todo: None,
                        agents: Vec::new(),
                    }),
                    false,
                );
            }
        }
    }

    /// `(name, summary)` of the call with this id, so the controller can attach a detail preview.
    pub fn tool_call_summary(&self, id: &str) -> Option<(String, String)> {
        self.blocks[self.cursor..]
            .iter()
            .rev()
            .find_map(|b| match &b.kind {
                Kind::Tool(t) if t.id == id => Some((t.name.clone(), t.summary.clone())),
                _ => None,
            })
    }

    /// Replace a call's input with richer data the controller found (e.g. what a write created).
    pub fn tool_input(&mut self, id: &str, input: String) {
        self.invalidate_tail();
        if let Some(Kind::Tool(t)) = self.blocks[self.cursor..]
            .iter_mut()
            .rev()
            .map(|b| &mut b.kind)
            .find(|k| matches!(k, Kind::Tool(t) if t.id == id))
        {
            t.input = input;
        }
    }

    /// Project typed worker results into cards, including status/wait calls.
    pub fn tool_data(&mut self, id: &str, data: &Value) {
        self.invalidate_tail();
        let jobs: Vec<&Value> = match data.as_array().or_else(|| data["jobs"].as_array()) {
            Some(jobs) => jobs.iter().collect(),
            None if data.get("status").is_some() => vec![data],
            _ => return,
        };
        for block in &mut self.blocks {
            let Kind::Tool(tool) = &mut block.kind else {
                continue;
            };
            if tool.id != id || tool.name != "task" {
                continue;
            }
            tool.agents = jobs
                .iter()
                .map(|job| {
                    let state = ToolState::from_status(job["status"].as_str().unwrap_or("failed"));
                    let preview = job
                        .get("result")
                        .map(|result| {
                            result
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| result.to_string())
                        })
                        .unwrap_or_default();
                    AgentRow {
                        name: job["name"]
                            .as_str()
                            .or_else(|| job["id"].as_str())
                            .unwrap_or("worker")
                            .to_owned(),
                        agent: job["agent"].as_str().unwrap_or("").to_owned(),
                        model: job["model"].as_str().unwrap_or("").to_owned(),
                        task: job["task"]
                            .as_str()
                            .or_else(|| job["assignment"]["task"].as_str())
                            .unwrap_or("")
                            .to_owned(),
                        state,
                        current_tool: job["phase"]
                            .as_str()
                            .filter(|_| state == ToolState::Running)
                            .map(str::to_owned),
                        tools: job["tools"].as_u64().unwrap_or(0),
                        steps: job["steps"].as_u64().unwrap_or(0),
                        elapsed_ms: job["elapsed_ms"].as_u64(),
                        preview: text::sanitize(&preview),
                    }
                })
                .collect();
            block.revision.set(block.revision.get().saturating_add(1));
        }
    }

    /// Mirror the workers each live `task` call spawned into its card. A worker belongs to the task
    /// call whose runtime body is its parent; `now_ms` dates running agents' elapsed time.
    pub fn sync_agents(&mut self, workers: &[crate::runtime::Node], now_ms: u64) {
        for block in self.blocks[self.cursor..].iter_mut() {
            let Some(id) = block.shared.as_ref().map(|b| b.read(|n| n.id.clone())) else {
                continue;
            };
            let Kind::Tool(t) = &mut block.kind else {
                continue;
            };
            if t.name != "task" {
                continue;
            }
            let rows: Vec<AgentRow> = workers
                .iter()
                .filter(|w| w.parent.as_deref() == Some(id.as_str()))
                .map(|w| {
                    let d = w.result.data.clone().unwrap_or_default();
                    let state = w.result.phase;
                    let running = state == ToolState::Running;
                    let current_tool = d["phase"]
                        .as_str()
                        .and_then(|p| p.strip_prefix("tool: "))
                        .filter(|_| running)
                        .map(str::to_owned);
                    let elapsed_ms = d["elapsed_ms"].as_u64().or_else(|| {
                        d["started_ms"]
                            .as_u64()
                            .filter(|_| running)
                            .map(|s| now_ms.saturating_sub(s))
                    });
                    let preview = if running {
                        w.result
                            .text
                            .lines()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("")
                    } else {
                        d["result"].as_str().unwrap_or(&w.result.text)
                    };
                    AgentRow {
                        name: d["name"].as_str().unwrap_or(&w.label).to_owned(),
                        agent: d["agent"].as_str().unwrap_or("").to_owned(),
                        model: d["model"].as_str().unwrap_or("").to_owned(),
                        task: d["task"].as_str().unwrap_or("").to_owned(),
                        state,
                        current_tool,
                        tools: d["tools"].as_u64().unwrap_or(0),
                        steps: d["steps"].as_u64().unwrap_or(0),
                        elapsed_ms,
                        preview: text::sanitize(preview),
                    }
                })
                .collect();
            // Status/wait calls refer to existing workers but own no child nodes. Keep their
            // typed projection instead of replacing it with an empty runtime snapshot.
            if rows.is_empty() {
                continue;
            }
            // A spawn call can return before its workers finish. Keep the card live until the
            // owned workers settle, so progress is never frozen prematurely in scrollback.
            block.active = t.state == ToolState::Running
                || rows.iter().any(|row| row.state == ToolState::Running);
            if rows != t.agents {
                t.agents = rows;
                block.revision.set(block.revision.get().saturating_add(1));
            }
        }
    }

    /// A typed controller event in its own lane.
    pub fn event(&mut self, event: &crate::agent::ui_event::UiEvent) {
        self.close_streaming_text();
        if self.blocks.last().is_some_and(|block| matches!(&block.kind, Kind::Event(previous) if previous.kind == event.kind && previous.text == event.text)) {
            return; // Presentation only: the execution journal keeps the original events.
        }
        self.push(Kind::Event(event.clone()), false);
    }

    pub fn notice(&mut self, title: &str, text: &str) {
        let brief = super::text::brief_chain(text);
        let text = brief.as_str();
        self.close_streaming_text();
        if self.blocks.last().is_some_and(|block| matches!(&block.kind, Kind::Notice { title: previous, text: body } if previous == title && body == text)) {
            return; // Presentation only: the execution journal keeps the original events.
        }
        self.push(
            Kind::Notice {
                title: title.to_owned(),
                text: text.to_owned(),
            },
            false,
        );
    }

    /// Bounded live output keyed by job, independent of foreground tool-card lifetimes.
    /// PTY snapshots replace previous content; pipe deltas append without deduplication.
    pub fn background_output(&mut self, name: &str, text: &str, replace: bool) {
        const LIMIT: usize = 16 * 1024;
        let title = format!("Background {name}");
        self.invalidate_tail();
        if let Some(block) = self.blocks[self.cursor..].iter_mut().rev().find(|block| {
            matches!(&block.kind, Kind::Notice { title: previous, .. } if previous == &title)
        }) {
            if let Kind::Notice { text: body, .. } = &mut block.kind {
                if replace {
                    body.clear();
                }
                body.push_str(text);
                let cut = body.ceil_char_boundary(body.len().saturating_sub(LIMIT));
                body.drain(..cut);
                block.revision.set(block.revision.get().saturating_add(1));
            }
        } else {
            let cut = text.ceil_char_boundary(text.len().saturating_sub(LIMIT));
            self.push(Kind::Notice { title, text: text[cut..].to_owned() }, false);
        }
    }

    /// Placeholder a background job shows until its first output arrives.
    pub const JOB_STARTED: &'static str = "Started; the result appears here when it finishes.";

    /// A background job's panel: shows [`Self::JOB_STARTED`] and stays in the live area until the
    /// job's output replaces it, so start, progress and result read as one notice. Anything else
    /// printed first releases it, and the result then follows as its own notice.
    pub fn notice_pending(&mut self, title: &str) {
        self.close_streaming_text();
        self.push(
            Kind::Notice {
                title: title.to_owned(),
                text: Self::JOB_STARTED.to_owned(),
            },
            true,
        );
    }

    /// Adds job output to the panel titled `title`: the pending one from [`Self::notice_pending`]
    /// (its placeholder is replaced), else the newest notice when it has that title and has not
    /// reached scrollback yet. Otherwise behaves like [`Self::notice`].
    pub fn notice_merge(&mut self, title: &str, text: &str) {
        let text = super::text::brief_chain(text);
        let cursor = self.cursor;
        let last = self.blocks.len().saturating_sub(1);
        let target = (cursor..self.blocks.len()).rev().find(|&index| {
            let block = &self.blocks[index];
            matches!(&block.kind, Kind::Notice { title: previous, .. } if previous == title)
                && (block.active || index == last)
        });
        if let Some(index) = target {
            let block = &mut self.blocks[index];
            if let Kind::Notice { text: body, .. } = &mut block.kind {
                if body == Self::JOB_STARTED {
                    *body = text;
                } else if !body.lines().any(|line| line == text) {
                    body.push('\n');
                    body.push_str(&text);
                }
                block.revision.set(block.revision.get().saturating_add(1));
                return;
            }
        }
        self.notice(title, &text);
    }

    /// Whether the job panel titled `title` already carries output.
    pub fn notice_has_output(&self, title: &str) -> bool {
        self.blocks[self.cursor..].iter().rev().any(|block| {
            matches!(&block.kind, Kind::Notice { title: previous, text } if previous == title && text != Self::JOB_STARTED)
        })
    }

    /// Whether a job panel titled `title` is still waiting for its first output.
    pub fn job_waiting(&self, title: &str) -> bool {
        self.blocks[self.cursor..].iter().any(|block| {
            block.active
                && matches!(&block.kind, Kind::Notice { title: previous, text } if previous == title && text == Self::JOB_STARTED)
        })
    }

    /// Ends every pending job panel. A panel the job never filled is dropped: whoever stopped the
    /// job reports that, and an empty "Started" panel would only mislead.
    pub fn settle_job_notices(&mut self) {
        let cursor = self.cursor;
        let mut index = cursor;
        while index < self.blocks.len() {
            let block = &mut self.blocks[index];
            if block.active
                && let Kind::Notice { text, .. } = &block.kind
            {
                if text == Self::JOB_STARTED {
                    self.blocks.remove(index);
                    continue;
                }
                block.active = false;
                block.revision.set(block.revision.get().saturating_add(1));
            }
            index += 1;
        }
    }

    pub fn image(&mut self, thumbnail: super::images::Thumbnail) {
        self.close_streaming_text();
        self.push(Kind::Image(thumbnail), false);
    }

    pub fn error(&mut self, text: &str) {
        self.close_streaming_text();
        self.push(Kind::Error(super::text::brief_chain(text)), false);
    }

    /// The agent reported how the turn ended; shown on the turn's closing rule.
    pub fn completion(&mut self, outcome: TurnOutcome) {
        self.outcome = Some(outcome);
    }

    pub fn turn_stats(&mut self, mut stats: TurnStats) {
        self.close_streaming_text();
        if stats.outcome.is_none() {
            stats.outcome = self.outcome.take();
        }
        self.push(Kind::Stats(stats), false);
    }

    /// Finalize every streaming block; a tool that never reported back is shown as interrupted.
    pub fn finish_turn(&mut self) {
        self.invalidate_tail();
        for block in self.blocks[self.cursor..].iter_mut() {
            if let Kind::Tool(t) = &mut block.kind
                && t.state == ToolState::Running
            {
                t.state = ToolState::Interrupted;
                if t.output.is_empty() {
                    t.output = "interrupted".into();
                }
            }
            block.active = false;
        }
    }

    #[cfg(test)]
    pub fn is_streaming(&self) -> bool {
        self.blocks[self.cursor..].iter().any(|b| b.active)
    }

    /// Rows of finalized blocks not handed out yet, each row exactly once; stops at the first block
    /// that is still active. Every block that has rows ends with one blank separator row. A block that
    /// was partly spilled by [`live`](Self::live) contributes only its remainder.
    #[cfg(test)]
    pub fn take_settled(&mut self, width: u16, ctx: &TranscriptCtx) -> Rows {
        self.take_settled_with_images(width, ctx).0
    }

    /// Native overlays are anchored to the separator after the preview and emitted once, together
    /// with settled history. Portable cells remain underneath as a fallback.
    pub fn take_settled_with_images(
        &mut self,
        width: u16,
        ctx: &TranscriptCtx,
    ) -> (Rows, Vec<(usize, super::images::NativeImage)>) {
        let mut out = Vec::new();
        let mut images = Vec::new();
        while let Some(block) = self.blocks.get_mut(self.cursor) {
            if block.active {
                break;
            }
            let rows = block.render(width, ctx);
            let skip = block.emitted(width, ctx);
            let tool = block.is_tool();
            if !rows.is_empty() || skip > 0 {
                // A partly spilled block already paid its separator when its head was spilled.
                if skip == 0 && self.owed_gap.is_some_and(|prev| gap_between(prev, tool)) {
                    out.push(blank_row());
                }
                let mut image_separator = false;
                if skip == 0
                    && let Kind::Image(image) = &block.kind
                    && let Some(native) = image.native(width)
                {
                    images.push((out.len() + rows.len(), native));
                    image_separator = true;
                }
                out.extend(rows.iter().skip(skip).cloned());
                if image_separator {
                    // Native graphics draw upward from a reserved separator cell. It must exist
                    // even when this image is the last settled block before the live composer.
                    out.push(blank_row());
                    self.owed_gap = None;
                } else {
                    self.owed_gap = Some(tool);
                }
            }
            self.cursor += 1;
        }
        (out, images)
    }

    /// `(spilled, live)`: rows that must be committed to scrollback now (the stable head of the
    /// streaming block when the tail is taller than `max_rows`, each row returned once) and the rows
    /// to repaint, at most `max_rows`.
    pub fn live(&mut self, width: u16, ctx: &TranscriptCtx, max_rows: usize) -> (Rows, Rows) {
        let Some((first, rest)) = self.blocks[self.cursor..].split_first_mut() else {
            // Idle: the owed separator keeps the last block off the composer.
            let gap = if self.owed_gap.is_some() {
                vec![blank_row()]
            } else {
                Vec::new()
            };
            return (Vec::new(), gap);
        };
        let head = first.render(width, ctx);
        let mut skip = first.emitted(width, ctx);
        let head_tool = first.is_tool();
        let had_head = !head.is_empty() || skip > 0;
        let rest_rows: Vec<(bool, std::sync::Arc<Rows>)> = rest
            .iter()
            .map(|b| (b.is_tool(), b.render(width, ctx)))
            .collect();
        let owed = self.owed_gap;

        let assemble = |head: &Rows, skip: usize| -> Rows {
            let mut out: Rows = Vec::new();
            if had_head && skip == 0 && owed.is_some_and(|prev| gap_between(prev, head_tool)) {
                out.push(blank_row());
            }
            out.extend(head.iter().skip(skip).cloned());
            let mut prev = had_head.then_some(head_tool).or(owed);
            for (tool, piece) in &rest_rows {
                if piece.is_empty() {
                    continue;
                }
                if prev.is_some_and(|p| gap_between(p, *tool)) {
                    out.push(blank_row());
                }
                out.extend(piece.iter().cloned());
                prev = Some(*tool);
            }
            // The owed separator also keeps the transcript off the composer while idle.
            if prev.is_some() {
                out.push(blank_row());
            }
            out
        };

        let mut display = assemble(&head, skip);
        let mut spilled = Vec::new();
        if display.len() > max_rows
            && first.active
            && let Some((src, rows)) = first.spill_point(width, ctx, &head, skip)
        {
            if skip == 0 && owed.is_some_and(|prev| gap_between(prev, head_tool)) {
                spilled.push(blank_row());
            }
            spilled.extend(head[skip..rows].iter().cloned());
            self.owed_gap = None;
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
    /// Also returns, for each block, the row where it starts, so a viewer can jump to a block.
    pub fn document(&self, width: u16, ctx: &TranscriptCtx) -> (Rows, Vec<usize>) {
        let mut out = Vec::new();
        let mut anchors = Vec::with_capacity(self.blocks.len());
        let mut prev: Option<bool> = None;
        for block in &self.blocks {
            let rows = block.render(width, ctx);
            if rows.is_empty() {
                anchors.push(out.len());
                continue;
            }
            if prev.is_some_and(|p| gap_between(p, block.is_tool())) {
                out.push(blank_row());
            }
            anchors.push(out.len());
            out.extend(rows.iter().cloned());
            prev = Some(block.is_tool());
        }
        (out, anchors)
    }

    #[cfg(test)]
    pub fn all_rows(&self, width: u16, ctx: &TranscriptCtx) -> Rows {
        self.document(width, ctx).0
    }

    /// What has already been handed to scrollback (settled blocks plus the spilled head of the
    /// streaming one), rendered afresh at `width`. After a display reset, commit this and keep
    /// using `take_settled` / `live` unchanged: nothing is repeated and nothing is lost.
    pub fn committed_rows(&self, width: u16, ctx: &TranscriptCtx) -> Rows {
        let mut out = Vec::new();
        let mut prev: Option<bool> = None;
        for (i, block) in self.blocks.iter().enumerate() {
            if i > self.cursor {
                break;
            }
            let rows = block.render(width, ctx);
            let gap = prev.is_some_and(|p| gap_between(p, block.is_tool()));
            if i < self.cursor {
                if !rows.is_empty() {
                    if gap {
                        out.push(blank_row());
                    }
                    out.extend(rows.iter().cloned());
                    prev = Some(block.is_tool());
                }
            } else if block.spill.src > 0 {
                let n = match block.spill.key {
                    Some(key) if key == (width, ctx.ui.show_thinking) => block.spill.rows,
                    _ => block.measure_prefix(width, ctx),
                };
                if gap {
                    out.push(blank_row());
                }
                out.extend(rows.iter().take(n).cloned());
            }
        }
        out
    }
}

/// Applies archived conversation messages to a transcript, one record at a time, so a file that
/// grows can be followed without replaying it from the start. A call and its result may arrive in
/// different pushes; the open-call map carries them across. Child archives also carry the richer
/// `dial_tool_presentation` card, which replaces the legacy summary when present.
#[derive(Default)]
pub struct Replay {
    open: std::collections::HashMap<String, String>,
}

impl Replay {
    /// Apply one message record. Returns the block index the record's first block was created at,
    /// so callers can map a record to a row.
    pub fn push(&mut self, transcript: &mut Transcript, message: &Value) -> usize {
        let first = transcript.blocks.len();
        match message["role"].as_str() {
            Some("user") => {
                if let Some(text) = message["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    transcript.push_user(text);
                }
            }
            Some("assistant") => {
                // Reasoning is stored either as `reasoning_content` or, for providers that return
                // typed blocks, as `thinking` blocks inside `dial_provider_content`.
                let provider_thinking: String = message["dial_provider_content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|block| block["type"] == "thinking")
                    .filter_map(|block| block["thinking"].as_str())
                    .collect();
                let reasoning = message["reasoning_content"]
                    .as_str()
                    .filter(|t| !t.trim().is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        (!provider_thinking.trim().is_empty()).then_some(provider_thinking)
                    });
                if let Some(text) = reasoning {
                    transcript.thinking_delta(&text);
                }
                if let Some(text) = message["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    transcript.assistant_delta(text);
                    transcript.finish_turn();
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let (id, name) = (
                        call["id"].as_str().unwrap_or("call"),
                        call["function"]["name"].as_str().unwrap_or("tool"),
                    );
                    let args: Value = call["function"]["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(Value::Null);
                    let summary = crate::agent::summarize_call(name, &args);
                    transcript.tool_start(id, name, &summary, &args.to_string());
                    self.open.insert(id.to_owned(), name.to_owned());
                }
            }
            Some("tool") => {
                let id = message["tool_call_id"].as_str().unwrap_or("");
                if self.open.remove(id).is_some() {
                    let child = &message["dial_child_tool_result"];
                    let output = child["text"]
                        .as_str()
                        .or_else(|| message["content"].as_str())
                        .unwrap_or("");
                    let failed = child["is_error"].as_bool().unwrap_or_else(|| {
                        output.starts_with("invalid tool call")
                            || output.starts_with("denied")
                            || output.starts_with("error")
                    });
                    transcript.tool_end(id, output, failed, 0.0);
                    if let Ok(view) =
                        serde_json::from_value::<crate::runtime::presentation::Presentation>(
                            message["dial_tool_presentation"]["view"].clone(),
                        )
                    {
                        transcript.tool_presentation(id, &view);
                    }
                }
            }
            _ => {}
        }
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Glyphs;
    use ratatui::style::Modifier;
    #[test]
    fn coherent_runtime_custom_cards_keep_state_preview_and_expanded_evidence() {
        use crate::runtime::{
            Phase, Tree,
            presentation::{BodyStyle, Presentation},
        };
        let env = Env::new();
        let tree = Tree::default();
        let body = tree.start("tool", "extension_name", None);
        body.correlate("custom-call");
        body.present(Presentation {
            label: "Inspect\u{1b}[31m".into(),
            target: "parser ownership\n".into(),
            body: BodyStyle::Text,
            preview: Some("short finding".into()),
            facts: vec!["3 findings".into()],
            ..Default::default()
        });
        body.settle(
            "FULL_EVIDENCE_SENTINEL\nsecond line".into(),
            None,
            Phase::Incomplete,
        );
        let mut transcript = Transcript::new();
        transcript.tool_start("custom-call", "extension_name", "opaque summary", "");
        transcript.bind_tool("custom-call", body);
        transcript.tool_end("custom-call", "ignored duplicate", false, 0.1);
        let compact = text_of(&transcript.all_rows(100, &env.ctx()));
        assert!(
            compact.contains("Inspect")
                && compact.contains("parser ownership")
                && compact.contains("incomplete")
        );
        assert!(compact.contains("short finding") && !compact.contains("FULL_EVIDENCE_SENTINEL"));
        let expanded = text_of(&transcript.all_rows(
            100,
            &TranscriptCtx {
                tools_expanded: true,
                ..env.ctx()
            },
        ));
        assert!(expanded.contains("FULL_EVIDENCE_SENTINEL") && expanded.contains("second line"));
        assert!(!expanded.contains("ignored duplicate"));
        assert!(
            !expanded.contains("\u{1b}") && !compact.contains("\u{1b}"),
            "Tool-owned labels are still untrusted terminal text"
        );
    }

    #[test]
    fn consistent_ux_expansion_reveals_committed_reads_searches_and_checks() {
        let env = Env::new();
        let mut transcript = Transcript::new();
        for (id, name, summary, result) in [
            (
                "read1",
                "read",
                "src/parser.rs",
                "[src/parser.rs#AB12]\n1: fn parse_tail() {}",
            ),
            (
                "search1",
                "grep",
                "parse_tail",
                "src/parser.rs:1: fn parse_tail() {}",
            ),
            (
                "check1",
                "bash",
                "Verify parser: cargo test",
                "test parse_tail ... ok\n[exit code: 0]",
            ),
        ] {
            let input = if name == "bash" {
                serde_json::json!({"command":"cargo test","_dial_check":"parser"}).to_string()
            } else {
                String::new()
            };
            transcript.tool_start(id, name, summary, &input);
            transcript.tool_end(id, result, false, 0.1);
        }
        transcript.take_settled(80, &env.ctx());
        let collapsed = text_of(&transcript.committed_rows(80, &env.ctx()));
        assert!(
            !collapsed.contains("fn parse_tail()") && !collapsed.contains("test parse_tail ... ok")
        );
        let expanded = TranscriptCtx {
            tools_expanded: true,
            ..env.ctx()
        };
        let rows = transcript.committed_rows(80, &expanded);
        assert_fits(&rows, 80);
        let text = text_of(&rows);
        assert!(text.contains("fn parse_tail()") && text.contains("test parse_tail ... ok"));
        assert!(
            transcript.take_settled(80, &expanded).is_empty(),
            "repainting does not redeliver committed records"
        );
    }

    #[test]
    fn consistent_ux_worker_cards_expand_reports_without_internal_json() {
        let env = Env::new();
        let mut transcript = Transcript::new();
        transcript.tool_start("task1", "task", "Parser scout", "");
        transcript.tool_end("task1", "Worker task-1: done", false, 0.1);
        transcript.tool_data("task1", &serde_json::json!([{"id":"task-1","name":"Parser scout","status":"done","steps":3,"result":"The parser drops the final line.\nFix parse_tail in parser.rs.","assignment":{"task":"Inspect parser"},"portable_rules":{"regenerations":0}}]));
        let expanded = TranscriptCtx {
            tools_expanded: true,
            ..env.ctx()
        };
        let text = text_of(&transcript.all_rows(80, &expanded));
        assert!(text.contains("Fix parse_tail in parser.rs"));
        assert!(!text.contains("portable_rules") && !text.contains("regenerations"));
    }

    #[test]
    fn consistent_ux_worker_card_settles_with_its_workers() {
        use crate::runtime::{Phase, Tree};
        let env = Env::new();
        let tree = Tree::default();
        let call = tree.start("tool", "task", None);
        let worker = tree.start("worker", "Parser scout", Some(call.read(|n| n.id.clone())));
        let mut transcript = Transcript::new();
        transcript.tool_start("spawn", "task", "Parser scout", "");
        transcript.bind_tool("spawn", call.clone());
        call.settle("Worker started".into(), None, Phase::Completed);
        transcript.tool_end("spawn", "Worker started", false, 0.1);
        transcript.sync_agents(&[worker.snapshot()], 100);
        assert!(transcript.take_settled(80, &env.ctx()).is_empty());
        worker.settle(
            "Parser drops the final line.".into(),
            None,
            Phase::Completed,
        );
        transcript.sync_agents(&[worker.snapshot()], 200);
        let expanded = TranscriptCtx {
            tools_expanded: true,
            ..env.ctx()
        };
        let settled = text_of(&transcript.take_settled(80, &expanded));
        assert!(settled.contains("Parser drops the final line."));
        assert!(transcript.take_settled(80, &expanded).is_empty());

        transcript.tool_start("status", "task", "Worker status", "");
        transcript.tool_end("status", "done", false, 0.1);
        transcript.tool_data("status", &serde_json::json!({"id":"task-1","status":"done","result":"Parser drops the final line."}));
        transcript.sync_agents(&[worker.snapshot()], 300);
        assert!(
            text_of(&transcript.take_settled(80, &expanded))
                .contains("Parser drops the final line.")
        );
    }

    #[test]
    fn consistent_ux_compact_diffs_honor_the_requested_preview_limit() {
        let mut env = Env::new();
        env.ui.tool_output_lines = 3;
        let mut transcript = Transcript::new();
        transcript.tool_start("edit1", "edit", "a.rs", "");
        let lines = (0..30).map(|i| format!("+line_{i}\n")).collect::<String>();
        transcript.tool_end(
            "edit1",
            &format!(
                "[a.rs#AB12]\nupdated (+30 -0 lines)\nchange preview (\n@@ -1,0 +1,30 @@\n{lines}"
            ),
            false,
            0.1,
        );
        let compact = text_of(&transcript.all_rows(80, &env.ctx()));
        assert!(!compact.contains("line_20") && compact.contains("more lines"));
        let expanded = TranscriptCtx {
            tools_expanded: true,
            ..env.ctx()
        };
        assert!(text_of(&transcript.all_rows(80, &expanded)).contains("line_29"));
    }

    #[test]
    fn consistent_ux_rendered_fixture_prioritizes_findings_and_the_composer() {
        use crate::ui::{
            composer::{Composer, ComposerCtx},
            keys::Keymap,
            status::StatusData,
            welcome::{self, WelcomeInfo},
        };
        use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
        let mut env = Env::new();
        env.theme = Theme::builtin("dial-dark").unwrap();
        let mut transcript = Transcript::new();
        transcript.push_user("Why do agents in Dial randomly stop?");
        transcript.tool_start("r", "read", "src/agent/child.rs", "");
        transcript.tool_end(
            "r",
            "[src/agent/child.rs#AB12]\n1: child completion path",
            false,
            0.12,
        );
        transcript.tool_start("t", "task", "Trace recovery", "");
        transcript.tool_end("t", "Worker task-1: done", false, 0.5);
        transcript.tool_data("t", &serde_json::json!([{"name":"Recovery scout","agent":"scout","status":"done","steps":3,"result":"Malformed child replies bypassed the parent's retry limit."}]));
        transcript.assistant_delta("The child loop could retry malformed calls indefinitely. Parent and child now share bounded recovery, and an unfinished report stays incomplete.");
        transcript.finish_turn();
        let mut rows = welcome::rows(
            &WelcomeInfo {
                version: "0.1.1".into(),
                provider: "chatgpt".into(),
                model: "gpt-6.1-sol".into(),
                project: "dial".into(),
                ..Default::default()
            },
            100,
            &env.theme,
        );
        rows.extend(transcript.all_rows(100, &env.ctx()));
        let keys = Keymap::defaults();
        let status = StatusData {
            model: "gpt-6.1-sol".into(),
            provider: "chatgpt".into(),
            project_path: "dial".into(),
            ..Default::default()
        };
        let mut composer = Composer::new(Vec::new(), 100);
        composer.set_draft("Keep working on the parser");
        rows.extend(
            composer
                .view(
                    100,
                    &ComposerCtx {
                        theme: &env.theme,
                        keys: &keys,
                        ui: &env.ui,
                        status: &status,
                    },
                )
                .rows,
        );
        let height = rows.len() as u16;
        let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(rows.clone()), frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(
            screen.contains("Malformed child replies")
                && screen.contains("Keep working on the parser")
        );
        assert!(
            !screen.contains("portable_rules")
                && !screen.contains("PROMPT CACHE")
                && !screen.contains("DIAL MK-I")
        );
        assert_fits(&rows, 100);
        if let Ok(path) = std::env::var("DIAL_TEST_TRANSCRIPT_CAPTURE") {
            let cells: Vec<_> = buffer.content.iter().map(|cell| serde_json::json!({"symbol":cell.symbol(), "fg":format!("{:?}",cell.fg), "bg":format!("{:?}",cell.bg), "modifiers":format!("{:?}",cell.modifier)})).collect();
            std::fs::write(
                path,
                serde_json::json!({"width":100,"height":height,"cells":cells}).to_string(),
            )
            .unwrap();
        }
    }

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
                        *cache.render(&source[..end], width, &theme),
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
                    *cache.render(source, width, &theme),
                    markdown::render(source, width, &theme, false)
                );
            }
        }
    }

    #[test]
    fn streaming_cache_rerenders_when_settled_text_replaces_the_streamed_prefix() {
        let theme = Theme::dark();
        let mut cache = StreamingRows::default();
        let streamed = "First paragraph.\n\nSecond paragraph.\n\nTail";
        cache.render(streamed, 40, &theme);
        let settled = "Rewritten opening.\n\nSecond paragraph.\n\nTail";
        assert_eq!(
            *cache.render(settled, 40, &theme),
            markdown::render(settled, 40, &theme, false)
        );
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

    /// Same workload as the byte-slice measurement, but with producer-owned `Vec` chunks.
    #[test]
    #[ignore = "manual Vec<T> streaming rendering measurement"]
    fn streaming_render_vec_delta_measurement() {
        let paragraph = "A streamed answer with **styled words**, a number 42, and ordinary prose that wraps across terminal rows.\n\n";
        let source = paragraph.repeat(240);
        let answer = &source[..20_000];
        let deltas: Vec<String> = answer
            .as_bytes()
            .chunks(40)
            .map(|chunk| String::from_utf8(chunk.to_vec()).unwrap())
            .collect();
        let mut source = String::with_capacity(20_000);
        let mut cache = StreamingRows::default();
        let before = std::time::Instant::now();
        for delta in deltas {
            source.push_str(&delta);
            std::hint::black_box(cache.render(&source, 100, &Theme::dark()));
        }
        eprintln!("20KB / Vec<String> 40-byte deltas: {:?}", before.elapsed());
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
                expand_key: "Ctrl+O",
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
            !rows.last().unwrap().spans.is_empty(),
            "separators occur between settled blocks"
        );
        assert!(t.take_settled(40, &ctx).is_empty());
    }

    #[test]
    fn background_output_renders_without_pending_tool_and_pty_replaces() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.background_output("pipe-job", "repeated\n", false);
        t.background_output("pipe-job", "repeated\n", false);
        t.background_output("pty-job", "old-screen", true);
        t.background_output("pty-job", "new-screen", true);
        let text = text_of(&t.take_settled(70, &ctx));
        assert!(
            text.contains("PIPE-JOB") && text.contains("PTY-JOB"),
            "{text}"
        );
        assert_eq!(
            text.matches("repeated").count(),
            2,
            "pipe deltas must not be deduplicated: {text}"
        );
        assert!(
            text.contains("new-screen") && !text.contains("old-screen"),
            "{text}"
        );
        t.background_output("pipe-job", "after scrollback", false);
        assert!(text_of(&t.take_settled(70, &ctx)).contains("after scrollback"));
    }

    #[test]
    fn background_live_panel_is_utf8_bounded_while_full_output_stays_in_artifacts() {
        let mut t = Transcript::new();
        t.background_output("unicode", &"😀".repeat(5000), false);
        t.background_output("unicode", "done", false);
        let Kind::Notice { text, .. } = &t.blocks[0].kind else {
            panic!("missing job panel")
        };
        assert!(text.len() <= 16 * 1024);
        assert!(text.ends_with("done"));
        assert!(text.starts_with('😀'));
    }

    #[test]
    fn a_job_panel_holds_its_place_and_is_filled_in_place() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.notice_pending("Usage");
        assert!(
            t.take_settled(60, &ctx).is_empty(),
            "the pending panel stays live"
        );
        assert!(t.job_waiting("Usage"));
        t.notice_merge("Usage", "No usage data.");
        t.settle_job_notices();
        let text = text_of(&t.take_settled(60, &ctx));
        assert!(!text.contains(Transcript::JOB_STARTED), "{text}");
        assert!(text.contains("No usage data."), "{text}");
        assert_eq!(text.matches("USAGE").count(), 1, "{text}");
    }

    #[test]
    fn later_output_releases_a_pending_job_panel() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.notice_pending("Benchmark");
        t.push_user("keep working");
        assert!(
            !t.is_streaming(),
            "a long job must not hold the conversation back"
        );
        let text = text_of(&t.take_settled(60, &ctx));
        assert!(
            text.find("Started").unwrap() < text.find("keep working").unwrap(),
            "{text}"
        );
        t.notice_merge("Benchmark", "suite finished");
        let text = text_of(&t.take_settled(60, &ctx));
        assert!(
            text.contains("suite finished"),
            "the result follows as its own notice: {text}"
        );
    }

    #[test]
    fn progress_lines_append_and_an_unfilled_panel_is_dropped() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.notice_pending("Images");
        t.notice_merge("Images", "Sessions covered: 0");
        t.notice_merge("Images", "Images found: 0");
        assert!(t.notice_has_output("Images"));
        t.settle_job_notices();
        let text = text_of(&t.take_settled(60, &ctx));
        assert!(
            text.contains("Sessions covered") && text.contains("Images found"),
            "{text}"
        );
        assert_eq!(text.matches("IMAGES").count(), 1, "{text}");

        t.notice_pending("Setup");
        t.settle_job_notices();
        assert!(!t.is_streaming());
        assert!(text_of(&t.take_settled(60, &ctx)).is_empty());
    }

    #[test]
    fn settled_blocks_have_one_separator_between_them() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.notice("", "one");
        t.notice("", "two");
        assert_eq!(plain(&t.take_settled(20, &ctx)), ["one", "", "two"]);
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
    fn shared_prose_survives_insertions_and_commits_each_delta_once() {
        let mut env = Env::new();
        env.ui.show_thinking = true;
        let ctx = env.ctx();
        let root = tempfile::tempdir().unwrap();
        image::RgbaImage::new(1, 1)
            .save(root.path().join("preview.png"))
            .unwrap();
        let reference =
            crate::llm::images::attachment(root.path(), std::path::Path::new("preview.png"))
                .unwrap();
        let prefix = "SHARED-PREFIX\n\nSHARED-MIDDLE";
        let answer = format!("{prefix} SHARED-SUFFIX");
        for kind in ["assistant", "reasoning"] {
            for insertion in ["notice", "user", "image"] {
                for max_rows in [1, 20] {
                    let tree = crate::runtime::Tree::default();
                    let body = tree.start(kind, kind, None);
                    let mut t = Transcript::new();
                    body.append(prefix);
                    t.bind_prose(&body);
                    let mut committed = t.live(80, &ctx, max_rows).0;
                    if max_rows == 1 {
                        assert!(
                            !committed.is_empty(),
                            "{kind} should spill its stable prefix"
                        );
                    }
                    match insertion {
                        "notice" => t.notice("Help", "INSERTED-NOTICE"),
                        "user" => t.push_user("INSERTED-USER"),
                        "image" => t.image(crate::ui::images::Thumbnail::load(&reference).unwrap()),
                        _ => unreachable!(),
                    }
                    assert!(t.is_streaming(), "{kind} must survive {insertion}");
                    assert!(t.take_settled(80, &ctx).is_empty());
                    assert_eq!(
                        body.read(|n| n.result.phase),
                        crate::runtime::Phase::Running
                    );
                    body.append(" SHARED-SUFFIX");
                    t.bind_prose(&body.clone());
                    committed.extend(t.live(80, &ctx, max_rows).0);
                    assert!(t.is_streaming());
                    assert!(t.take_settled(80, &ctx).is_empty());
                    body.settle(answer.clone(), None, crate::runtime::Phase::Completed);
                    t.bind_prose(&body);
                    assert!(!t.is_streaming());
                    committed.extend(t.take_settled(80, &ctx));
                    let text = text_of(&committed);
                    for marker in ["SHARED-PREFIX", "SHARED-MIDDLE", "SHARED-SUFFIX"] {
                        assert_eq!(
                            text.matches(marker).count(),
                            1,
                            "{kind}, {insertion}, {max_rows} rows:\n{text}"
                        );
                    }
                    assert_eq!(plain(&committed), plain(&t.all_rows(80, &ctx)));
                    assert!(t.take_settled(80, &ctx).is_empty());
                }
            }
        }
    }

    #[test]
    fn repeated_shared_prose_binding_tracks_identity_and_settlement() {
        let env = Env::new();
        let ctx = env.ctx();
        let first_tree = crate::runtime::Tree::default();
        let second_tree = crate::runtime::Tree::default();
        let first = first_tree.start("assistant", "first", None);
        let second = second_tree.start("assistant", "second", None);
        // Separate trees can issue the same journal id, but these are distinct live bodies.
        assert_eq!(first.read(|n| n.id.clone()), second.read(|n| n.id.clone()));
        let mut t = Transcript::new();
        first.append("FIRST-BODY");
        for _ in 0..3 {
            t.bind_prose(&first.clone());
            assert!(t.is_streaming());
            assert!(t.take_settled(80, &ctx).is_empty());
        }
        second.append("SECOND-BODY");
        t.bind_prose(&second);
        first.settle("FIRST-BODY".into(), None, crate::runtime::Phase::Completed);
        t.bind_prose(&first.clone());
        let mut committed = t.take_settled(80, &ctx);
        assert!(t.is_streaming(), "the second body is still running");
        assert_eq!(plain(&committed), ["FIRST-BODY"]);
        t.bind_prose(&first);
        assert!(t.take_settled(80, &ctx).is_empty());
        second.append("-TAIL");
        t.bind_prose(&second.clone());
        second.settle(
            "SECOND-BODY-TAIL".into(),
            None,
            crate::runtime::Phase::Completed,
        );
        t.bind_prose(&second);
        assert!(
            !t.is_streaming(),
            "settlement must reach the existing binding"
        );
        committed.extend(t.take_settled(80, &ctx));
        t.bind_prose(&second.clone());
        assert!(t.take_settled(80, &ctx).is_empty());
        assert_eq!(plain(&committed), ["FIRST-BODY", "", "SECOND-BODY-TAIL"]);
        assert_eq!(plain(&committed), plain(&t.all_rows(80, &ctx)));
    }

    #[test]
    fn assistant_streams_into_one_block_and_rerenders_as_it_grows() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.assistant_delta("Hello **wor");
        let (spilled, live) = t.live(40, &ctx, 20);
        assert!(spilled.is_empty());
        assert_eq!(plain(&live), ["Hello **wor", ""]);
        t.assistant_delta("ld** again");
        let (_, live) = t.live(40, &ctx, 20);
        assert_eq!(plain(&live), ["Hello world again", ""]);
        assert!(
            live[0]
                .spans
                .iter()
                .any(|s| s.content.contains("world")
                    && s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(t.take_settled(40, &ctx).is_empty(), "still streaming");
        t.finish_turn();
        assert_eq!(plain(&t.take_settled(40, &ctx)), ["Hello world again"]);
    }

    #[test]
    fn a_new_kind_of_block_ends_the_streaming_one() {
        let mut env = Env::new();
        env.ui.show_thinking = true;
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
    fn user_block_is_compact_and_has_a_prompt_marker() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.push_user("hello there, this is a fairly long message that wraps");
        let rows = t.take_settled(20, &ctx);
        let body: Vec<_> = rows.iter().collect();
        assert!(body.len() >= 4);
        for row in &body {
            assert!(text::line_width(row) <= 20);
        }
        assert!(plain(&rows)[0].starts_with("▎ › hello"));
        assert_eq!(body[0].spans[0].style.fg, env.theme.fg(Tok::Accent).fg);
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
        env.ui.show_thinking = true;
        let mut t = Transcript::new();
        t.thinking_delta("first thought\nsecond thought\n\nthird");
        t.finish_turn();
        let rows = t.all_rows(40, &env.ctx());
        assert!(plain(&rows).contains(&"▎ first thought".to_owned()));
        assert_eq!(rows[0].spans[1].style, env.theme.fg(Tok::ThinkingText));
        env.ui.show_thinking = false;
        let rows = t.all_rows(40, &env.ctx());
        assert_eq!(plain(&rows), ["› reasoning (3 lines)"]);
    }

    #[test]
    fn stats_row_is_dim_and_gated_by_setting() {
        let mut env = Env::new();
        env.ui.show_turn_stats = true;
        let mut t = Transcript::new();
        t.turn_stats(TurnStats {
            clock: "2026-10-01 10:54:32".into(),
            elapsed_secs: 26.84,
            prompt_tokens: Some(11_000),
            output_tokens: Some(59),
            tokens_per_second: Some(2.2),
            outcome: None,
        });
        let rows = t.all_rows(80, &env.ctx());
        let line = &plain(&rows)[0];
        assert!(
            line.contains("26.8s") && line.contains("11K in") && line.contains("59 out"),
            "{line}"
        );
        assert!(
            !line.contains("2026-10-01"),
            "the compact footer omits redundant clock/rate accounting"
        );
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|s| s.content.contains("26.8s") && s.style.fg == env.theme.fg(Tok::Muted).fg)
        );
        env.ui.show_turn_stats = false;
        assert!(t.all_rows(80, &env.ctx()).is_empty());
        let mut t2 = Transcript::new();
        t2.turn_stats(TurnStats {
            clock: String::new(),
            elapsed_secs: 1.0,
            prompt_tokens: None,
            output_tokens: None,
            tokens_per_second: None,
            outcome: None,
        });
        env.ui.show_turn_stats = true;
        let text = plain(&t2.all_rows(80, &env.ctx()))[0].clone();
        assert!(text.contains("1.0s") && !text.contains(" in") && !text.contains(" out"));
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
        assert!(text.iter().any(|l| l.to_uppercase().contains("HEADS UP")));
    }

    // ---- tools ----

    #[test]
    fn bash_tool_has_a_compact_header_and_output_without_bookkeeping() {
        let env = Env::new();
        let ctx = env.ctx();
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "ls -la", "");
        t.tool_end(
            "1",
            "total 0\nfile.txt\n[exit code: 0]\n[complete output: artifact://abc]",
            false,
            0.06,
        );
        let rows = t.take_settled(40, &ctx);
        let text = plain(&rows);
        assert!(
            text[0].contains("✓ Run") && text[0].contains("ls -la") && text[0].ends_with("0.06s")
        );
        assert!(text[1].contains("total 0") && text[2].contains("file.txt"));
        assert!(!text_of(&rows).contains("exit code") && !text_of(&rows).contains("artifact://"));
        // Shell bookkeeping stays out of the body; the run time sits in the header row.
        // The block ends with its separating blank row.
        assert_eq!(text.len(), 3, "{text:?}");
        assert_eq!(text::line_width(&rows[0]), 40);
        assert_eq!(
            rows[0].spans[3].style.fg,
            env.theme.fg(Tok::SectionSandbox).fg
        );
    }

    #[test]
    fn failed_tool_output_wraps_at_word_boundaries() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "probe", "");
        t.tool_end(
            "1",
            "connection refused while talking to model server",
            true,
            0.01,
        );
        let rows = t.all_rows(24, &env.ctx());
        assert_fits(&rows, 24);
        let output = plain(&rows);
        assert!(
            output
                .iter()
                .any(|line| line.contains("connection refused")),
            "{output:?}"
        );
        assert!(
            output.iter().any(|line| line.contains("while talking to")),
            "{output:?}"
        );
    }

    #[test]
    fn tool_gutter_follows_family_and_failure() {
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
        let gutter = |rows: &Rows| rows[0].spans[3].style.fg;
        assert_eq!(gutter(&pending), env.theme.fg(Tok::SectionSandbox).fg);
        assert_eq!(gutter(&ok), env.theme.fg(Tok::SectionSandbox).fg);
        assert_eq!(gutter(&err), env.theme.fg(Tok::Error).fg);
        assert!(plain(&err)[0].contains('✗'));
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
        assert!(
            compact.trim_start().starts_with(env.theme.spinner(0)),
            "{compact}"
        );
        assert!(
            b.iter()
                .any(|l| l.trim_start().starts_with(env.theme.spinner(1)) && l.contains("Read"))
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
    fn shared_result_revisions_update_cached_rows_and_output_settings_invalidate_them() {
        let mut env = Env::new();
        let tree = crate::runtime::Tree::default();
        let body = tree.start("tool", "bash", None);
        let mut t = Transcript::new();
        t.tool_start("shared", "bash", "check", "");
        t.bind_tool("shared", body.clone());
        body.append("one\ntwo\nthree\nfour\n");
        env.ui.tool_output_lines = 2;
        let first = text_of(&t.all_rows(80, &env.ctx()));
        assert!(first.contains("one"));
        assert!(!first.contains("four"));
        env.ui.tool_output_lines = 10;
        assert!(text_of(&t.all_rows(80, &env.ctx())).contains("four"));
        body.settle(
            "typed result".into(),
            Some(serde_json::json!({"ok":true})),
            crate::runtime::Phase::Completed,
        );
        t.tool_end("shared", "legacy projection", false, 0.2);
        let done = text_of(&t.all_rows(80, &env.ctx()));
        assert!(done.contains("typed result"));
        assert!(!done.contains("legacy projection"));
        assert!(!t.take_settled(80, &env.ctx()).is_empty());
        assert!(t.take_settled(80, &env.ctx()).is_empty());
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
            text.contains("▎ b") && !text.contains("▎ c") && text.contains("1 more line (Ctrl-O"),
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
        assert!(
            plain(&rows)
                .iter()
                .any(|r| r.contains("✓ Edit") && r.ends_with("0.50s"))
        );
    }

    #[test]
    fn a_failing_exit_code_is_flagged_in_the_card_header() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "cargo test", "");
        t.tool_end("1", "1 failed\n[exit code: 101]", true, 2.0);
        let rows = t.all_rows(50, &env.ctx());
        let header = &rows[0];
        let exit = header
            .spans
            .iter()
            .find(|s| s.content.contains("exit 101"))
            .expect("exit badge");
        assert_eq!(exit.style.fg, env.theme.fg(Tok::Error).fg);
        assert!(!text_of(&rows[..rows.len() - 1].to_vec()).contains("exit code"));
        assert_eq!(text::line_width(header), 50);
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
            .map(|r| r.trim_matches(|c: char| c == '▎' || c == ' '))
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
        assert!(!text_of(&rows).contains("change preview"));
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
            [" ✓ Plan    1/2 done", "  ✓ read", "  ● fix"]
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
            text.contains("Run") && text.contains("echo hi") && text.contains("out/a.txt"),
            "{text}"
        );
    }

    #[test]
    fn long_commands_are_compact_and_expand_to_the_full_target() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "bash", &"word ".repeat(80), "");
        let rows = t.all_rows(30, &env.ctx());
        assert_fits(&rows, 30);
        assert_eq!(rows.len(), 1, "{:?}", plain(&rows));
        let expanded = t.all_rows(
            30,
            &TranscriptCtx {
                tools_expanded: true,
                ..env.ctx()
            },
        );
        assert_fits(&expanded, 30);
        assert!(expanded.len() > rows.len());
        assert_eq!(
            text_of(&expanded).matches("word").count(),
            80 + text_of(&rows).matches("word").count()
        );
    }

    #[test]
    fn simple_tools_are_compact_rows() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "read", "notes.txt", "");
        t.tool_end("1", "\nfirst real line\nsecond", false, 0.2);
        let rows = t.all_rows(60, &env.ctx());
        assert_eq!(rows.len(), 1);
        assert!(
            plain(&rows)[0].contains("✓ Read")
                && plain(&rows)[0].contains("notes.txt")
                && plain(&rows)[0].contains("3 lines")
        );
        assert_eq!(
            rows[0].spans[3].style.fg,
            env.theme.fg(Tok::SectionTools).fg
        );
        assert_eq!(rows[0].spans[1].style.fg, env.theme.fg(Tok::Success).fg);
    }

    #[test]
    fn failed_simple_tool_shows_its_error_underneath() {
        let env = Env::new();
        let mut t = Transcript::new();
        t.tool_start("1", "grep", "foo", "");
        t.tool_end("1", "regex parse error\nunclosed group", true, 0.2);
        let text = plain(&t.all_rows(40, &env.ctx()));
        assert!(text[0].contains("✗ Search") && text[0].contains("foo"));
        assert!(text[1].contains("regex parse error"));
        assert!(text[2].contains("unclosed group"));
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
        assert_eq!(plain(&t.take_settled(20, &ctx)), ["a", "", "b"]);
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
            outcome: None,
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
        assert_eq!(plain(&live), ["one", "", "two", "", "three", ""]);
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
        let mut env = Env::new();
        env.ui.show_thinking = true;
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

#[cfg(test)]
mod replay_reasoning_tests {
    use super::*;
    use serde_json::json;

    fn thinking_texts(transcript: &Transcript) -> Vec<String> {
        transcript
            .blocks
            .iter()
            .filter_map(|block| match &block.kind {
                Kind::Thinking(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn replay_shows_reasoning_from_content_and_from_provider_thinking_blocks() {
        let mut transcript = Transcript::default();
        let mut replay = Replay::default();
        replay.push(
            &mut transcript,
            &json!({"role":"assistant","content":"done","reasoning_content":"plain reasoning"}),
        );
        replay.push(
            &mut transcript,
            &json!({"role":"assistant","content":"again",
                "dial_provider_content":[
                    {"type":"thinking","thinking":"typed ","signature":"s"},
                    {"type":"thinking","thinking":"reasoning","signature":"s"},
                    {"type":"text","text":"again"}]}),
        );
        replay.push(
            &mut transcript,
            &json!({"role":"assistant","content":"no thoughts"}),
        );
        assert_eq!(
            thinking_texts(&transcript),
            vec!["plain reasoning".to_owned(), "typed reasoning".to_owned()]
        );
    }
}
