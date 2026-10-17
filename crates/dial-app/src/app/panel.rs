//! The recovery panel: layout and scrolling for the recovery prompt.

use crate::ui::markdown::truncate_spans;
use crate::ui::theme::{Sym, Tok};
use crate::ui::{Rows, text};
use ratatui::text::{Line, Span};

// ---------------------------------------------------------------------------
// Panels, replay
// ---------------------------------------------------------------------------

/// Title of the agent's stop panel. The summary/raw toggle applies only to this panel.
pub(super) const RECOVERY_PANEL: &str = "Recovery needs direction";

pub(super) fn panel_rows_window(
    theme: &crate::ui::theme::Theme,
    width: u16,
    title: &str,
    body: &str,
    options: &[(&str, char)],
    selected: usize,
    offset: usize,
    height: usize,
) -> Rows {
    let width = width as usize;
    if width < 6 {
        return vec![Line::from(text::clip(title, width))];
    }
    let inner = panel_inner(width as u16);
    let border = theme.fg(Tok::Warning);
    let pad_row = |spans: Vec<Span<'static>>| -> Line<'static> {
        let spans = truncate_spans(spans, inner, "");
        let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
        let mut row = vec![Span::styled(format!("{} ", theme.sym(Sym::BoxV)), border)];
        row.extend(spans);
        row.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
        row.push(Span::styled(format!(" {}", theme.sym(Sym::BoxV)), border));
        Line::from(row)
    };
    let mut rows = Vec::new();
    // Same title shape as every panel: a mark, then the title in bold body text. Tiny panels
    // have no room for the mark.
    let mark = if width < 14 {
        ""
    } else if theme.ascii() {
        "*"
    } else {
        "\u{25c6}"
    };
    let title_text = text::ellipsize(
        &text::one_line(title),
        inner.saturating_sub(4),
        theme.ascii(),
    );
    let title_text = if mark.is_empty() {
        title_text
    } else {
        format!(" {title_text}")
    };
    let dashes = width.saturating_sub(text::width(&title_text) + text::width(mark) + 5);
    rows.push(Line::from(vec![
        Span::styled(
            format!("{}{} ", theme.sym(Sym::BoxTl), theme.sym(Sym::BoxH)),
            border,
        ),
        Span::styled(mark, border.add_modifier(ratatui::style::Modifier::BOLD)),
        Span::styled(
            format!("{title_text} "),
            theme
                .fg(Tok::Text)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{}{}",
                theme.sym(Sym::BoxH).repeat(dashes),
                theme.sym(Sym::BoxTr)
            ),
            border,
        ),
    ]));
    let lines = text::wrap_words(&text::sanitize(body), inner);
    let body_limit = panel_body_limit(options.len(), height);
    let offset = offset.min(lines.len().saturating_sub(body_limit));
    for line in lines.iter().skip(offset).take(body_limit) {
        rows.push(pad_row(vec![Span::styled(
            text::clip(line, inner),
            theme.fg(Tok::ToolOutput),
        )]));
    }
    let option_limit = options.len().min(height.saturating_sub(5).max(1));
    let start = selected
        .saturating_sub(option_limit.saturating_sub(1))
        .min(options.len().saturating_sub(option_limit));
    for (index, (label, hotkey)) in options.iter().enumerate().skip(start).take(option_limit) {
        let on = index == selected;
        let marker = if on { theme.sym(Sym::Pointer) } else { " " };
        let style = if on {
            theme
                .on(Tok::Text, Tok::SelectedBg)
                .patch(theme.fg(Tok::Accent))
        } else {
            theme.fg(Tok::Text)
        };
        let label = text::ellipsize(label, inner.saturating_sub(8), theme.ascii());
        rows.push(pad_row(vec![
            Span::styled(format!("{marker} {label}"), style),
            Span::styled(format!("  ({hotkey})"), theme.fg(Tok::Muted)),
        ]));
    }
    let hint = if lines.len() > body_limit {
        format!(
            "Choose · Enter confirm · Esc cancel · Page keys scroll details {}-{} of {}",
            offset + 1,
            (offset + body_limit).min(lines.len()),
            lines.len()
        )
    } else {
        "Choose · Enter confirm · Esc cancel · Page keys scroll details".to_owned()
    };
    rows.push(pad_row(vec![Span::styled(hint, theme.fg(Tok::Dim))]));
    rows.push(Line::from(Span::styled(
        format!(
            "{}{}{}",
            theme.sym(Sym::BoxBl),
            theme.sym(Sym::BoxH).repeat(width - 2),
            theme.sym(Sym::BoxBr)
        ),
        border,
    )));
    rows
}

/// Text columns inside a modal panel's border and padding.
pub(super) fn panel_inner(width: u16) -> usize {
    (width as usize).saturating_sub(4).max(1)
}

/// Body rows a modal panel shows for `options` choices within `height` rows: the same budget the renderer and
/// the page keys use, so paging never jumps past rows the user has not seen.
pub(super) fn panel_body_limit(options: usize, height: usize) -> usize {
    let option_limit = options.min(height.saturating_sub(5).max(1));
    height.saturating_sub(option_limit + 3).clamp(1, 8)
}

/// The next body offset for a page key: one visible page minus a row of overlap, never past the last page.
pub(super) fn panel_scroll(offset: usize, lines: usize, visible: usize, down: bool) -> usize {
    let page = visible.saturating_sub(1).max(1);
    if down {
        offset
            .saturating_add(page)
            .min(lines.saturating_sub(visible))
    } else {
        offset.saturating_sub(page)
    }
}
