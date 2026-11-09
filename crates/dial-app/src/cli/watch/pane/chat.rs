//! The Chats tab's conversation view: text-message style bubbles. Incoming messages sit on the
//! left, this side's on the right, lifecycle events in the middle; consecutive messages from one
//! sender group under a single name and age.
use crate::ui::text;
use crate::ui::theme::{Theme, Tok};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Messages from one sender this close together share a header.
const GROUP_MS: u64 = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    Left,
    Right,
    Center,
}

pub(super) struct Bubble {
    pub side: Side,
    pub sender: String,
    pub text: String,
    pub at_ms: u64,
    /// A small status under the bubble, such as a read receipt.
    pub receipt: Option<(String, Tok)>,
}

/// How long ago `at_ms` was, compactly; empty when unknown.
pub(super) fn age(now_ms: u64, at_ms: u64) -> String {
    if at_ms == 0 {
        return String::new();
    }
    let secs = now_ms.saturating_sub(at_ms) / 1000;
    match secs {
        0..45 => "now".into(),
        45..3_600 => format!("{}m", (secs / 60).max(1)),
        3_600..86_400 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn grouped(previous: Option<&Bubble>, bubble: &Bubble) -> bool {
    previous.is_some_and(|previous| {
        previous.side == bubble.side
            && previous.side != Side::Center
            && previous.sender == bubble.sender
            && previous.receipt.is_none()
            && bubble.at_ms.saturating_sub(previous.at_ms) <= GROUP_MS
    })
}

/// `content` placed at the left or right edge of a row `width` cells wide.
fn aligned(spans: Vec<Span<'static>>, right: bool, width: usize) -> Line<'static> {
    let used: usize = spans.iter().map(|span| text::width(&span.content)).sum();
    let mut row = Vec::new();
    if right {
        row.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    }
    row.extend(spans);
    Line::from(row)
}

/// The conversation as rows `width` cells wide, oldest first.
pub(super) fn render(
    bubbles: &[Bubble],
    width: usize,
    now_ms: u64,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let max = (width * 3 / 4).clamp(8.min(width), width);
    let mut lines = Vec::new();
    for (index, bubble) in bubbles.iter().enumerate() {
        let when = age(now_ms, bubble.at_ms);
        if bubble.side == Side::Center {
            let label = if when.is_empty() {
                format!(" {} ", text::one_line(&bubble.text))
            } else {
                format!(" {} · {when} ", text::one_line(&bubble.text))
            };
            let label = text::clip(&label, width);
            let rule = width.saturating_sub(text::width(&label));
            lines.push(Line::styled(
                format!(
                    "{}{label}{}",
                    "─".repeat(rule / 2),
                    "─".repeat(rule - rule / 2)
                ),
                theme.fg(Tok::Dim),
            ));
            lines.push(Line::default());
            continue;
        }
        let right = bubble.side == Side::Right;
        let previous = index.checked_sub(1).map(|i| &bubbles[i]);
        if !grouped(previous, bubble) {
            let mut header = vec![Span::styled(
                text::clip(&text::one_line(&bubble.sender), max),
                theme
                    .fg(if right { Tok::Accent } else { Tok::Success })
                    .add_modifier(Modifier::BOLD),
            )];
            if !when.is_empty() {
                header.push(Span::styled(format!(" · {when}"), theme.fg(Tok::Dim)));
            }
            lines.push(aligned(header, right, width));
        }
        let rows = text::wrap_words(&text::sanitize(&bubble.text), max.saturating_sub(2).max(1));
        let inner = rows.iter().map(|row| text::width(row)).max().unwrap_or(0);
        let style = if right {
            theme.bg(Tok::UserBg).patch(theme.fg(Tok::UserText))
        } else {
            theme.bg(Tok::PanelBg).patch(theme.fg(Tok::Text))
        };
        for row in rows {
            let cell = format!(" {} ", text::pad(&row, inner));
            lines.push(aligned(vec![Span::styled(cell, style)], right, width));
        }
        if let Some((receipt, tok)) = &bubble.receipt {
            lines.push(aligned(
                vec![Span::styled(receipt.clone(), theme.fg(*tok))],
                right,
                width,
            ));
        }
        let next = bubbles.get(index + 1);
        if !next.is_some_and(|next| grouped(Some(bubble), next)) {
            lines.push(Line::default());
        }
    }
    lines
}
