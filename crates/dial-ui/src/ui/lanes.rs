//! Controller event lanes. A context fact is one quiet line beside the work it describes; an
//! alert keeps the same shape in the warning color; a divider is a centered rule across the
//! transcript marking where earlier history was summarised. None of them carries a heading, so
//! the conversation itself stays the loudest thing on screen.
use super::{
    Rows, text,
    theme::Tok,
    transcript::{TranscriptCtx, fitted},
};
use crate::agent::ui::ui_event::{EventKind, Lane, UiEvent};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

/// Context and alert lines align with tool cards: the glyph in the transcript's icon gutter, then
/// the label padded to the tool-verb column so targets and sentences start at the same x.
const GUTTER: usize = super::tool_cards::GUTTER;
const LABEL_W: usize = 7;

fn icon(kind: EventKind, ascii: bool) -> &'static str {
    use EventKind::*;
    match (kind, ascii) {
        (Steering, false) => "↳",
        (Skill | Tools, false) => "◆",
        (Plan, false) => "▤",
        (Coach, false) => "◇",
        (Hook, false) => "⚑",
        (Verification, false) => "✓",
        (Investigation, false) => "⌕",
        (Model, false) => "⇄",
        (Recovery, false) => "▲",
        (Learning, false) => "✦",
        (Subagent, false) => "◎",
        (Continue, false) => "↻",
        (Compaction | ContextReset, false) => "⟲",
        (Draft, false) => "✎",
        (Recovery, true) => "!",
        (Steering, true) => ">",
        (Compaction | ContextReset, true) => "~",
        (_, true) => "*",
    }
}

fn icon_tok(kind: EventKind) -> Tok {
    use EventKind::*;
    match kind {
        Recovery => Tok::Warning,
        Skill | Tools | Learning => Tok::SectionLearn,
        Steering | Continue | Plan | Draft => Tok::Accent,
        Verification => Tok::Success,
        Hook | Model | Subagent => Tok::SectionConnect,
        _ => Tok::Muted,
    }
}

/// Rows for one controller event, laid out by its lane.
pub(super) fn rows(event: &UiEvent, w: u16, ctx: &TranscriptCtx) -> Rows {
    let w = w as usize;
    if w == 0 || event.text.trim().is_empty() {
        return Vec::new();
    }
    match event.kind.lane() {
        Lane::Divider => divider(event, w, ctx),
        Lane::Context | Lane::Alert => line(event, w, ctx),
    }
}

/// `↳ steer   Steering delivered to the running response.` A multi-line body (an approved
/// plan) shows its first line; the rest follows under the expand toggle.
fn line(event: &UiEvent, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let clean = text::sanitize(&event.text);
    let mut lines = clean.trim().lines().filter(|line| !line.trim().is_empty());
    let first = lines.next().unwrap_or_default().trim().to_owned();
    let rest: Vec<&str> = lines.collect();
    let glyph = icon(event.kind, theme.ascii());
    let label = event.kind.label();
    let alert = event.kind.lane() == Lane::Alert;
    let label_style = if alert {
        theme.fg(Tok::Warning)
    } else {
        theme.fg(Tok::Muted)
    };
    let body_style = if alert {
        theme.fg(Tok::Text)
    } else {
        theme.fg(Tok::Muted)
    };
    // Hanging indent under the text, past glyph and label.
    let lead = GUTTER.saturating_sub(text::width(glyph)).max(1);
    let hang = text::width(glyph) + lead + LABEL_W.max(text::width(label)) + 1;
    let room = w.saturating_sub(hang).max(8);
    let mut rows = Vec::new();
    for (i, part) in text::wrap_words(&first, room).into_iter().enumerate() {
        let mut spans = Vec::new();
        if i == 0 {
            spans.push(Span::styled(
                glyph.to_owned(),
                theme.fg(icon_tok(event.kind)),
            ));
            spans.push(Span::raw(" ".repeat(lead)));
            spans.push(Span::styled(format!("{label:<LABEL_W$}"), label_style));
            spans.push(Span::raw(" "));
        } else {
            spans.push(Span::raw(" ".repeat(hang)));
        }
        spans.push(Span::styled(part, body_style));
        rows.push(fitted(spans, w, theme));
    }
    if !rest.is_empty() {
        if ctx.tools_expanded {
            for line in &rest {
                for part in text::wrap_words(line.trim_end(), room) {
                    rows.push(fitted(
                        vec![
                            Span::raw(" ".repeat(hang)),
                            Span::styled(part, theme.fg(Tok::Dim)),
                        ],
                        w,
                        theme,
                    ));
                }
            }
        } else {
            // Same wording as a tool card's hidden output, so the expand key reads one way.
            rows.push(fitted(
                vec![
                    Span::raw(" ".repeat(hang)),
                    super::tool_cards::hidden_hint(rest.len(), ctx.expand_key, theme),
                ],
                w,
                theme,
            ));
        }
    }
    rows
}

fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", (n + 500) / 1_000)
    } else {
        n.to_string()
    }
}

/// `──── ⟲ compacted · 172k → 31k · Ctrl+O ────`, with the full report under the expand toggle.
fn divider(event: &UiEvent, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let ascii = theme.ascii();
    let rule = if ascii { "-" } else { "─" };
    let arrow = if ascii { "->" } else { "→" };
    let dot = theme.sym(super::theme::Sym::Dot);
    let mut label = format!("{} {}", icon(event.kind, ascii), event.kind.label());
    if let Some((before, after)) = event.tokens {
        label.push_str(&format!(
            " {dot} {} {arrow} {}",
            compact_tokens(before),
            compact_tokens(after)
        ));
    }
    if !ctx.tools_expanded {
        label.push_str(&format!(" {dot} {}", ctx.expand_key));
    }
    let label = text::clip(&label, w.saturating_sub(4).max(1));
    let side = w.saturating_sub(text::width(&label) + 2);
    let left = side / 2;
    let right = side - left;
    let mut rows = vec![Line::from(vec![
        Span::styled(rule.repeat(left), theme.fg(Tok::BorderMuted)),
        Span::raw(" "),
        Span::styled(label, theme.fg(Tok::Muted).add_modifier(Modifier::ITALIC)),
        Span::raw(" "),
        Span::styled(rule.repeat(right), theme.fg(Tok::BorderMuted)),
    ])];
    if ctx.tools_expanded {
        let clean = text::sanitize(&event.text);
        for line in clean.trim().lines() {
            for part in text::wrap_words(line, w.saturating_sub(4).max(8)) {
                rows.push(fitted(
                    vec![Span::raw("  "), Span::styled(part, theme.fg(Tok::Dim))],
                    w,
                    theme,
                ));
            }
        }
    }
    rows
}
