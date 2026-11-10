//! Shared chrome: labels, panel titles, key hints, status marks and the inset panel surface.
//!
//! There are no frames. A panel is an inset surface (`Tok::PanelBg`) with a bold title row and a
//! muted, italic hint at its right edge; prose and tool rows sit on the terminal's own background
//! and hang off a one-glyph gutter. The filled cells in the UI are the panel surface, the shaded
//! bands behind the user's messages and the composer (`Tok::UserBg`), the selected row
//! (`Tok::SelectedBg`) and the text cursor. Section colors appear as foreground text only.
use ratatui::{
    layout::Alignment,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Padding},
};

use super::{
    text,
    theme::{Sym, Theme, Tok},
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

/// Key hints as spans: each key in bold body text with its label in the secondary color, joined by
/// ` · `. Example: `ctrl+p` `actions` · `/` `commands`.
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
            spans.push(Span::styled(sep, theme.fg(Tok::Dim)));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}"), theme.fg(Tok::Muted)));
    }
    spans
}

/// Footer key hints fitted to `width` cells: `(key, label)` pairs as [`key_hint_spans`] draws them.
/// The last pair is the way out and always stays; when the row is too narrow the hints before it
/// are dropped whole, nearest the end first, so a label is never cut mid-word. Pairs with neither
/// key nor label are skipped. A last hint that cannot fit even alone is clipped with an ellipsis.
pub fn hint_line(theme: &Theme, hints: &[(&str, &str)], width: usize) -> Line<'static> {
    hint_line_after(theme, "", hints, width)
}

/// The separator between footer segments: ` · `, or ` | ` under the ASCII glyph preset.
pub fn separator(theme: &Theme) -> &'static str {
    if theme.ascii() { " | " } else { " · " }
}

/// [`hint_line`] after a muted `lead` such as a list position (`3/12`). The lead stays in front
/// of the hints but is clipped or omitted before the final (exit) hint can be hidden.
pub fn hint_line_after(
    theme: &Theme,
    lead: &str,
    hints: &[(&str, &str)],
    width: usize,
) -> Line<'static> {
    let sep = separator(theme);
    let lead = text::one_line(lead);
    let mut keep: Vec<(&str, &str)> = hints
        .iter()
        .copied()
        .filter(|(key, label)| !key.is_empty() || !label.is_empty())
        .collect();
    let exit_width = keep.last().map_or(0, |hint| {
        key_hint_spans(theme, &[*hint])
            .iter()
            .map(|s| text::width(&s.content))
            .sum::<usize>()
    });
    let lead_room = if keep.is_empty() {
        width
    } else {
        width.saturating_sub(exit_width + text::width(sep))
    };
    let lead: String =
        super::markdown::truncate_spans(vec![Span::raw(lead)], lead_room, theme.sym(Sym::Ellipsis))
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
    let build = |keep: &[(&str, &str)]| {
        let mut spans = Vec::new();
        if !lead.is_empty() {
            spans.push(Span::styled(lead.clone(), theme.fg(Tok::Muted)));
            if !keep.is_empty() {
                spans.push(Span::styled(sep, theme.fg(Tok::Dim)));
            }
        }
        spans.extend(key_hint_spans(theme, keep));
        spans
    };
    let cells =
        |spans: &[Span<'static>]| spans.iter().map(|s| text::width(&s.content)).sum::<usize>();
    let mut spans = build(&keep);
    while keep.len() > 1 && cells(&spans) > width {
        keep.remove(keep.len() - 2);
        spans = build(&keep);
    }
    Line::from(super::markdown::truncate_spans(
        spans,
        width,
        theme.sym(Sym::Ellipsis),
    ))
}

/// The colored dot and friendly name of an approval mode, `● Ask first`, in the one color mapping
/// every surface uses (see [`approval_tok`]). The name comes from
/// [`super::status::approval_label`]; an unknown mode shows its raw value.
pub fn approval_spans(theme: &Theme, mode: &str) -> Vec<Span<'static>> {
    let mode = text::one_line(mode.trim());
    vec![
        Span::styled(
            format!("{} ", theme.sym(Sym::Bullet)),
            theme.fg(approval_tok(&mode)),
        ),
        Span::styled(
            super::status::approval_label(&mode).to_owned(),
            theme.fg(Tok::Text),
        ),
    ]
}

/// Safety color of an approval mode: modes that ask before changing anything (`ask`, `readonly`)
/// are success, `write` is the accent, `auto` (no questions asked) is a warning, unknown is muted.
pub fn approval_tok(mode: &str) -> Tok {
    match mode.trim() {
        "ask" | "readonly" => Tok::Success,
        "write" => Tok::Accent,
        "auto" => Tok::Warning,
        _ => Tok::Muted,
    }
}

/// The lifecycle state of a run, tool, task, agent or question, with one glyph, color and word on
/// every surface. Stopped-by-someone states (interrupted, cancelled, denied) are muted, never error
/// red: only `Failed` is an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StateMark {
    Running,
    Waiting,
    Done,
    Answered,
    Failed,
    Interrupted,
    Cancelled,
    Denied,
    Blocked,
}

impl StateMark {
    pub const ALL: &'static [StateMark] = &[
        StateMark::Running,
        StateMark::Waiting,
        StateMark::Done,
        StateMark::Answered,
        StateMark::Failed,
        StateMark::Interrupted,
        StateMark::Cancelled,
        StateMark::Denied,
        StateMark::Blocked,
    ];

    /// The state's symbol token; its glyph follows the theme's preset and overrides.
    pub fn sym(self) -> Sym {
        match self {
            StateMark::Running => Sym::Working,
            StateMark::Waiting => Sym::Pending,
            StateMark::Done | StateMark::Answered => Sym::Check,
            StateMark::Failed => Sym::Cross2,
            StateMark::Interrupted | StateMark::Cancelled | StateMark::Denied => Sym::Interrupted,
            StateMark::Blocked => Sym::Blocked,
        }
    }

    pub fn glyph(self, theme: &Theme) -> &str {
        theme.sym(self.sym())
    }

    pub fn tok(self) -> Tok {
        match self {
            StateMark::Running => Tok::Accent,
            StateMark::Waiting
            | StateMark::Interrupted
            | StateMark::Cancelled
            | StateMark::Denied => Tok::Muted,
            StateMark::Done | StateMark::Answered => Tok::Success,
            StateMark::Failed => Tok::Error,
            StateMark::Blocked => Tok::Warning,
        }
    }

    /// The one lowercase word for the state.
    pub fn word(self) -> &'static str {
        match self {
            StateMark::Running => "running",
            StateMark::Waiting => "waiting",
            StateMark::Done => "done",
            StateMark::Answered => "answered",
            StateMark::Failed => "failed",
            StateMark::Interrupted => "interrupted",
            StateMark::Cancelled => "stopped",
            StateMark::Denied => "denied",
            StateMark::Blocked => "blocked",
        }
    }

    /// The glyph alone in the state's color.
    pub fn mark_span(self, theme: &Theme) -> Span<'static> {
        Span::styled(self.glyph(theme).to_owned(), theme.fg(self.tok()))
    }

    /// `✓ done`: glyph and word in the state's color.
    pub fn chip_span(self, theme: &Theme) -> Span<'static> {
        Span::styled(
            format!("{} {}", self.glyph(theme), self.word()),
            theme.fg(self.tok()),
        )
    }
}

/// Style of one tab in any panel's tab strip: the active tab is a soft pill (the panel inset fill)
/// with an accent, bold and underlined label; the others are muted words. Themes without an inset
/// fill keep the plain accent label, so a tab strip reads the same in every panel.
pub fn tab_style(theme: &Theme, active: bool) -> Style {
    if active {
        surface(theme).patch(
            theme
                .fg(Tok::Accent)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
    } else {
        theme.fg(Tok::Muted)
    }
}

/// A panel title: the title in bold, in the section's color, e.g. `Settings`. No mark, no frame:
/// the surface it sits on already sets the panel apart.
pub fn fig_label(theme: &Theme, section: Section, title: &str) -> Vec<Span<'static>> {
    vec![Span::styled(
        title.to_owned(),
        theme.fg(section.tok()).add_modifier(Modifier::BOLD),
    )]
}

/// The panel surface: the theme's inset fill, or the terminal background when the theme has none.
pub fn surface(theme: &Theme) -> Style {
    theme.bg(Tok::PanelBg)
}

/// `spans` drawn on the panel surface: spans that set no background of their own get the fill.
pub fn on_surface(theme: &Theme, spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let fill = surface(theme);
    spans
        .into_iter()
        .map(|mut span| {
            span.style = fill.patch(span.style);
            span
        })
        .collect()
}

/// The borderless panel surface with one cell of breathing room on every side, so its inner area
/// is the same as a one-cell frame's would be.
pub fn panel_block(theme: &Theme) -> Block<'static> {
    Block::default()
        .style(surface(theme))
        .padding(Padding::uniform(1))
}

/// [`panel_block`] whose top row carries the title, ` Settings ……… extra `: the title in bold
/// section color, `extra` muted and italic at the right edge when non-empty.
pub fn panel_block_titled(
    theme: &Theme,
    section: Section,
    title: &str,
    extra: &str,
) -> Block<'static> {
    let mut left = vec![Span::raw(" ")];
    left.extend(fig_label(theme, section, title));
    let mut block = Block::default()
        .style(surface(theme))
        .padding(Padding {
            left: 1,
            right: 1,
            top: 0,
            bottom: 1,
        })
        .title_top(Line::from(left));
    if !extra.is_empty() {
        block = block.title_top(
            Line::from(Span::styled(
                format!("{extra} "),
                theme.fg(Tok::Muted).add_modifier(Modifier::ITALIC),
            ))
            .alignment(Alignment::Right),
        );
    }
    block
}

/// Rows drawn inline (not through a ratatui block) on the panel surface: a title row
/// ` Title ……… extra `, each row as `  row  ` padded to `width`, then one blank surface row. Rows
/// wider than the inner width are clipped. Every returned line is exactly `width` cells (when ≥ 6).
pub fn framed_lines(
    theme: &Theme,
    section: Section,
    title: &str,
    extra: &str,
    rows: Vec<Line<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    if width < 6 {
        return rows;
    }
    let fill = surface(theme);
    let inner = width - 4;
    let ellipsis = theme.sym(Sym::Ellipsis).to_owned();
    let mut head = vec![Span::raw(" ")];
    let label = fig_label(theme, section, &text::one_line(title));
    head.extend(super::markdown::truncate_spans(label, width - 2, &ellipsis));
    let used: usize = head.iter().map(|s| text::width(&s.content)).sum();
    let extra = text::one_line(extra);
    let tail = if extra.is_empty() || used + text::width(&extra) + 3 > width {
        String::new()
    } else {
        format!("{extra} ")
    };
    head.push(Span::raw(
        " ".repeat(width.saturating_sub(used + text::width(&tail))),
    ));
    if !tail.is_empty() {
        head.push(Span::styled(
            tail,
            theme.fg(Tok::Muted).add_modifier(Modifier::ITALIC),
        ));
    }
    let mut out = vec![Line::from(on_surface(theme, head))];
    for row in rows {
        let style = fill.patch(row.style);
        let mut spans = vec![Span::raw("  ")];
        let body = super::markdown::truncate_spans(row.spans, inner, &ellipsis);
        let used: usize = body.iter().map(|s| text::width(&s.content)).sum();
        spans.extend(body.into_iter().map(|mut span| {
            span.style = style.patch(span.style);
            span
        }));
        spans.push(Span::styled(" ".repeat(inner.saturating_sub(used)), style));
        spans.push(Span::raw("  "));
        out.push(Line::from(on_surface(theme, spans)));
    }
    out.push(Line::from(Span::styled(" ".repeat(width), fill)));
    out
}

/// Writes `hint` muted and right-aligned into the last row of `area`, but only when that row is
/// blank (the panel's bottom padding) and wide enough: it never covers panel content. Returns
/// whether it was drawn.
pub fn stamp_bottom_hint(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    theme: &Theme,
    hint: &str,
) -> bool {
    let hint = format!("{} ", text::one_line(hint));
    let needed = text::width(&hint) as u16;
    if area.height < 3 || area.width < needed.saturating_add(4) {
        return false;
    }
    let y = area.bottom() - 1;
    let blank = (area.left()..area.right()).all(|x| {
        buf.cell((x, y))
            .is_some_and(|cell| cell.symbol().trim().is_empty())
    });
    if !blank {
        return false;
    }
    let x = area.right() - needed;
    buf.set_string(
        x,
        y,
        &hint,
        surface(theme).patch(theme.fg(Tok::Muted).add_modifier(Modifier::ITALIC)),
    );
    true
}

/// Style for a row that is the selected one in a list: the selection fill with bold body text.
pub fn selected_style(theme: &Theme) -> Style {
    theme
        .fg(Tok::Text)
        .patch(theme.bg(Tok::SelectedBg))
        .add_modifier(Modifier::BOLD)
}

/// The selected list row: `spans` restyled by [`select_spans`], clipped with an ellipsis to `width`
/// cells, then filled with the selection background to exactly `width` cells. The one selected-row
/// look for every list.
pub fn selected_row(theme: &Theme, spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut spans = super::markdown::truncate_spans(spans, width, theme.sym(Sym::Ellipsis));
    select_spans(theme, &mut spans);
    let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            theme.bg(Tok::SelectedBg),
        ));
    }
    Line::from(spans).style(theme.bg(Tok::SelectedBg))
}

/// Restyles the spans of the selected list row the way the menus draw it: the selection fill under
/// one body color, since accents and status colors blur into a colored fill. Primary text turns
/// bold; muted and dim details keep their normal weight, so they read as secondary; bold accent
/// emphasis (search matches) is underlined so it still stands out.
pub fn select_spans(theme: &Theme, spans: &mut [Span<'static>]) {
    let fill = theme.bg(Tok::SelectedBg);
    let body = theme.fg(Tok::Text);
    let accent = theme.fg(Tok::Accent).fg;
    let faded = [theme.fg(Tok::Muted).fg, theme.fg(Tok::Dim).fg];
    for span in spans {
        let fg = span.style.fg;
        let secondary = fg.is_some() && faded.contains(&fg);
        let emphasis = fg.is_some()
            && fg == accent
            && span.style.add_modifier.contains(Modifier::BOLD)
            && !span.content.trim().is_empty();
        let style = span.style.patch(body).patch(fill);
        span.style = if secondary {
            style.remove_modifier(Modifier::BOLD)
        } else if emphasis {
            style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            style.add_modifier(Modifier::BOLD)
        };
    }
}

#[cfg(test)]
mod tests;
