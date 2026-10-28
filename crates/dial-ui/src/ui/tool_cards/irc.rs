//! The IRC lane: messages one agent sends another (`message`, `peers` with `action: send`).
//!
//! ```text
//! ✉  IRC     ➤ all                         2 queued · 1 refused
//!    ▏ rebase onto main before you push
//!    ├─ reviewer ⟦queued⟧
//!    ├─ scout#1 ⟦queued⟧
//!    └─ worker#3 ⟦refused⟧ — not running
//! ```
//!
//! The envelope marks a delivered note so conversation between agents reads apart from work. The
//! quoted text is what was said; the receipt tree only appears when it says more than the header.
use super::{
    GUTTER, Rows, Tool, ToolState, VERB_W, fit_facts, frame::chip, gutter_span, phase_glyph,
};
use crate::ui::{
    markdown::truncate_spans,
    text,
    theme::{Sym, Tok},
    transcript::{TranscriptCtx, ellipsis, fitted},
};
use ratatui::{style::Modifier, text::Span};
use serde_json::Value;

/// Quoted lines a collapsed card keeps before the expand hint.
const QUOTE_ROWS: usize = 3;

/// One recipient's delivery receipt.
struct Receipt {
    to: String,
    queued: bool,
    reason: String,
}

/// The recipient and text of a call that sends a message, or `None` for any other call (listing
/// recipients, claiming scopes) so it keeps the ordinary card.
fn outgoing(t: &Tool) -> Option<(String, String)> {
    let args: Value = serde_json::from_str(&t.input).ok()?;
    let sends = match t.name.as_str() {
        "message" => true,
        "peers" => args["action"].as_str() == Some("send"),
        _ => false,
    };
    let body = args["text"].as_str().filter(|s| !s.trim().is_empty())?;
    sends.then(|| {
        let to = args["to"].as_str().unwrap_or_default();
        (text::one_line(to), body.to_owned())
    })
}

/// Receipts from a `message` result (`{"receipts": [...]}`) or a `peers send` result (one receipt).
fn receipts(output: &str) -> Vec<Receipt> {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    let one = |r: &Value| {
        let queued = match r["status"].as_str()? {
            "queued" => true,
            "refused" => false,
            _ => return None,
        };
        Some(Receipt {
            to: text::one_line(r["to"].as_str()?),
            queued,
            reason: text::one_line(r["reason"].as_str().unwrap_or_default()),
        })
    };
    match value["receipts"].as_array() {
        Some(rows) => rows.iter().filter_map(one).collect(),
        None => one(&value).into_iter().collect(),
    }
}

/// The IRC card for a sent message, or `None` when `t` is not one. A failed call keeps the
/// ordinary card, whose body is the error.
pub(super) fn irc_card(t: &Tool, w: usize, ctx: &TranscriptCtx) -> Option<Rows> {
    if t.state == ToolState::Failed {
        return None;
    }
    let (to, body) = outgoing(t)?;
    let theme = ctx.theme;
    let ascii = theme.ascii();
    let mark = ellipsis(theme);
    let receipts = receipts(&t.output);
    // Unknown result formats keep their evidence instead of looking like a delivered note.
    if t.state == ToolState::Completed && receipts.is_empty() && !t.output.trim().is_empty() {
        return None;
    }
    let queued = receipts.iter().filter(|r| r.queued).count();
    let refused = receipts.len() - queued;
    let undelivered = !receipts.is_empty() && queued == 0;

    let (glyph, glyph_style) = match t.state {
        ToolState::Completed if undelivered => {
            (theme.sym(Sym::Cross2).to_owned(), theme.fg(Tok::Error))
        }
        ToolState::Completed => (
            if ascii { "@" } else { "✉" }.to_owned(),
            theme.fg(Tok::SectionConnect),
        ),
        state => phase_glyph(state, ctx),
    };
    let sep = format!(" {} ", theme.sym(Sym::Dot));
    let mut facts = Vec::new();
    match (receipts.len(), queued, refused) {
        (0, _, _) => {}
        (1, 1, _) => facts.push("queued".to_owned()),
        (1, _, _) => facts.push("refused".to_owned()),
        _ => {
            if queued > 0 {
                facts.push(format!("{queued} queued"));
            }
            if refused > 0 {
                facts.push(format!("{refused} refused"));
            }
        }
    }
    if !matches!(t.state, ToolState::Running | ToolState::Completed) {
        facts.push(t.state.label().to_owned());
    }
    let facts = facts
        .into_iter()
        .map(|text| super::Fact { text, drop: 0 })
        .collect();
    let arrow = if ascii { "->" } else { "➤" };
    let target = if to.is_empty() {
        arrow.to_owned()
    } else {
        format!("{arrow} {to}")
    };
    let lead = gutter_span(glyph, glyph_style);
    let lead_w: usize = lead.iter().map(|s| text::width(&s.content)).sum();
    let (facts_text, room) = fit_facts(facts, w, lead_w + VERB_W + 1, text::width(&target), &sep);
    let mut head = lead;
    head.push(Span::styled(
        format!("{:<VERB_W$}", "IRC"),
        theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD),
    ));
    head.push(Span::raw(" "));
    head.push(Span::styled(
        text::ellipsize(&target, room.max(1), ascii),
        theme.fg(Tok::Muted),
    ));
    if !facts_text.is_empty() {
        let used: usize = head.iter().map(|s| text::width(&s.content)).sum();
        let right_w = text::width(&facts_text);
        head.push(Span::raw(
            " ".repeat(w.saturating_sub(used + right_w).max(1)),
        ));
        let tok = if undelivered { Tok::Error } else { Tok::Dim };
        head.push(Span::styled(facts_text, theme.fg(tok)));
    }
    let mut rows = vec![fitted(head, w, theme)];

    // What was said, quoted under the header.
    let indent = " ".repeat(GUTTER);
    let bar = if ascii { "| " } else { "▏ " };
    let room = w.saturating_sub(GUTTER + text::width(bar)).max(1);
    let lines: Vec<String> = text::sanitize(&body)
        .lines()
        .flat_map(|line| text::wrap_words(line, room))
        .collect();
    let keep = if ctx.tools_expanded {
        lines.len()
    } else {
        lines.len().min(QUOTE_ROWS)
    };
    for line in &lines[..keep] {
        rows.push(fitted(
            vec![
                Span::styled(format!("{indent}{bar}"), theme.fg(Tok::MdQuoteBorder)),
                Span::styled(line.clone(), theme.fg(Tok::Text)),
            ],
            w,
            theme,
        ));
    }
    if keep < lines.len() {
        rows.push(fitted(
            vec![
                Span::styled(format!("{indent}{bar}"), theme.fg(Tok::MdQuoteBorder)),
                super::hidden_hint(lines.len() - keep, ctx.expand_key, theme),
            ],
            w,
            theme,
        ));
    }

    // A lone queued receipt only repeats the header; a broadcast or a refusal earns the tree.
    if receipts.len() > 1 || refused > 0 {
        let (branch, last) = if ascii {
            ("|-", "`-")
        } else {
            ("├─", "└─")
        };
        let rail = theme.fg(Tok::Border);
        let dash = if ascii { "-" } else { "—" };
        for (i, r) in receipts.iter().enumerate() {
            let fork = if i + 1 == receipts.len() {
                last
            } else {
                branch
            };
            let mut spans = vec![
                Span::styled(format!("{indent}{fork} "), rail),
                Span::styled(r.to.clone(), theme.fg(Tok::Text)),
                Span::raw(" "),
            ];
            if r.queued {
                spans.push(chip(theme, "queued", Tok::Success));
            } else {
                spans.push(chip(theme, "refused", Tok::Error));
                if !r.reason.is_empty() {
                    spans.push(Span::styled(
                        format!(" {dash} {}", r.reason),
                        theme.fg(Tok::Muted),
                    ));
                }
            }
            rows.push(fitted(truncate_spans(spans, w, &mark), w, theme));
        }
    }
    Some(rows)
}

/// Render controller notices for received messages in the same lane as sent notes.
/// Only worker notices carry message text; project-peer notices identify arrival, not the body.
pub(in crate::ui) fn incoming_notice(body: &str, w: u16, ctx: &TranscriptCtx) -> Option<Rows> {
    let (from, quote) = if let Some(rest) = body.strip_prefix("Message from ") {
        let (from, quote) = rest.split_once(" to parent: ")?;
        (from, Some(quote))
    } else {
        let from = body
            .strip_prefix("Project peer ")?
            .strip_suffix(" sent a coordination message.")?;
        (from, None)
    };
    if w == 0 {
        return Some(Vec::new());
    }
    let theme = ctx.theme;
    let (envelope, arrow, bar, dot) = if theme.ascii() {
        ("@", "<-", "| ", ":")
    } else {
        ("✉", "⟵", "▏ ", "·")
    };
    let mut head = gutter_span(envelope.into(), theme.fg(Tok::SectionConnect));
    head.push(Span::styled(
        format!("{:<VERB_W$} ", "IRC"),
        theme.fg(Tok::ToolTitle).add_modifier(Modifier::BOLD),
    ));
    head.push(Span::styled(
        format!("{arrow} {}", text::one_line(from)),
        theme.fg(Tok::Muted),
    ));
    if quote.is_none() {
        head.push(Span::styled(
            format!(" {dot} coordination message received"),
            theme.fg(Tok::Dim),
        ));
    }
    let w = w as usize;
    let mut rows = vec![fitted(head, w, theme)];
    if let Some(quote) = quote {
        for line in text::wrap_words(&text::sanitize(quote), w.saturating_sub(GUTTER + 2).max(1)) {
            rows.push(fitted(
                vec![
                    Span::styled(
                        format!("{}{bar}", " ".repeat(GUTTER)),
                        theme.fg(Tok::MdQuoteBorder),
                    ),
                    Span::styled(line, theme.fg(Tok::Text)),
                ],
                w,
                theme,
            ));
        }
    }
    Some(rows)
}

#[cfg(test)]
mod tests;
