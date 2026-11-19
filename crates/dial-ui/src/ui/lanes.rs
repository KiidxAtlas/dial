//! Controller event lanes. A context fact is one quiet line beside the work it describes; an
//! alert (the loop recovering from a stall, a retry or an output limit) is a small amber card so
//! it is not missed; a divider is a centered rule across the transcript marking where earlier
//! history was summarised. Events with structured detail read as their parts: a model switch is
//! `from → to · why`, an advisor review is a quoted card marked unverified, and a configured check
//! that did not pass names the check and its status in the failure color.
use super::{
    Rows, text,
    theme::{Sym, Theme, Tok},
    transcript::{TranscriptCtx, fitted},
};
use crate::agent::ui::ui_event::{EventDetail, EventKind, Lane, SwitchReason, UiEvent};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

/// Context and alert lines align with tool cards: the glyph in the transcript's icon gutter, then
/// the label padded to the tool-verb column so targets and sentences start at the same x.
const GUTTER: usize = super::tool_cards::GUTTER;
const LABEL_W: usize = 7;
/// Narrowest width that frames an alert or a review; narrower ones keep the one-line shape.
const CARD_MIN: usize = 32;
/// Body rows a collapsed alert card shows before the expand hint.
const ALERT_ROWS: usize = 3;

/// The mark of a context-loading fact (skills, tool schemas), shared with the tool card that
/// loaded them so both read as one kind of thing.
pub(super) fn context_glyph(theme: &Theme) -> &'static str {
    if theme.ascii() { "*" } else { "◆" }
}

/// A lane's identity icon. States that have a shared symbol (a passed check, a recovery warning,
/// coach reasoning) take it from the theme, so overrides and presets apply; the other kinds have no
/// theme symbol of their own and keep a fixed identity glyph with an ASCII fallback.
fn icon(kind: EventKind, theme: &Theme) -> String {
    use EventKind::*;
    let ascii = theme.ascii();
    let fixed = match (kind, ascii) {
        (Verification, _) => return theme.sym(Sym::Check).to_owned(),
        (Recovery, _) => return theme.sym(Sym::Warn).to_owned(),
        (Coach, _) => return theme.sym(Sym::Reasoning).to_owned(),
        (Skill | Tools, _) => context_glyph(theme),
        (Steering, false) => "↳",
        (Plan, false) => "▤",
        (Hook, false) => "⚑",
        (Investigation, false) => "⌕",
        (Model, false) => "⇄",
        (Learning, false) => "✦",
        (Subagent, false) => "◎",
        (Continue, false) => "↻",
        (Compaction | ContextReset, false) => "⟲",
        (Draft, false) => "✎",
        (Wait, false) => "◷",
        (Retry, false) => "↺",
        (Steering, true) => ">",
        (Compaction | ContextReset, true) => "~",
        (_, true) => "*",
    };
    fixed.to_owned()
}

fn icon_tok(kind: EventKind) -> Tok {
    use EventKind::*;
    match kind {
        Recovery => Tok::Warning,
        Skill | Tools | Learning => Tok::SectionLearn,
        Steering | Continue | Plan | Draft => Tok::Accent,
        Verification => Tok::Muted,
        Hook | Model | Subagent => Tok::SectionConnect,
        _ => Tok::Muted,
    }
}

/// Rows for one controller event, laid out by its lane and detail.
pub(super) fn rows(event: &UiEvent, w: u16, ctx: &TranscriptCtx) -> Rows {
    let w = w as usize;
    if w == 0 || event.text.trim().is_empty() {
        return Vec::new();
    }
    match &event.detail {
        Some(EventDetail::ModelSwitch { from, to, reason }) => {
            return switch_line(event, from.as_deref(), to, *reason, w, ctx);
        }
        Some(EventDetail::Review { severity, note }) if w >= CARD_MIN => {
            return review_card(severity, note, w, ctx);
        }
        _ => {}
    }
    if event.kind == EventKind::Verification
        && let Some((name, status)) = check_problem(&event.text)
    {
        return check_line(&name, &status, w, ctx);
    }
    // A rewind cuts history back to a checkpoint, so it marks the transcript like a compaction.
    if event.kind == EventKind::Investigation && event.text.starts_with("Investigation rewound") {
        return divider(event, "rewound", w, ctx);
    }
    match event.kind.lane() {
        Lane::Divider => divider(event, event.kind.label(), w, ctx),
        Lane::Alert if w >= CARD_MIN => alert_card(event, w, ctx),
        Lane::Context | Lane::Alert => line(event, w, ctx),
    }
}

/// The glyph and label lead every one-line event: `⇄  model   `.
fn lead(kind: EventKind, glyph: &str, glyph_tok: Tok, theme: &Theme) -> Vec<Span<'static>> {
    let pad = GUTTER.saturating_sub(text::width(glyph)).max(1);
    vec![
        Span::styled(glyph.to_owned(), theme.fg(glyph_tok)),
        Span::raw(" ".repeat(pad)),
        Span::styled(format!("{:<LABEL_W$}", kind.label()), theme.fg(Tok::Muted)),
        Span::raw(" "),
    ]
}

/// The authoritative sentence under a one-line summary, shown only while tools are expanded.
fn expanded_text(event: &UiEvent, w: usize, ctx: &TranscriptCtx) -> Rows {
    if !ctx.tools_expanded {
        return Vec::new();
    }
    let hang = GUTTER + LABEL_W + 1;
    let room = w.saturating_sub(hang).max(8);
    text::sanitize(&event.text)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .flat_map(|line| text::wrap_words(line.trim(), room))
        .map(|part| {
            fitted(
                vec![
                    Span::raw(" ".repeat(hang)),
                    Span::styled(part, ctx.theme.fg(Tok::Dim)),
                ],
                w,
                ctx.theme,
            )
        })
        .collect()
}

fn switch_reason(reason: SwitchReason) -> &'static str {
    match reason {
        SwitchReason::Fallback => "fallback after an error",
        SwitchReason::Restore => "primary restored",
        SwitchReason::Promotion => "larger context window",
        SwitchReason::Prewalk => "prewalk handoff",
    }
}

/// `⇄  model   anthropic/opus → openai/gpt · fallback after an error`. The new model is the
/// bright part; a fallback reads amber because the configured model is not answering.
fn switch_line(
    event: &UiEvent,
    from: Option<&str>,
    to: &str,
    reason: SwitchReason,
    w: usize,
    ctx: &TranscriptCtx,
) -> Rows {
    let theme = ctx.theme;
    let arrow = if theme.ascii() { "->" } else { "→" };
    let dot = theme.sym(Sym::Dot);
    let glyph_tok = if reason == SwitchReason::Fallback {
        Tok::Warning
    } else {
        icon_tok(event.kind)
    };
    let mut spans = lead(event.kind, &icon(event.kind, theme), glyph_tok, theme);
    if let Some(from) = from.filter(|from| *from != to) {
        spans.push(Span::styled(text::one_line(from), theme.fg(Tok::Dim)));
        spans.push(Span::styled(format!(" {arrow} "), theme.fg(Tok::Muted)));
    }
    spans.push(Span::styled(
        text::one_line(to),
        theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!(" {dot} {}", switch_reason(reason)),
        theme.fg(if reason == SwitchReason::Fallback {
            Tok::Warning
        } else {
            Tok::Muted
        }),
    ));
    let mut rows = vec![fitted(spans, w, theme)];
    rows.extend(expanded_text(event, w, ctx));
    rows
}

/// An advisor's note as a quoted card: `╭─ ◇ Advisor · concern ─╮`, the note behind a quote
/// bar, and `unverified advice` on the bottom edge. A blocker is amber; other notes stay muted.
fn review_card(severity: &str, note: &str, w: usize, ctx: &TranscriptCtx) -> Rows {
    use super::tool_cards::frame::{CHROME_W, Frame};
    let theme = ctx.theme;
    let blocker = severity == "blocker";
    let tone = if blocker { Tok::Warning } else { Tok::Muted };
    let dot = format!(" {} ", theme.sym(Sym::Dot));
    let mut card = Frame::new(
        vec![
            Span::styled(icon(EventKind::Coach, theme), theme.fg(tone)),
            Span::raw(" "),
            Span::styled(
                "Advisor",
                theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD),
            ),
            Span::styled(dot.clone(), theme.fg(Tok::Dim)),
            Span::styled(text::one_line(severity), theme.fg(tone)),
        ],
        theme.fg(if blocker {
            Tok::Warning
        } else {
            Tok::BorderMuted
        }),
    );
    let bar = format!("{} ", theme.sym(Sym::QuoteBar));
    let room = w.saturating_sub(CHROME_W + text::width(&bar)).max(1);
    let body: Rows = text::sanitize(note)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .flat_map(|line| text::wrap_words(line.trim(), room))
        .map(|part| {
            Line::from(vec![
                Span::styled(bar.clone(), theme.fg(tone)),
                Span::styled(part, theme.fg(Tok::Text)),
            ])
        })
        .collect();
    card.sections.push((None, body));
    card.foot = vec![Span::styled(
        format!("unverified advice{dot}added to the next request"),
        theme.fg(Tok::Dim),
    )];
    card.rows(theme, w)
}

/// The loop correcting itself, as an amber card: `╭─ ⚠ Recovered ─╮` with the sentence inside.
/// Long reports keep their first rows and put the rest under the expand key.
fn alert_card(event: &UiEvent, w: usize, ctx: &TranscriptCtx) -> Rows {
    use super::tool_cards::frame::{CHROME_W, Frame};
    let theme = ctx.theme;
    let mut card = Frame::new(
        vec![
            Span::styled(icon(event.kind, theme), theme.fg(Tok::Warning)),
            Span::raw(" "),
            Span::styled(
                "Recovered",
                theme.fg(Tok::Warning).add_modifier(Modifier::BOLD),
            ),
        ],
        theme.fg(Tok::Warning),
    );
    let room = w.saturating_sub(CHROME_W).max(1);
    let mut body: Rows = text::sanitize(&event.text)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .flat_map(|line| text::wrap_words(line.trim(), room))
        .map(|part| Line::from(Span::styled(part, theme.fg(Tok::Text))))
        .collect();
    if !ctx.tools_expanded && body.len() > ALERT_ROWS {
        let hidden = body.len() - ALERT_ROWS;
        body.truncate(ALERT_ROWS);
        card.foot = vec![super::tool_cards::hidden_hint(
            hidden,
            ctx.expand_key,
            theme,
        )];
    }
    card.sections.push((None, body));
    card.rows(theme, w)
}

/// `Verification lint: CodeFailed` as `(lint, code failed)`. Only problems are reported as
/// events (a passing check is already its own tool row), so any parsed status is a problem.
fn check_problem(text: &str) -> Option<(String, String)> {
    let rest = text.trim().strip_prefix("Verification ")?;
    let (name, status) = rest.rsplit_once(": ")?;
    let status = status.trim();
    if name.trim().is_empty()
        || status.is_empty()
        || !status.chars().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    let mut words = String::new();
    for (i, c) in status.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            words.push(' ');
        }
        words.push(c.to_ascii_lowercase());
    }
    Some((name.trim().to_owned(), words))
}

/// `✗  check   lint · code failed`: red when the code failed, amber when the check could not
/// give an answer (timed out, unavailable, cancelled).
fn check_line(name: &str, status: &str, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let failed = status.contains("failed");
    let (glyph, tok) = if failed {
        (theme.sym(Sym::Cross2), Tok::Error)
    } else {
        (theme.sym(Sym::Warn), Tok::Warning)
    };
    let mut spans = lead(EventKind::Verification, glyph, tok, theme);
    spans.push(Span::styled(
        text::one_line(name),
        theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!(" {} {status}", theme.sym(Sym::Dot)),
        theme.fg(tok),
    ));
    vec![fitted(spans, w, theme)]
}

/// `↳ steer   Steering delivered to the running response.` A multi-line body (an approved
/// plan) shows its first line; the rest follows under the expand toggle.
fn line(event: &UiEvent, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let clean = text::sanitize(&event.text);
    let mut lines = clean.trim().lines().filter(|line| !line.trim().is_empty());
    let first = lines.next().unwrap_or_default().trim().to_owned();
    let rest: Vec<&str> = lines.collect();
    let glyph = icon(event.kind, theme);
    let glyph = glyph.as_str();
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

/// `──── ⟲ compacted · 172k → 31k · Ctrl+O ────`, with the full report under the expand toggle.
fn divider(event: &UiEvent, word: &str, w: usize, ctx: &TranscriptCtx) -> Rows {
    let theme = ctx.theme;
    let rule = theme.sym(Sym::Hr);
    // No theme symbol names an arrow; it degrades with the glyph set.
    let arrow = if theme.ascii() { "->" } else { "→" };
    let dot = theme.sym(Sym::Dot);
    let mut label = format!("{} {word}", icon(event.kind, theme));
    if let Some((before, after)) = event.tokens {
        label.push_str(&format!(
            " {dot} {} {arrow} {}",
            text::compact_count(before),
            text::compact_count(after)
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

#[cfg(test)]
mod tests;
