//! The frame heavy tool cards are drawn in: a rounded border whose top edge carries the call's
//! title, whose body holds optionally titled sections, and whose bottom edge carries the facts
//! (`╰─ exit 1 · 1.20s ─╯`). One primitive draws every framed card (MCP service calls, commands,
//! edits, web calls, workers), so their edges, clipping and fills agree.
use crate::ui::{
    Rows,
    markdown::truncate_spans,
    text,
    theme::{Sym, Theme, Tok},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

/// Narrowest frame worth drawing. Below it callers keep their unframed rows.
pub(in crate::ui) const MIN_W: usize = 12;

/// Cells a frame takes from each body row: `│ ` on the left and ` │` on the right.
pub(in crate::ui) const CHROME_W: usize = 4;

pub(in crate::ui) struct Frame {
    /// Spans written into the top edge.
    pub head: Vec<Span<'static>>,
    /// Body sections in order. A labelled section opens with `├─ Label ─┤`; an unlabelled one after
    /// the first opens with a plain divider.
    pub sections: Vec<(Option<String>, Rows)>,
    /// Spans written into the bottom edge; an empty foot draws a plain edge.
    pub foot: Vec<Span<'static>>,
    /// Foreground of the border glyphs.
    pub border: Style,
    /// Background under every cell of the frame (the terminal's own when unset).
    pub fill: Style,
}

impl Frame {
    pub fn new(head: Vec<Span<'static>>, border: Style) -> Self {
        Frame {
            head,
            sections: Vec::new(),
            foot: Vec::new(),
            border,
            fill: Style::default(),
        }
    }

    /// The frame as rows exactly `w` cells wide. Callers check `w >= MIN_W` first; narrower widths
    /// still never overflow, they only lose the title.
    pub fn rows(self, theme: &Theme, w: usize) -> Rows {
        let Frame {
            head,
            sections,
            foot,
            border,
            fill,
        } = self;
        let pen = Pen {
            theme,
            w,
            border: fill.patch(border),
            fill,
        };
        let mut out = vec![pen.edge(Sym::BoxTl, head, Sym::BoxTr)];
        for (i, (label, rows)) in sections.into_iter().enumerate() {
            match label {
                Some(label) => out.push(pen.edge(
                    Sym::TeeL,
                    vec![Span::styled(
                        text::one_line(&label),
                        theme.fg(Tok::Muted).add_modifier(Modifier::BOLD),
                    )],
                    Sym::TeeR,
                )),
                None if i > 0 => out.push(pen.edge(Sym::TeeL, Vec::new(), Sym::TeeR)),
                None => {}
            }
            out.extend(rows.into_iter().map(|row| pen.body(row)));
        }
        out.push(pen.edge(Sym::BoxBl, foot, Sym::BoxBr));
        out
    }
}

struct Pen<'a> {
    theme: &'a Theme,
    w: usize,
    border: Style,
    fill: Style,
}

impl Pen<'_> {
    /// `{left}─ label ───{right}`, or a bare rule when the label is empty or there is no room.
    fn edge(&self, left: Sym, label: Vec<Span<'static>>, right: Sym) -> Line<'static> {
        let theme = self.theme;
        let h = theme.sym(Sym::BoxH);
        let mark = theme.sym(Sym::Ellipsis);
        let mut spans = vec![Span::styled(format!("{}{h}", theme.sym(left)), self.border)];
        // Two corner cells, one rule cell each side and a space each side of the label.
        let label = truncate_spans(label, self.w.saturating_sub(6), mark);
        let label_w: usize = label.iter().map(|s| text::width(&s.content)).sum();
        let mut used = 2;
        if label_w > 0 {
            spans.push(Span::styled(" ", self.border));
            spans.extend(label.into_iter().map(|mut span| {
                span.style = self.fill.patch(span.style);
                span
            }));
            spans.push(Span::styled(" ", self.border));
            used += label_w + 2;
        }
        spans.push(Span::styled(
            h.repeat(self.w.saturating_sub(used + 1)),
            self.border,
        ));
        spans.push(Span::styled(theme.sym(right).to_owned(), self.border));
        clip(spans, self.w, mark)
    }

    /// `│ row … │`, the row clipped to the inner width and padded on the fill.
    fn body(&self, row: Line<'static>) -> Line<'static> {
        let theme = self.theme;
        let mark = theme.sym(Sym::Ellipsis);
        let v = theme.sym(Sym::BoxV);
        let inner = self.w.saturating_sub(CHROME_W);
        let style = self.fill.patch(row.style);
        let body = truncate_spans(row.spans, inner, mark);
        let used: usize = body.iter().map(|s| text::width(&s.content)).sum();
        let mut spans = vec![Span::styled(format!("{v} "), self.border)];
        spans.extend(body.into_iter().map(|mut span| {
            span.style = style.patch(span.style);
            span
        }));
        spans.push(Span::styled(" ".repeat(inner.saturating_sub(used)), style));
        spans.push(Span::styled(format!(" {v}"), self.border));
        clip(spans, self.w, mark)
    }
}

/// Frames narrower than their chrome degrade to clipped rows instead of overflowing.
fn clip(spans: Vec<Span<'static>>, w: usize, mark: &str) -> Line<'static> {
    let width: usize = spans.iter().map(|s| text::width(&s.content)).sum();
    if width <= w {
        Line::from(spans)
    } else {
        Line::from(truncate_spans(spans, w, mark))
    }
}

/// A status or metadata chip, `⟦done⟧` (`[done]` in ASCII), in one foreground color.
pub(in crate::ui) fn chip(theme: &Theme, label: &str, tok: Tok) -> Span<'static> {
    let (open, close) = if theme.ascii() {
        ("[", "]")
    } else {
        ("⟦", "⟧")
    };
    Span::styled(
        format!("{open}{}{close}", text::one_line(label)),
        theme.fg(tok),
    )
}

#[cfg(test)]
mod tests;
