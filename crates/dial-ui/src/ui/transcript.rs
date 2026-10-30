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
    tool_cards::{AgentRow, Tool, ToolState, lookup_group, lookup_tree, may_join, tool_rows},
};
use crate::llm::model_config::UiConfig;
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

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

/// How much settled history one paint may hand to scrollback. Retirement stops before the first
/// block that would exceed the budget and leaves the rest, in order, for a later paint. The first
/// block always retires, so one block larger than `rows` still makes progress.
#[derive(Clone, Copy)]
pub struct Retire {
    /// Most blocks retired by one paint.
    pub blocks: usize,
    /// Most rows retired by one paint.
    pub rows: usize,
    /// Point after which no further block retires.
    pub until: Option<Instant>,
}

impl Retire {
    /// No budget: the whole finalized backlog retires.
    pub const UNLIMITED: Self = Self {
        blocks: usize::MAX,
        rows: usize::MAX,
        until: None,
    };

    /// Budget for one paint while a turn runs: a few blocks, bounded rows, and a few milliseconds.
    pub fn frame(now: Instant) -> Self {
        Self {
            blocks: 32,
            rows: 256,
            until: Some(now + Duration::from_millis(4)),
        }
    }

    /// Whether `blocks` retired blocks and `rows` rows already exhaust this budget.
    fn spent(self, blocks: usize, rows: usize) -> bool {
        blocks >= self.blocks
            || rows >= self.rows
            || self.until.is_some_and(|at| Instant::now() >= at)
    }
}

/// A settled block whose rows are partly in scrollback. Its rows freeze at the width it started at,
/// so the remainder keeps that wrapping across resizes. `next` is the first row not yet retired.
struct Retiring {
    rows: std::sync::Arc<Rows>,
    next: usize,
    /// Exclusive block end covered by these rows, including every member of a frozen lookup tree.
    end: usize,
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
    Event(crate::agent::ui::ui_event::UiEvent),
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
    timestamp: Option<String>,
    active: bool,
    spill: Spill,
    markdown: std::cell::RefCell<StreamingRows>,
    shared: Option<crate::runtime::Body>,
    revision: std::cell::Cell<u64>,
    rendered: std::cell::RefCell<Option<(RenderKey, u64, std::sync::Arc<Rows>)>>,
    /// Visible byte prefix of a streaming answer under `ui.smooth_reveal`. `None` shows all
    /// text. Inactive blocks ignore it, so every structural boundary flushes the full text.
    revealed: Option<usize>,
    /// A lookup run may not reach back across this block's start: it opens a new turn, or the
    /// block that stood between it and its predecessor was removed.
    breaks_run: bool,
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
            timestamp: None,
            active,
            spill: Spill::default(),
            markdown: std::cell::RefCell::new(StreamingRows::default()),
            shared: None,
            revision: std::cell::Cell::new(0),
            rendered: std::cell::RefCell::new(None),
            revealed: None,
            breaks_run: false,
        }
    }

    /// The visible streaming prefix, or the complete text at a structural boundary.
    fn visible<'a>(&self, text: &'a str) -> &'a str {
        match self.revealed {
            Some(cut) if self.active => &text[..text.floor_char_boundary(cut.min(text.len()))],
            _ => text,
        }
    }

    /// Drop the render and streaming caches once the block is committed. `render` rebuilds them from
    /// the body on demand, so replay and resize output stay identical.
    fn release_caches(&self) {
        *self.rendered.borrow_mut() = None;
        *self.markdown.borrow_mut() = StreamingRows::default();
    }

    /// The call as it stands: a shared body carries the newest output, presentation and phase.
    fn tool_view(&self) -> Option<std::borrow::Cow<'_, Tool>> {
        let Kind::Tool(t) = &self.kind else {
            return None;
        };
        Some(match &self.shared {
            Some(b) => std::borrow::Cow::Owned(b.read(|n| {
                let mut view = t.clone();
                view.output = n.result.text.clone();
                if let Some(presentation) = &n.presentation {
                    view.presentation = presentation.clone();
                }
                view.state = n.result.phase;
                view
            })),
            None => std::borrow::Cow::Borrowed(t),
        })
    }

    /// Applies `f` to the call's current phase and presentation without copying its output.
    fn tool_phase<R>(
        &self,
        f: impl FnOnce(ToolState, &crate::runtime::presentation::Presentation) -> R,
    ) -> Option<R> {
        let Kind::Tool(t) = &self.kind else {
            return None;
        };
        Some(match &self.shared {
            Some(b) => b.read(|n| {
                f(
                    n.result.phase,
                    n.presentation.as_ref().unwrap_or(&t.presentation),
                )
            }),
            None => f(t.state, &t.presentation),
        })
    }

    fn prose(&self) -> Option<(Prose, std::borrow::Cow<'_, str>)> {
        match &self.kind {
            Kind::Assistant(t) => Some((
                Prose::Assistant,
                self.shared.as_ref().map_or_else(
                    || std::borrow::Cow::Borrowed(self.visible(t)),
                    |b| {
                        std::borrow::Cow::Owned(b.read(|n| self.visible(&n.result.text).to_owned()))
                    },
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
        let mut rows = match &self.kind {
            Kind::User(t) => Arc::new(user_rows(t, w, ctx)),
            Kind::Assistant(t) => match &self.shared {
                Some(b) => b.read(|n| {
                    self.markdown
                        .borrow_mut()
                        .render(self.visible(&n.result.text), w, ctx.theme)
                }),
                None => self
                    .markdown
                    .borrow_mut()
                    .render(self.visible(t), w, ctx.theme),
            },
            Kind::Thinking(t) => Arc::new(match &self.shared {
                Some(b) => b.read(|n| prose_rows(Prose::Thinking, &n.result.text, w, ctx)),
                None => prose_rows(Prose::Thinking, t, w, ctx),
            }),
            // Work rows put their own status glyph in the shared icon gutter.
            Kind::Tool(_) => Arc::new(
                self.tool_view()
                    .map_or_else(Vec::new, |view| tool_rows(&view, w, ctx)),
            ),
            Kind::Notice { title, text } => Arc::new(super::notice::rows(title, text, w, ctx)),
            Kind::Event(event) => Arc::new(super::lanes::rows(event, w, ctx)),
            Kind::Error(t) => Arc::new(error_rows(t, w, ctx)),
            Kind::Image(image) => Arc::new(image.rows(w, ctx.theme)),
            Kind::Stats(s) => Arc::new(stats_rows(s, w, ctx)),
        };
        if self.timestamp.is_some() {
            self.timestamp_rows(Arc::make_mut(&mut rows), w, ctx);
        }
        *self.rendered.borrow_mut() = Some((key, ctx.tick, Arc::clone(&rows)));
        rows
    }

    fn timestamp_rows(&self, rows: &mut Rows, width: u16, ctx: &TranscriptCtx) {
        let Some(timestamp) = &self.timestamp else {
            return;
        };
        let label = match self.kind {
            Kind::User(_) => "You",
            Kind::Assistant(_) => "Assistant",
            _ => return,
        };
        let header = format!("{label} · {} UTC", timestamp.replace('T', " "));
        rows.insert(
            0,
            Line::from(Span::styled(
                text::clip(&header, width as usize),
                ctx.theme.fg(Tok::Muted),
            )),
        );
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
            let mut rows = prose_rows(kind, &text[..end], w, ctx);
            self.timestamp_rows(&mut rows, w, ctx);
            rows.len()
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
            let mut prefix = prose_rows(kind, &text[..cut], w, ctx);
            self.timestamp_rows(&mut prefix, w, ctx);
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
            return std::sync::Arc::new(answer_rows(source, width, theme, true));
        }
        if source[self.cut..].contains(['[', '<']) || width == 0 {
            let rendered = answer_rows(&source[self.cut..], width, theme, self.prefix.is_empty());
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
            let first = self.prefix.is_empty();
            let rows = answer_rows(&tail[..cut], width, theme, first);
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
        let tail = answer_rows(&source[self.cut..], width, theme, self.prefix.is_empty());
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

/// The assistant's answer: markdown laid out past the icon gutter, its first row marked with `●`.
/// `lead` is false for a continuation of an answer whose first row is already marked. Too narrow
/// for a gutter, the markdown fills the width as is.
fn answer_rows(source: &str, width: u16, theme: &Theme, lead: bool) -> Rows {
    let gutter = super::tool_cards::GUTTER;
    let w = usize::from(width);
    if w < gutter + 12 {
        return markdown::render(source, width, theme, false);
    }
    let rows = markdown::render(source, (w - gutter) as u16, theme, false);
    let mut marked = !lead;
    rows.into_iter()
        .map(|mut row| {
            if row.spans.iter().all(|s| s.content.is_empty()) {
                return row;
            }
            let glyph = if marked {
                Span::raw(" ".repeat(gutter))
            } else {
                marked = true;
                Span::styled(in_gutter(theme.sym(Sym::Bullet)), theme.fg(Tok::Accent))
            };
            row.spans.insert(0, glyph);
            row
        })
        .collect()
}

/// A user message is a shaded band across the full width: a row of padding, the text after the
/// `›` prompt in the icon gutter, and another row of padding, so turns are easy to find when
/// scrolling back.
fn user_rows(raw: &str, w: u16, ctx: &TranscriptCtx) -> Rows {
    let w = w as usize;
    let clean = text::sanitize(raw);
    let body = clean.trim();
    if w == 0 || body.is_empty() {
        return Vec::new();
    }
    let theme = ctx.theme;
    let tint = theme.bg(Tok::UserBg);
    let prompt_style = theme
        .fg(Tok::Accent)
        .patch(tint)
        .add_modifier(Modifier::BOLD);
    let text_style = theme.on(Tok::UserText, Tok::UserBg);
    let tinted = |spans: Vec<Span<'static>>| {
        let mut line = fitted(spans, w, theme);
        let used = text::line_width(&line);
        if used < w {
            line.spans.push(Span::styled(" ".repeat(w - used), tint));
        }
        line
    };
    let gutter = super::tool_cards::GUTTER;
    let mut rows = Vec::new();
    if w < gutter + 4 {
        let flat = body.replace('\n', " ");
        rows.push(tinted(vec![
            Span::styled(format!("{} ", theme.sym(Sym::Prompt)), prompt_style),
            Span::styled(flat, text_style),
        ]));
        return rows;
    }
    // Padding rows only when the theme tints the band; untinted they would read as stray gaps.
    let padded = tint.bg.is_some_and(|c| c != ratatui::style::Color::Reset);
    if padded {
        rows.push(tinted(Vec::new()));
    }
    let inner = w - gutter - 1;
    for (i, line) in text::wrap_words(body, inner).into_iter().enumerate() {
        let marker = format!(" {}", if i == 0 { theme.sym(Sym::Prompt) } else { "" });
        let pad = gutter.saturating_sub(text::width(&marker));
        rows.push(tinted(vec![
            Span::styled(format!("{marker}{}", " ".repeat(pad)), prompt_style),
            Span::styled(text::clip(&line, inner), text_style),
        ]));
    }
    if padded {
        rows.push(tinted(Vec::new()));
    }
    rows
}

fn prose_rows(kind: Prose, raw: &str, w: u16, ctx: &TranscriptCtx) -> Rows {
    match kind {
        Prose::Assistant => answer_rows(raw, w, ctx.theme, true),
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
            "{}reasoning ({n} line{})",
            in_gutter(ctx.theme.sym(Sym::Pointer)),
            if n == 1 { "" } else { "s" }
        );
        return vec![fitted(vec![Span::styled(label, style)], w, ctx.theme)];
    }
    // Reasoning hangs off a quote bar in the icon gutter, quieter than the answer's `●`.
    let rail = in_gutter(ctx.theme.sym(Sym::Quote));
    let lead = text::width(&rail);
    if w <= lead + 4 {
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
                Span::styled(rail.clone(), ctx.theme.fg(Tok::Border)),
                Span::styled(text::clip(&row, w - lead), style),
            ])
        })
        .collect()
}

/// `mark` padded to the transcript's icon gutter, so the text after it starts at the content column.
pub(super) fn in_gutter(mark: &str) -> String {
    let gutter = super::tool_cards::GUTTER;
    format!(
        "{mark}{}",
        " ".repeat(gutter.saturating_sub(text::width(mark)).max(1))
    )
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
    let glyph = in_gutter(theme.sym(Sym::Cross2));
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

/// The turn's closing line: how it ended (its mark in the icon gutter), what it changed, how long
/// it took and what it cost, with the follow-up commands at the right edge when files changed.
fn stats_rows(s: &TurnStats, w: u16, ctx: &TranscriptCtx) -> Rows {
    let timing = ctx.ui.show_turn_stats;
    if w == 0 || (!timing && s.outcome.is_none()) {
        return Vec::new();
    }
    let theme = ctx.theme;
    let w = w as usize;
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let gutter = |mark: &str, style: ratatui::style::Style| Span::styled(in_gutter(mark), style);
    let mut left: Vec<Span<'static>> = vec![gutter(theme.sym(Sym::Dot), theme.fg(Tok::Dim))];
    let push = |left: &mut Vec<Span<'static>>, text: String, style| {
        if left.len() > 1 {
            left.push(Span::styled(dot.clone(), theme.fg(Tok::Dim)));
        }
        left.push(Span::styled(text, style));
    };
    let mut changed = 0;
    if let Some(o) = &s.outcome {
        // An outcome the agent did not name is reported as unknown, never as a failure or success.
        let known = matches!(
            o.status.as_str(),
            "running"
                | "pending"
                | "done"
                | "completed"
                | "blocked"
                | "needs_input"
                | "incomplete"
                | "failed"
                | "interrupted"
                | "cancelled"
                | "canceled"
        );
        let phase = crate::runtime::Phase::from_status(&o.status);
        let word = if known { phase.label() } else { "unknown" };
        let (tok, mark) = match phase {
            _ if !known => (Tok::Warning, Sym::Warn),
            crate::runtime::Phase::Completed => (Tok::Success, Sym::Check),
            crate::runtime::Phase::Failed => (Tok::Error, Sym::Cross2),
            crate::runtime::Phase::Interrupted => (Tok::Muted, Sym::Cross2),
            crate::runtime::Phase::Running => (Tok::Accent, Sym::Working),
            _ => (Tok::Warning, Sym::Warn),
        };
        // The outcome's mark takes the gutter so the end of a turn is findable at a glance.
        left[0] = gutter(theme.sym(mark), theme.fg(tok));
        push(
            &mut left,
            word.to_owned(),
            theme.fg(tok).add_modifier(Modifier::BOLD),
        );
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
        if let Some(rate) = s.tokens_per_second.filter(|r| r.is_finite() && *r > 0.0) {
            push(&mut left, format!("{rate:.1} tok/s"), theme.fg(Tok::Muted));
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
    if right_w > 0 && left_w + right_w + 4 <= w {
        left.push(Span::raw(" ".repeat(w - left_w - right_w - 1)));
        left.extend(right);
        left.push(Span::raw(" "));
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
    /// The settled block split at a row cursor, if one is in progress. See `retire_settled`.
    retiring: Option<Retiring>,
    /// How the running turn ended, waiting for the closing stats row that shows it.
    outcome: Option<TurnOutcome>,
    /// A turn just ended: a trailing lookup run is complete, and the next block starts a new run.
    sealed: bool,
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
            Kind::Event(event) => event.kind.lane() != crate::agent::ui::ui_event::Lane::Divider,
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

    /// The lookup run block `index` belongs to, as `(start, end, label)`: consecutive settled
    /// reads, searches or finds of one kind, never across a finished turn. Only the collapsed view
    /// groups; expanded and hidden tool views keep each call.
    fn lookup_run(&self, index: usize, ctx: &TranscriptCtx) -> Option<(usize, usize, String)> {
        if !ctx.ui.show_tools || ctx.tools_expanded {
            return None;
        }
        let label = self.lookup_label(index)?;
        let same = |i: usize| self.lookup_label(i).as_deref() == Some(label.as_str());
        let mut start = index;
        while start > 0 && !self.blocks[start].breaks_run && same(start - 1) {
            start -= 1;
        }
        let mut end = index + 1;
        while end < self.blocks.len() && !self.blocks[end].breaks_run && same(end) {
            end += 1;
        }
        Some((start, end, label))
    }

    fn lookup_label(&self, index: usize) -> Option<String> {
        self.blocks[index].tool_phase(lookup_group).flatten()
    }

    /// Whether block `index` heads a lookup run that may still grow: nothing after it has closed
    /// the run yet, or the next call is a running lookup of the same kind.
    fn lookup_waits(&self, index: usize, ctx: &TranscriptCtx) -> bool {
        let Some((start, end, label)) = self.lookup_run(index, ctx) else {
            return false;
        };
        if start != index {
            return false;
        }
        match self.blocks.get(end) {
            None => !self.sealed,
            Some(next) => {
                !next.breaks_run
                    && next
                        .tool_phase(|state, p| may_join(state, p, &label))
                        .unwrap_or(false)
            }
        }
    }

    /// Block `index`'s rows. A run of two or more lookups renders as one tree at its first block
    /// and as nothing at the others, so every path that lays out blocks agrees on the grouping.
    fn rows_at(&self, index: usize, w: u16, ctx: &TranscriptCtx) -> std::sync::Arc<Rows> {
        self.rows_in_range(index, w, ctx, 0..self.blocks.len())
    }

    /// A scrollback boundary may bisect a lookup run after Ctrl+O changes. Never include calls
    /// outside the boundary being rendered, or suppress an unretired call behind a retired head.
    fn rows_in_range(
        &self,
        index: usize,
        w: u16,
        ctx: &TranscriptCtx,
        range: std::ops::Range<usize>,
    ) -> std::sync::Arc<Rows> {
        if let Some((start, end, _)) = self.lookup_run(index, ctx) {
            let start = start.max(range.start);
            let end = end.min(range.end);
            if end - start <= 1 {
                return self.blocks[index].render(w, ctx);
            }
            if index != start {
                return Arc::new(Vec::new());
            }
            let views: Vec<_> = self.blocks[start..end]
                .iter()
                .filter_map(Block::tool_view)
                .collect();
            let tools: Vec<&Tool> = views.iter().map(|v| v.as_ref()).collect();
            return Arc::new(lookup_tree(&tools, w, ctx));
        }
        self.blocks[index].render(w, ctx)
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
        self.push_block(block);
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
            retiring: None,
            outcome: None,
            sealed: false,
        }
    }

    /// Whether the conversation has a user message (typed, replayed or resumed).
    pub fn has_user(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b.kind, Kind::User(_)))
    }

    /// How many blocks the conversation holds; it grows whenever something new is shown.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
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
        self.push_block(Block::new(kind, active));
    }

    /// Appends a block. The first block after a finished turn starts a new lookup run.
    fn push_block(&mut self, mut block: Block) {
        block.breaks_run = std::mem::take(&mut self.sealed);
        self.blocks.push(block);
    }

    pub fn push_user(&mut self, text: &str) {
        self.close_streaming_text();
        self.push(Kind::User(text.to_owned()), false);
    }

    /// Append live user text with its observed UTC submission time.
    pub fn push_user_at(&mut self, text: &str, timestamp: &str) {
        let first = self.blocks.len();
        self.push_user(text);
        self.stamp_blocks(first, timestamp);
    }

    /// Append live assistant text, timestamping only a newly created answer block.
    pub fn assistant_delta_at(&mut self, text: &str, timestamp: &str) {
        let first = self.blocks.len();
        self.assistant_delta(text);
        self.stamp_blocks(first, timestamp);
    }

    /// Bind a live answer body without changing its timestamp on subsequent updates.
    pub fn bind_prose_at(&mut self, body: &crate::runtime::Body, timestamp: &str) {
        let first = self.blocks.len();
        self.bind_prose(body);
        self.stamp_blocks(first, timestamp);
    }

    fn stamp_blocks(&mut self, first: usize, timestamp: &str) {
        let Some(time) = timestamp
            .get(..19)
            .filter(|time| crate::platform::time::parse_utc(time).is_some())
        else {
            return;
        };
        if !timestamp.ends_with('Z') {
            return;
        }
        for block in &mut self.blocks[first..] {
            if matches!(block.kind, Kind::User(_) | Kind::Assistant(_)) {
                block.timestamp = Some(time.to_owned());
                block.revision.set(block.revision.get().saturating_add(1));
            }
        }
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

    /// Advance the visible prefix of the active answer for `ui.smooth_reveal`. Disabling the
    /// setting drops the prefix so pending text shows at once; inactive blocks always show all text.
    /// Called from input and heartbeat ticks rather than from `live`, so the reveal keeps moving
    /// while the provider is quiet.
    pub fn advance_reveal(&mut self, ui: &UiConfig) {
        let Some(block) = self.blocks[self.cursor..]
            .iter_mut()
            .rev()
            .find(|block| block.active && matches!(&block.kind, Kind::Assistant(_)))
        else {
            return;
        };
        let Kind::Assistant(text) = &block.kind else {
            return;
        };
        if !block.active {
            return;
        }
        let from = block.revealed.unwrap_or(0);
        let next = ui.smooth_reveal.then(|| match &block.shared {
            Some(body) => body.read(|node| Self::reveal_cut(&node.result.text, from)),
            None => Self::reveal_cut(text, from),
        });
        if block.revealed == next {
            return;
        }
        block.revealed = next;
        block.revision.set(block.revision.get().saturating_add(1));
    }

    /// Next visible byte cut: a quarter of the pending text (at least a few bytes), moved forward to a
    /// grapheme boundary and never past the newest grapheme, which a following combining mark may join.
    fn reveal_cut(text: &str, from: usize) -> usize {
        const REVEAL_MIN_BYTES: usize = 8;
        let from = text.floor_char_boundary(from.min(text.len()));
        let want = from + ((text.len() - from) / 4).max(REVEAL_MIN_BYTES);
        let limit = text
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(at, _)| at);
        text[from..]
            .grapheme_indices(true)
            .map(|(at, _)| from + at)
            .find(|&at| at >= want)
            .unwrap_or(limit)
            .min(limit)
            .max(from)
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
        // A card already showing this call's arguments as they streamed in becomes the started
        // call in place, so the transcript never holds two cards for one call.
        if let Some(tool) = self.live_running_tool(id) {
            tool.name = name.to_owned();
            tool.summary = summary.to_owned();
            tool.presentation = crate::agent::legacy_tool_presentation(name, summary, input);
            tool.input = input.to_owned();
            tool.progress = None;
            self.invalidate_tail();
            return;
        }
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

    /// Show a call's arguments while the model is still writing them. The card is observational:
    /// its `input` and `progress` hold the partial JSON text for display, and nothing dispatches
    /// from it. `tool_start` replaces it with the completed call, which is the only executable form.
    pub fn tool_arguments_delta(&mut self, id: &str, name: &str, delta: &str) {
        if id.is_empty() {
            return;
        }
        self.invalidate_tail();
        if let Some(tool) = self.live_running_tool(id) {
            Self::append_partial(&mut tool.input, delta);
            tool.progress = Some(Self::tail_within(&tool.input, Self::PREVIEW_LIMIT).to_owned());
            return;
        }
        self.close_streaming_text();
        let mut input = String::new();
        Self::append_partial(&mut input, delta);
        let progress = Some(Self::tail_within(&input, Self::PREVIEW_LIMIT).to_owned());
        self.push(
            Kind::Tool(Tool {
                id: id.to_owned(),
                name: name.to_owned(),
                summary: name.to_owned(),
                presentation: crate::agent::legacy_tool_presentation(name, name, ""),
                input,
                state: ToolState::Running,
                output: String::new(),
                progress,
                elapsed: None,
                todo: None,
                agents: Vec::new(),
            }),
            true,
        );
    }

    /// The running card for call `id` in the live tail, if one exists.
    fn live_running_tool(&mut self, id: &str) -> Option<&mut Tool> {
        self.blocks[self.cursor..]
            .iter_mut()
            .rev()
            .find_map(|b| match &mut b.kind {
                Kind::Tool(t) if t.id == id && t.state == ToolState::Running => Some(t),
                _ => None,
            })
    }

    /// Retain the newest partial argument text, so a long streamed write stays bounded.
    fn append_partial(target: &mut String, text: &str) {
        target.push_str(text);
        let keep = Self::tail_within(target.as_str(), Self::PARTIAL_ARGS_LIMIT).len();
        let cut = target.len() - keep;
        target.drain(..cut);
    }

    /// The last `limit` bytes of `text`, cut at a character boundary.
    fn tail_within(text: &str, limit: usize) -> &str {
        let mut cut = text.len().saturating_sub(limit);
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        &text[cut..]
    }

    /// Bounds for partial argument text: `PARTIAL_ARGS_LIMIT` is retained, `PREVIEW_LIMIT` shown.
    const PARTIAL_ARGS_LIMIT: usize = 64 * 1024;
    const PREVIEW_LIMIT: usize = 16 * 1024;

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
    pub fn tool_todo(&mut self, id: &str, tasks: Vec<(crate::agent::workflow::todo::Task, bool)>) {
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
    pub fn event(&mut self, event: &crate::agent::ui::ui_event::UiEvent) {
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
                    // Its neighbours were never adjacent, so their lookup runs stay apart.
                    match self.blocks.get_mut(index) {
                        Some(next) => next.breaks_run = true,
                        None => self.sealed = true,
                    }
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
            }
            block.active = false;
        }
        self.sealed = true;
    }

    #[cfg(test)]
    pub fn is_streaming(&self) -> bool {
        self.blocks[self.cursor..].iter().any(|b| b.active)
    }

    /// Whether the active answer has another reveal step. The newest grapheme waits for more text.
    pub fn has_pending_reveal(&self) -> bool {
        let Some(Block {
            kind: Kind::Assistant(text),
            active: true,
            shared,
            revealed: Some(cut),
            ..
        }) = self.blocks[self.cursor..]
            .iter()
            .rev()
            .find(|block| block.active && matches!(&block.kind, Kind::Assistant(_)))
        else {
            return false;
        };
        match shared {
            Some(body) => body.read(|node| Self::reveal_cut(&node.result.text, *cut) > *cut),
            None => Self::reveal_cut(text, *cut) > *cut,
        }
    }

    /// Finalized blocks not yet handed to scrollback.
    pub fn settled_backlog(&self) -> usize {
        self.blocks[self.cursor..]
            .iter()
            .take_while(|block| !block.active)
            .count()
    }

    pub fn has_settled_backlog(&self) -> bool {
        self.blocks
            .get(self.cursor)
            .is_some_and(|block| !block.active)
    }

    /// Rows of finalized blocks not handed out yet, each row exactly once; stops at the first block
    /// that is still active. Every block that has rows ends with one blank separator row. A block that
    /// was partly spilled by [`live`](Self::live) contributes only its remainder.
    #[cfg(test)]
    pub fn take_settled(&mut self, width: u16, ctx: &TranscriptCtx) -> Rows {
        self.take_settled_with_images(width, ctx).0
    }

    /// Native overlays are anchored to the separator after the preview and emitted once, together
    /// with settled history. Portable cells remain underneath as a fallback. Retires the whole backlog.
    pub fn take_settled_with_images(
        &mut self,
        width: u16,
        ctx: &TranscriptCtx,
    ) -> (Rows, Vec<(usize, super::images::NativeImage)>) {
        self.retire_settled(width, ctx, Retire::UNLIMITED)
    }

    /// Like [`take_settled_with_images`](Self::take_settled_with_images), but retires at most `retire`'s
    /// budget per call. A block that does not fit is split at a row cursor: its rows freeze at the width
    /// they started at and the remainder waits in `self.retiring`, so each row reaches scrollback exactly
    /// once and the remainder keeps its wrapping across resizes. A native image is indivisible and waits
    /// whole for a call with room. Every call makes progress by at least one row.
    pub fn retire_settled(
        &mut self,
        width: u16,
        ctx: &TranscriptCtx,
        retire: Retire,
    ) -> (Rows, Vec<(usize, super::images::NativeImage)>) {
        let mut out: Rows = Vec::new();
        let mut images = Vec::new();
        let mut finished = 0;
        while self.cursor < self.blocks.len() {
            let index = self.cursor;
            if self.blocks[index].active {
                break;
            }
            if (finished > 0 || !out.is_empty()) && retire.spent(finished, out.len()) {
                break;
            }
            // A lookup run that may still grow waits, so its tree reaches scrollback whole.
            if self.retiring.is_none() && self.lookup_waits(index, ctx) {
                break;
            }
            let tool = self.blocks[index].is_tool();
            let (frozen, next, block_end) = match &self.retiring {
                Some(state) => (std::sync::Arc::clone(&state.rows), state.next, state.end),
                None => {
                    let end = self
                        .lookup_run(index, ctx)
                        .map_or(index + 1, |(_, end, _)| end);
                    (
                        self.rows_in_range(index, width, ctx, index..self.blocks.len()),
                        self.blocks[index].emitted(width, ctx),
                        end,
                    )
                }
            };
            let native = match &self.blocks[index].kind {
                Kind::Image(image) if next == 0 && !frozen.is_empty() => image.native(width),
                _ => None,
            };
            let gap = next == 0
                && !frozen.is_empty()
                && self.owed_gap.is_some_and(|prev| gap_between(prev, tool));
            let needed = if frozen.is_empty() {
                0
            } else if native.is_some() {
                usize::from(gap) + frozen.len() + 1
            } else {
                usize::from(gap) + 1
            };
            if !out.is_empty() && out.len() + needed > retire.rows {
                break;
            }
            if let Some(native) = native {
                if gap {
                    out.push(blank_row());
                }
                images.push((out.len() + frozen.len(), native));
                out.extend(frozen.iter().cloned());
                // Native graphics draw upward from a reserved separator cell. It must exist even when
                // this image is the last settled block before the live composer.
                out.push(blank_row());
                self.owed_gap = None;
            } else if !frozen.is_empty() {
                if gap {
                    out.push(blank_row());
                }
                let room = retire.rows.saturating_sub(out.len()).max(1);
                let end = frozen.len().min(next.saturating_add(room));
                out.extend(frozen[next..end].iter().cloned());
                if end < frozen.len() {
                    self.retiring = Some(Retiring {
                        rows: frozen,
                        next: end,
                        end: block_end,
                    });
                    break;
                }
                self.owed_gap = Some(tool);
            }
            self.retiring = None;
            for block in &self.blocks[index..block_end] {
                block.release_caches();
            }
            self.cursor = block_end;
            finished += block_end - index;
        }
        (out, images)
    }

    /// `(spilled, live)`: rows that must be committed to scrollback now (the stable head of the
    /// streaming block when the tail is taller than `max_rows`, each row returned once) and the rows
    /// to repaint, at most `max_rows`.
    pub fn live(&mut self, width: u16, ctx: &TranscriptCtx, max_rows: usize) -> (Rows, Rows) {
        /// One block's rows in the tail, newest first. `skip` rows were already retired.
        struct Piece {
            head: bool,
            tool: bool,
            rows: std::sync::Arc<Rows>,
            skip: usize,
        }
        /// Joins the tail oldest first, with the same separators `document` uses.
        fn assemble(pieces: &[Piece], owed: Option<bool>, max_rows: usize) -> (Rows, usize) {
            let mut total = 0usize;
            let mut previous = None;
            for piece in pieces.iter().rev() {
                let before = if piece.head {
                    owed.filter(|_| piece.skip == 0)
                } else {
                    previous
                };
                total += usize::from(before.is_some_and(|kind| gap_between(kind, piece.tool)));
                total += piece.rows.len().saturating_sub(piece.skip);
                previous = Some(piece.tool);
            }
            let trailing = previous.or(owed).is_some();
            total += usize::from(trailing);
            let mut out = Rows::with_capacity(total.min(max_rows));
            let mut hidden = total.saturating_sub(max_rows);
            let mut append = |rows: &[Line<'static>]| {
                let skipped = hidden.min(rows.len());
                hidden -= skipped;
                out.extend(rows[skipped..].iter().cloned());
            };
            previous = None;
            for piece in pieces.iter().rev() {
                let before = if piece.head {
                    owed.filter(|_| piece.skip == 0)
                } else {
                    previous
                };
                if before.is_some_and(|kind| gap_between(kind, piece.tool)) {
                    append(std::slice::from_ref(&blank_row()));
                }
                append(&piece.rows[piece.skip.min(piece.rows.len())..]);
                previous = Some(piece.tool);
            }
            if trailing {
                append(std::slice::from_ref(&blank_row()));
            }
            (out, total)
        }

        let Some(first) = self.blocks.get_mut(self.cursor) else {
            // Idle: the owed separator keeps the last block off the composer.
            let gap = if self.owed_gap.is_some() {
                vec![blank_row()]
            } else {
                Vec::new()
            };
            return (Vec::new(), gap);
        };
        let cursor = self.cursor;
        let head_tool = first.is_tool();
        let active = first.active;
        let owed = self.owed_gap;
        // A streaming head may spill and a partly retired head keeps frozen rows, so both are fixed up
        // front. Any other head renders only when the visible tail reaches it.
        let pre: Option<(std::sync::Arc<Rows>, usize)> = match &self.retiring {
            Some(state) => Some((std::sync::Arc::clone(&state.rows), state.next)),
            None if active => Some((first.render(width, ctx), first.emitted(width, ctx))),
            None => None,
        };

        // Compose from the newest block back, stopping once the rows shown are full. Blocks above
        // that point, including history far above the view, are never rendered.
        let mut pieces: Vec<Piece> = Vec::new();
        let mut visible = 0;
        let mut index = self.blocks.len();
        while index > cursor && visible < max_rows {
            index -= 1;
            if self
                .retiring
                .as_ref()
                .is_some_and(|state| index > cursor && index < state.end)
            {
                continue;
            }
            let head = index == cursor;
            let (rows, skip) = match (head, &pre) {
                (true, Some((rows, next))) => (std::sync::Arc::clone(rows), *next),
                (true, None) => (
                    self.rows_in_range(index, width, ctx, cursor..self.blocks.len()),
                    self.blocks[index].emitted(width, ctx),
                ),
                (false, _) => {
                    let floor = self.retiring.as_ref().map_or(cursor, |state| state.end);
                    (
                        self.rows_in_range(index, width, ctx, floor..self.blocks.len()),
                        0,
                    )
                }
            };
            if rows.is_empty() && skip == 0 {
                continue;
            }
            visible += rows.len().saturating_sub(skip);
            pieces.push(Piece {
                head,
                tool: self.blocks[index].is_tool(),
                rows,
                skip,
            });
        }

        let mut spilled = Vec::new();
        let (mut display, total) = assemble(&pieces, owed, max_rows);
        if total > max_rows
            && active
            && let Some((head_rows, head_skip)) = &pre
            && let Some((src, rows)) =
                self.blocks[cursor].spill_point(width, ctx, head_rows, *head_skip)
        {
            if *head_skip == 0 && owed.is_some_and(|prev| gap_between(prev, head_tool)) {
                spilled.push(blank_row());
            }
            spilled.extend(head_rows[*head_skip..rows].iter().cloned());
            self.owed_gap = None;
            self.blocks[cursor].spill = Spill {
                src,
                rows,
                key: Some((width, ctx.ui.show_thinking)),
            };
            if let Some(piece) = pieces.iter_mut().find(|piece| piece.head) {
                piece.skip = rows;
            }
            display = assemble(&pieces, owed, max_rows).0;
        }
        (spilled, display)
    }

    /// Everything rendered from scratch (settled, spilled and active), for a display-reset replay.
    /// Also returns, for each block, the row where it starts, so a viewer can jump to a block.
    pub fn document(&self, width: u16, ctx: &TranscriptCtx) -> (Rows, Vec<usize>) {
        let mut out = Vec::new();
        let mut anchors = Vec::with_capacity(self.blocks.len());
        let mut prev: Option<bool> = None;
        for (index, block) in self.blocks.iter().enumerate() {
            let rows = self.rows_at(index, width, ctx);
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
            let rows = if i < self.cursor {
                self.rows_in_range(i, width, ctx, 0..self.cursor)
            } else {
                self.rows_in_range(i, width, ctx, self.cursor..self.blocks.len())
            };
            let gap = prev.is_some_and(|p| gap_between(p, block.is_tool()));
            if i < self.cursor {
                if !rows.is_empty() {
                    if gap {
                        out.push(blank_row());
                    }
                    out.extend(rows.iter().cloned());
                    prev = Some(block.is_tool());
                }
            } else if let Some(state) = self.retiring.as_ref() {
                if gap {
                    out.push(blank_row());
                }
                out.extend(state.rows.iter().take(state.next).cloned());
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
                // A drafted message shows what the user typed, not the prompt sent in its place.
                let text = crate::storage::session::typed_text(message)
                    .or_else(|| message["content"].as_str());
                if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
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
        if let Some(timestamp) = message["dial_timestamp"].as_str() {
            transcript.stamp_blocks(first, timestamp);
        }
        first
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod replay_reasoning_tests;
