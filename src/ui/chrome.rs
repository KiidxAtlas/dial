//! Shared chrome: labels, panel titles, key hints, status marks and thin rounded panel frames.
//!
//! Everything here is text on the terminal's own background. There are no chips, no page fills and
//! no shadows. The only filled cells in the UI are the selected row (`Tok::SelectedBg`) and the
//! text cursor. Section colors appear as foreground text or as a 1-cell `▎` gutter, never as fills.
use ratatui::{
    style::{Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders},
};

use super::{
    text,
    theme::{Theme, Tok},
};

/// Section identity. Each section has one foreground token: the color of its label, its panel-title
/// mark, and its gutter mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Dial,
    Tools,
    Sandbox,
    Learn,
    Connect,
    Loop,
    Primary,
    Warn,
}

impl Section {
    /// Foreground token for the section's number and gutter mark.
    pub fn tok(self) -> Tok {
        match self {
            Section::Dial | Section::Loop => Tok::Accent,
            Section::Primary => Tok::Error,
            Section::Tools => Tok::SectionTools,
            Section::Sandbox => Tok::SectionSandbox,
            Section::Learn => Tok::SectionLearn,
            Section::Connect => Tok::SectionConnect,
            Section::Warn => Tok::Warning,
        }
    }

    /// Foreground token for a label. Plain sections keep the secondary color so labels stay quiet;
    /// only the warning, error and ok sections color their label.
    fn label_tok(self) -> Tok {
        match self {
            Section::Dial | Section::Loop => Tok::Muted,
            Section::Primary => Tok::Error,
            Section::Warn => Tok::Warning,
            other => other.tok(),
        }
    }
}

/// Readiness of a setup item, shown as a colored status mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ready,
    Missing,
    Warn,
    Error,
}

impl Status {
    fn tok(self) -> Tok {
        match self {
            Status::Ready => Tok::Success,
            Status::Missing => Tok::Muted,
            Status::Warn => Tok::Warning,
            Status::Error => Tok::Error,
        }
    }

    fn mark(self, ascii: bool) -> &'static str {
        match (self, ascii) {
            (Status::Ready, false) => "●",
            (Status::Missing, false) => "○",
            (Status::Warn, false) => "▲",
            (Status::Error, false) => "✗",
            (Status::Ready, true) => "*",
            (Status::Missing, true) => "o",
            (Status::Warn, true) => "!",
            (Status::Error, true) => "x",
        }
    }
}

/// Uppercase label in the section's label color, e.g. `RECENT`. No fill, no padding.
pub fn tag(theme: &Theme, section: Section, text: &str) -> Span<'static> {
    Span::styled(text.to_uppercase(), theme.fg(section.label_tok()))
}

/// Uppercase secondary label, e.g. `TOOLS`. Same as [`tag`] for a plain section.
pub fn label(theme: &Theme, text: &str) -> Span<'static> {
    Span::styled(text.to_uppercase(), theme.fg(Tok::Muted))
}

/// Status mark and word as colored text: `● ready`, `○ missing`, `▲ warn`, `✗ error`.
pub fn status_chip(theme: &Theme, status: Status, text: &str) -> Span<'static> {
    Span::styled(
        format!("{} {text}", status.mark(theme.ascii())),
        theme.fg(status.tok()),
    )
}

/// Key hints as spans: each key in amber bold with its label in the secondary color, joined by ` · `.
/// Example: `ctrl+p` `actions` · `/` `commands`.
///
/// Every panel uses the same words, so a key always means the same thing: Esc is `close` at a
/// panel's top level, `back` inside a nested view, `cancel` for an edit or a question, and
/// `clear search` while a search or filter is typed; `/` is `search`; ←/→ (or Tab where the
/// panel has no fields) are `tabs`; Enter names the row's main action. The last hint is always
/// the way out, and its label follows what Esc will do next.
pub fn key_hint_spans(theme: &Theme, hints: &[(&str, &str)]) -> Vec<Span<'static>> {
    let sep = if theme.ascii() { " | " } else { " · " };
    let mut spans = Vec::with_capacity(hints.len() * 3);
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(sep, theme.fg(Tok::Muted)));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}"), theme.fg(Tok::Muted)));
    }
    spans
}

/// Style of one tab in any panel's tab strip: the active tab is accent, bold and underlined, the
/// others muted words. No fills, so a tab strip reads the same in every panel and theme.
pub fn tab_style(theme: &Theme, active: bool) -> Style {
    if active {
        theme
            .fg(Tok::Accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        theme.fg(Tok::Muted)
    }
}

/// A panel title: a mark in the section's color, then the title in bold body text, e.g.
/// `◆ Settings`. ASCII glyph sets use `*`.
pub fn fig_label(theme: &Theme, section: Section, title: &str) -> Vec<Span<'static>> {
    let mark = if theme.ascii() { "*" } else { "\u{25c6}" };
    vec![
        Span::styled(mark, theme.fg(section.tok()).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" {title}"),
            theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
        ),
    ]
}

/// Thin rounded frame in the muted rule color (`╭─╮│╰─╯`). The frame never paints a background.
pub fn panel_block(theme: &Theme) -> Block<'static> {
    const ASCII: border::Set = border::Set {
        top_left: "+",
        top_right: "+",
        bottom_left: "+",
        bottom_right: "+",
        vertical_left: "|",
        vertical_right: "|",
        horizontal_top: "-",
        horizontal_bottom: "-",
    };
    let set = if theme.ascii() {
        ASCII
    } else {
        border::ROUNDED
    };
    Block::default()
        .borders(Borders::ALL)
        .border_set(set)
        .border_style(theme.fg(Tok::BorderMuted))
}

/// [`panel_block`] with the title in the top border: `╭ ◆ Settings ──────── extra ╮`. `extra`
/// sits at the right edge when non-empty.
pub fn panel_block_titled(
    theme: &Theme,
    section: Section,
    title: &str,
    extra: &str,
) -> Block<'static> {
    let mut left = vec![Span::raw(" ")];
    left.extend(fig_label(theme, section, title));
    left.push(Span::raw(" "));
    let mut block = panel_block(theme).title_top(Line::from(left));
    if !extra.is_empty() {
        block = block.title_top(
            Line::from(Span::styled(format!(" {extra} "), theme.fg(Tok::Muted))).right_aligned(),
        );
    }
    block
}

/// Rows drawn inline (not through a ratatui block) wrapped in the standard rounded frame:
/// `╭─ ◆ Title ───── extra ─╮`, each row as `│ row │` padded to `width`, then `╰───╯`. Rows wider
/// than the inner width are clipped. Every returned line is exactly `width` cells (when ≥ 6).
pub fn framed_lines(
    theme: &Theme,
    section: Section,
    title: &str,
    extra: &str,
    rows: Vec<Line<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    use super::theme::Sym;
    if width < 6 {
        return rows;
    }
    let border = theme.fg(Tok::BorderMuted);
    let h = theme.sym(Sym::BoxH);
    let v = theme.sym(Sym::BoxV).to_owned();
    let inner = width - 4;
    let ellipsis = theme.sym(Sym::Ellipsis).to_owned();
    let mut head = vec![Span::styled(
        format!("{}{h} ", theme.sym(Sym::BoxTl)),
        border,
    )];
    let mut label = fig_label(theme, section, &text::one_line(title));
    label.push(Span::raw(" "));
    head.extend(super::markdown::truncate_spans(label, inner, &ellipsis));
    let used: usize = head.iter().map(|s| text::width(&s.content)).sum();
    let extra = text::one_line(extra);
    let tail = if extra.is_empty() || used + text::width(&extra) + 4 > width {
        String::new()
    } else {
        format!(" {extra} ")
    };
    let fill = width.saturating_sub(used + text::width(&tail) + 1);
    head.push(Span::styled(h.repeat(fill), border));
    if !tail.is_empty() {
        head.push(Span::styled(tail, theme.fg(Tok::Muted)));
    }
    head.push(Span::styled(theme.sym(Sym::BoxTr).to_owned(), border));
    let mut out = vec![Line::from(head)];
    for row in rows {
        let style = row.style;
        let mut spans = vec![Span::styled(format!("{v} "), border)];
        let body = super::markdown::truncate_spans(row.spans, inner, &ellipsis);
        let used: usize = body.iter().map(|s| text::width(&s.content)).sum();
        spans.extend(body);
        spans.push(Span::styled(" ".repeat(inner.saturating_sub(used)), style));
        spans.push(Span::styled(format!(" {v}"), border));
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        format!(
            "{}{}{}",
            theme.sym(Sym::BoxBl),
            h.repeat(width - 2),
            theme.sym(Sym::BoxBr)
        ),
        border,
    )));
    out
}

/// Style for a row that is the selected one in a list: the selection fill with bold body text.
pub fn selected_style(theme: &Theme) -> Style {
    theme
        .fg(Tok::Text)
        .patch(theme.bg(Tok::SelectedBg))
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests;
