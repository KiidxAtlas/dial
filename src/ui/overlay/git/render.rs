use super::super::{
    DrawCtx,
    settings::{filled, fit, hint},
};
use super::{
    GitOverlay,
    model::*,
    tree::{self, Entry},
    view,
};
use crate::ui::images::{Placement, Thumbnail};
use crate::ui::{
    chrome,
    keys::Action,
    text,
    theme::{Sym, Theme, Tok},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

impl GitOverlay {
    // ---- rendering ----

    pub(super) fn compose(&self, width: u16, height: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(width), usize::from(height));
        if w < 20 || h < 4 {
            return Vec::new();
        }
        let theme = ctx.theme;
        let narrow = w < view::NARROW_COLS;
        let side_w = if narrow {
            0
        } else {
            (w * 3 / 10).clamp(26, 48)
        };
        let sep = if side_w > 0 { 1 } else { 0 };
        let diff_w = w - side_w - sep;
        let body_h = h - 2;
        // Narrow terminals stack the changes sheet under the diff and keep the top share for the diff.
        let stack_h = if narrow {
            (body_h / 3).min(body_h.saturating_sub(3))
        } else {
            0
        };
        let diff_h = body_h - stack_h;
        self.body_h.set(diff_h);
        let sep_text = theme.sym(Sym::BoxV).to_owned();
        let join = |left: Vec<Span<'static>>, right: Vec<Span<'static>>| -> Line<'static> {
            let mut spans = left;
            if side_w > 0 {
                spans.push(Span::styled(sep_text.clone(), theme.fg(Tok::BorderMuted)));
                spans.extend(right);
            }
            Line::from(spans)
        };

        let mut out = Vec::with_capacity(h);
        if narrow {
            out.push(Line::from(self.diff_header(w, theme)));
            out.extend(
                self.diff_column(w, diff_h, theme)
                    .into_iter()
                    .map(Line::from),
            );
            if stack_h > 0 {
                out.push(Line::from(pad(
                    vec![Span::raw(" "), chrome::label(theme, "Changes")],
                    w,
                )));
                out.extend(
                    self.side_column(w, stack_h - 1, theme)
                        .into_iter()
                        .map(Line::from),
                );
            }
        } else {
            let side_head = vec![Span::raw(" "), chrome::label(theme, "Changes")];
            out.push(join(
                self.diff_header(diff_w, theme),
                pad(side_head, side_w),
            ));

            let diff_rows = self.diff_column(diff_w, diff_h, theme);
            let side_rows = self.side_column(side_w, body_h, theme);
            for index in 0..body_h {
                out.push(join(
                    diff_rows.get(index).cloned().unwrap_or_default(),
                    side_rows.get(index).cloned().unwrap_or_default(),
                ));
            }
        }

        let footer = match &self.status {
            Some((message, error, at)) if at.elapsed() < STATUS_TTL => {
                let tok = if *error { Tok::Error } else { Tok::Success };
                Line::from(vec![Span::styled(format!(" {message}"), theme.fg(tok))])
            }
            _ => footer_line(theme, &self.hints(ctx), w),
        };
        out.push(footer);
        out
    }

    fn hints(&self, ctx: &DrawCtx) -> Vec<String> {
        if self.editing {
            return vec![
                "ctrl+s commit".to_owned(),
                "ctrl+g generate".to_owned(),
                "ctrl+t amend".to_owned(),
                "esc stop editing".to_owned(),
            ];
        }
        let mut hints = vec![hint(ctx.keys, Action::SelectCancel, "close")];
        hints.push("tab focus".to_owned());
        hints.push("alt+↓/↑ hunk".to_owned());
        hints.push("]/[ file".to_owned());
        hints.push("v view".to_owned());
        hints.push("b whitespace".to_owned());
        hints.push("w wrap".to_owned());
        hints.push("r refresh".to_owned());
        if self.commit.is_none() {
            hints.push("s/space stage".to_owned());
            hints.push("x discard hunk".to_owned());
            hints.push("del discard file".to_owned());
            hints.push("c commit".to_owned());
            hints.push("A ai stage".to_owned());
        }
        hints.extend(self.line_hints());
        hints
    }

    fn diff_header(&self, width: usize, theme: &Theme) -> Vec<Span<'static>> {
        let dim = theme.fg(Tok::Dim);
        let mut spans = Vec::new();
        match self.shown.as_ref() {
            Some(shown) => {
                let change = &shown.change;
                let (dir, base) = split_path(&change.path);
                spans.push(Span::styled(format!(" {dir}"), dim));
                spans.push(Span::styled(
                    base.to_owned(),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                if let Some(orig) = &change.orig {
                    spans.push(Span::styled(format!(" ← {orig}"), dim));
                }
                if shown.binary.is_some() {
                    spans.push(Span::styled("  binary", dim));
                } else {
                    spans.push(Span::styled(
                        format!("  +{}", shown.doc.additions),
                        theme.fg(Tok::Success),
                    ));
                    spans.push(Span::styled(
                        format!(" -{}", shown.doc.deletions),
                        theme.fg(Tok::Error),
                    ));
                }
                spans.push(Span::styled(
                    format!("  {}", area_label(change, self.commit.is_some())),
                    theme.fg(Tok::Muted),
                ));
            }
            None => spans.push(Span::styled(" no file selected", dim)),
        }

        let tabs = [
            (View::File, "1 file"),
            (View::Split, "2 split"),
            (View::Inline, "3 inline"),
            (View::Hunk, "4 hunk"),
        ];
        let mut right = Vec::new();
        for (view, label) in tabs {
            let style = if self.view == view {
                theme
                    .fg(Tok::Accent)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                theme.fg(Tok::Muted)
            };
            right.push(Span::styled(format!(" {label} "), style));
        }
        let chip = self.whitespace.chip();
        if !chip.is_empty() {
            right.push(Span::styled(chip, theme.fg(Tok::Warning)));
        }
        if self.wrap {
            right.push(Span::styled(" wrap", theme.fg(Tok::Warning)));
        }
        let right_w: usize = right.iter().map(|s| text::width(&s.content)).sum();
        let mut line = pad(spans, width.saturating_sub(right_w));
        line.extend(right);
        line
    }

    /// Native preview of a binary change: a caption row, the image (both sides side by side in the
    /// split view), a blank row, then the metadata rows. Returns None when no image can be drawn or
    /// the stack does not fit the body, so the text placeholder is drawn instead.
    fn binary_column(
        &self,
        width: usize,
        body_h: usize,
        theme: &Theme,
    ) -> Option<Vec<Vec<Span<'static>>>> {
        let previews = self.shown.as_ref()?.binary.as_ref()?.previews.as_ref()?;
        let muted = theme.fg(Tok::Muted);
        let blank = || vec![Span::raw(" ".repeat(width))];
        let meta: Vec<Vec<Span<'static>>> = self
            .rows
            .iter()
            .filter_map(|row| match &row.kind {
                RowKind::Meta(text) if text.trim() != view::NO_PREVIEW => {
                    Some(pad(vec![Span::styled(text.clone(), muted)], width))
                }
                _ => None,
            })
            .collect();
        // Each side as (label, thumbnail, column origin, column width).
        let sides: Vec<(&str, &Thumbnail, usize, usize)> = match (&previews.before, &previews.after)
        {
            (Some(before), Some(after)) if self.view == View::Split => {
                let left = width.saturating_sub(1) / 2;
                vec![
                    ("before", before, 0, left),
                    ("after", after, left + 1, width.saturating_sub(left + 1)),
                ]
            }
            (_, Some(after)) => vec![("after", after, 0, width)],
            (Some(before), None) => vec![("before", before, 0, width)],
            (None, None) => return None,
        };
        let mut images = Vec::with_capacity(sides.len());
        for (_, thumb, x, col_w) in &sides {
            images.push((*x, thumb.native(u16::try_from(*col_w).ok()?)?));
        }
        let img_h = images.iter().map(|(_, image)| image.rows()).max()?;
        // Caption, image rows, a blank row and the metadata must all fit in the body.
        if 2 + img_h + meta.len() > body_h {
            return None;
        }
        let mut caption = Vec::new();
        let mut cursor = 0;
        for (label, _, x, col_w) in &sides {
            caption.push(Span::raw(" ".repeat(*x - cursor)));
            let text = format!(" {label}");
            let fill = col_w.saturating_sub(text.chars().count());
            caption.push(Span::styled(text, muted));
            caption.push(Span::raw(" ".repeat(fill)));
            cursor = x + col_w;
        }
        if cursor < width {
            caption.push(Span::raw(" ".repeat(width - cursor)));
        }
        let mut out = vec![caption];
        out.extend((0..img_h).map(|_| blank()));
        out.push(blank());
        out.extend(meta);
        // Body row 1 is the first image row, directly under the caption.
        self.placements
            .borrow_mut()
            .extend(images.into_iter().map(|(x, image)| Placement {
                x: (x + usize::from(image.indent())) as u16,
                y: 1,
                image,
            }));
        Some(out)
    }

    /// Body rows of the diff column, each exactly `width` wide (minimap included). `diff_top` and
    /// the cursor count visual lines: a row takes several lines when wrapping.
    fn diff_column(&self, width: usize, body_h: usize, theme: &Theme) -> Vec<Vec<Span<'static>>> {
        if let Some(rows) = self.binary_column(width, body_h, theme) {
            return rows;
        }
        let mm = if width >= 40 { 2 } else { 0 };
        let text_w = width - mm;
        let heights: Vec<usize> = self
            .rows
            .iter()
            .map(|row| {
                if self.wrap {
                    view::segments(row, text_w).len()
                } else {
                    1
                }
            })
            .collect();
        let total: usize = heights.iter().sum();
        let cursor = self.cursor_row.min(heights.len().saturating_sub(1));
        let cursor_top: usize = heights.iter().take(cursor).sum();
        let cursor_end = cursor_top + heights.get(cursor).copied().unwrap_or(1);
        let max_top = total.saturating_sub(body_h);
        let mut top = self.diff_top.get().min(max_top);
        if cursor_top < top {
            top = cursor_top;
        } else if cursor_end > top + body_h {
            top = cursor_end.saturating_sub(body_h);
        }
        top = top.min(max_top);
        self.diff_top.set(top);

        // Visual lines in view, each tagged with the row it belongs to.
        let mut visible: Vec<(usize, Vec<Span<'static>>)> = Vec::with_capacity(body_h);
        let mut y = 0;
        for (index, row) in self.rows.iter().enumerate() {
            if y >= top + body_h {
                break;
            }
            let height = heights[index];
            if y + height > top {
                for (k, spans) in self
                    .row_lines(index, row, text_w, theme)
                    .into_iter()
                    .enumerate()
                {
                    let at = y + k;
                    if at >= top && at < top + body_h {
                        visible.push((index, spans));
                    }
                }
            }
            y += height;
        }
        let marks: Vec<Option<Side>> = self
            .rows
            .iter()
            .zip(&heights)
            .flat_map(|(row, &height)| std::iter::repeat(row_mark(row)).take(height))
            .collect();

        let focused = self.focus == Focus::Diff;
        let mut visible = visible.into_iter();
        (0..body_h)
            .map(|i| {
                let mut spans = match visible.next() {
                    Some((index, mut spans)) => {
                        if focused && index == self.cursor_row {
                            for span in &mut spans {
                                span.style = span.style.add_modifier(Modifier::BOLD);
                            }
                        }
                        spans
                    }
                    None if i == 0 => pad(
                        vec![Span::styled(self.empty_message(), theme.fg(Tok::Muted))],
                        text_w,
                    ),
                    None => pad(Vec::new(), text_w),
                };
                spans.extend(Self::minimap_cell(&marks, i, body_h, top, mm, theme));
                spans
            })
            .collect()
    }

    /// Visual lines of one diff row, each padded to `text_w`. Unwrapped rows give exactly one line.
    fn row_lines(
        &self,
        index: usize,
        row: &Row,
        text_w: usize,
        theme: &Theme,
    ) -> Vec<Vec<Span<'static>>> {
        if !self.wrap {
            let content = if self.view == View::Split {
                split_spans(row, text_w, self.hscroll, theme)
            } else {
                inline_spans(row, self.hscroll, theme)
            };
            return vec![pad(self.with_line_marker(index, content, theme), text_w)];
        }
        view::segments(row, text_w)
            .into_iter()
            .enumerate()
            .map(|(k, (left, right))| {
                let first = k == 0;
                let spans = match &row.kind {
                    RowKind::Pair {
                        left: left_col,
                        right: right_col,
                    } => {
                        let (left_w, right_w) = view::split_widths(text_w);
                        let mut spans = wrapped_cell(left_col.as_ref(), first, left, left_w, theme);
                        spans.push(Span::styled("│", theme.fg(Tok::BorderMuted)));
                        spans.extend(wrapped_cell(
                            right_col.as_ref(),
                            first,
                            right,
                            right_w,
                            theme,
                        ));
                        spans
                    }
                    RowKind::Line { side, old, new, .. } => {
                        let mut spans = if first {
                            vec![
                                Span::styled(
                                    format!("{} {} ", num_cell(*old), num_cell(*new)),
                                    theme.fg(Tok::Dim),
                                ),
                                Span::styled(
                                    format!("{} ", sign(*side)),
                                    theme.fg(side_tok(*side)),
                                ),
                            ]
                        } else {
                            vec![Span::raw(" ".repeat(view::INLINE_GUTTER))]
                        };
                        spans.push(Span::styled(left, theme.fg(side_tok(*side))));
                        spans
                    }
                    RowKind::Meta(_) => vec![Span::styled(left, theme.fg(Tok::Muted))],
                    RowKind::Hunk(_) => vec![Span::styled(left, theme.fg(Tok::Accent))],
                };
                pad(self.with_line_marker(index, spans, theme), text_w)
            })
            .collect()
    }

    fn empty_message(&self) -> String {
        if self.commit.is_some() {
            if self.changes.is_empty() {
                " This commit changes no files.".to_owned()
            } else {
                " Select a file to see its changes.".to_owned()
            }
        } else if self.changes.is_empty() {
            " The working tree is clean.".to_owned()
        } else {
            " Select a file to see its changes.".to_owned()
        }
    }

    /// Two-column minimap: a block per slot of visual lines, a highlighted band where the viewport sits.
    fn minimap_cell(
        marks: &[Option<Side>],
        index: usize,
        body_h: usize,
        top: usize,
        mm: usize,
        theme: &Theme,
    ) -> Vec<Span<'static>> {
        if mm == 0 {
            return Vec::new();
        }
        let n = marks.len();
        let start = index * n / body_h;
        let end = ((index + 1) * n / body_h).max(start + 1);
        let mut mark = None;
        for slot in marks.iter().take(end.min(n)).skip(start) {
            match slot {
                Some(Side::Added) => {
                    mark = Some(Side::Added);
                    break;
                }
                Some(_) => mark = mark.or(Some(Side::Removed)),
                None => {}
            }
        }
        let in_view = end > top && start < top + body_h;
        let (glyph, tok) = match mark {
            Some(Side::Added) => ("█", Tok::DiffAdded),
            Some(_) => ("█", Tok::DiffRemoved),
            None if in_view => (theme.sym(Sym::BoxV), Tok::Muted),
            None => (" ", Tok::Dim),
        };
        vec![
            Span::styled(glyph.to_owned(), theme.fg(tok)),
            Span::raw(" "),
        ]
    }

    /// Sidebar body rows, each exactly `width` wide.
    fn side_column(&self, width: usize, body_h: usize, theme: &Theme) -> Vec<Vec<Span<'static>>> {
        let muted = theme.fg(Tok::Muted);
        let bold = |tok: Tok| theme.fg(tok).add_modifier(Modifier::BOLD);
        let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
        let mut cursor_line = 0usize;

        let groups: Vec<(Area, &str)> = if let Some(commit) = &self.commit {
            lines.push(vec![
                Span::styled(format!(" {}", commit.short), bold(Tok::Accent)),
                Span::styled(format!(" {}", commit.subject), bold(Tok::Text)),
            ]);
            lines.push(vec![Span::styled(
                format!(" {} · {}", commit.author, commit.date),
                muted,
            )]);
            for body in commit.body.lines() {
                lines.push(vec![Span::styled(format!(" {body}"), theme.fg(Tok::Text))]);
            }
            lines.push(Vec::new());
            vec![(Area::Commit, "Files")]
        } else {
            let branch = self
                .branch
                .clone()
                .unwrap_or_else(|| "detached HEAD".to_owned());
            lines.push(vec![Span::styled(
                format!(" {} {branch}", theme.sym(Sym::IconGit)),
                bold(Tok::Text),
            )]);
            if let Some((sha, subject)) = &self.head {
                lines.push(vec![Span::styled(format!(" HEAD {sha} {subject}"), muted)]);
            }
            lines.push(Vec::new());
            vec![(Area::Staged, "Staged"), (Area::Unstaged, "Changes")]
        };

        let mut row = 0usize;
        for (area, title) in groups {
            let count = self.changes.iter().filter(|c| c.area == area).count();
            lines.push(vec![
                Span::raw(" "),
                chrome::label(theme, &format!("{title} ({count})")),
            ]);
            for entry in tree::flatten(&self.changes, area, &self.collapsed) {
                let selected = row == self.cursor;
                if selected {
                    cursor_line = lines.len();
                }
                lines.push(self.change_row(&entry, selected, width, theme));
                row += 1;
            }
        }

        if self.commit.is_none() {
            lines.push(Vec::new());
            let title = if self.amend {
                "Commit message · amend"
            } else {
                "Commit message"
            };
            lines.push(vec![Span::raw(" "), chrome::label(theme, title)]);
            if self.message.is_empty() {
                let placeholder = if self.editing {
                    "  ▌"
                } else {
                    "  press c to write a message"
                };
                lines.push(vec![Span::styled(placeholder, muted)]);
            } else {
                let count = self.message.split('\n').count();
                for (i, line) in self.message.split('\n').enumerate() {
                    let mut content = format!("  {line}");
                    if self.editing && i + 1 == count {
                        content.push('▌');
                    }
                    lines.push(vec![Span::styled(content, theme.fg(Tok::Text))]);
                }
            }
            if self.editing {
                lines.push(vec![Span::styled(
                    "  ctrl+s commit · ctrl+t amend · ctrl+g generate",
                    muted,
                )]);
            }
        }

        let lines: Vec<Vec<Span<'static>>> = lines.into_iter().map(|l| pad(l, width)).collect();
        let max_top = lines.len().saturating_sub(body_h);
        let mut top = self.side_top.get().min(max_top);
        if cursor_line < top {
            top = cursor_line;
        } else if cursor_line >= top + body_h {
            top = cursor_line + 1 - body_h;
        }
        top = top.min(max_top);
        self.side_top.set(top);
        (0..body_h)
            .map(|i| lines.get(top + i).cloned().unwrap_or_default())
            .collect()
    }

    fn change_row(
        &self,
        entry: &Entry,
        selected: bool,
        width: usize,
        theme: &Theme,
    ) -> Vec<Span<'static>> {
        let pointer = if selected {
            theme.sym(Sym::Pointer).to_owned()
        } else {
            " ".to_owned()
        };
        let indent = "  ".repeat(entry.depth());
        let parts = match entry {
            Entry::Dir {
                name, collapsed, ..
            } => {
                let chevron = if *collapsed { "▸" } else { "▾" };
                vec![
                    Span::styled(format!("{pointer} "), theme.fg(Tok::Accent)),
                    Span::styled(format!("{indent}{chevron} "), theme.fg(Tok::Muted)),
                    Span::styled(format!("{name}/"), theme.fg(Tok::Text)),
                ]
            }
            Entry::File { index, .. } => {
                let change = &self.changes[*index];
                let (letter, tok) = kind_mark(change.kind);
                let label = match &change.orig {
                    Some(orig) => format!("{orig} → {}", change.path),
                    None => split_path(&change.path).1.to_owned(),
                };
                vec![
                    Span::styled(format!("{pointer} "), theme.fg(Tok::Accent)),
                    Span::styled(format!("{indent}{letter} "), theme.fg(tok)),
                    Span::styled(label, theme.fg(Tok::Text)),
                ]
            }
        };
        let mut spans = pad(parts, width);
        if selected && self.focus == Focus::Sidebar {
            let fill = theme.bg(Tok::SelectedBg);
            for span in &mut spans {
                span.style = span.style.patch(fill).add_modifier(Modifier::BOLD);
            }
        }
        spans
    }
}

// ---- rendering helpers ----

fn pad(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    filled(spans, width, Style::default()).spans
}

/// Footer hints: each hint is `key label` with the key in amber bold. Segments drop from the one
/// before the last (the close hint stays) until they fit `width`.
fn footer_line(theme: &Theme, segments: &[String], width: usize) -> Line<'static> {
    let mut keep: Vec<(&str, &str)> = segments
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.split_once(' ').unwrap_or((s.as_str(), "")))
        .collect();
    let cells = |keep: &[(&str, &str)]| -> usize {
        chrome::key_hint_spans(theme, keep)
            .iter()
            .map(|span| text::width(&span.content))
            .sum()
    };
    while keep.len() > 1 && cells(&keep) > width.saturating_sub(1) {
        keep.remove(keep.len() - 2);
    }
    let mut spans = vec![Span::raw(" ")];
    spans.extend(chrome::key_hint_spans(theme, &keep));
    Line::from(fit(spans, width))
}

pub(super) fn plain_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

fn skip_chars(text: &str, count: usize) -> String {
    text.chars().skip(count).collect()
}

fn sign(side: Side) -> char {
    match side {
        Side::Added => '+',
        Side::Removed => '-',
        Side::Context => ' ',
    }
}

fn side_tok(side: Side) -> Tok {
    match side {
        Side::Added => Tok::DiffAdded,
        Side::Removed => Tok::DiffRemoved,
        Side::Context => Tok::Text,
    }
}

fn num_cell(num: Option<u32>) -> String {
    num.map_or_else(|| "    ".to_owned(), |n| format!("{n:>4}"))
}

fn inline_spans(row: &Row, hscroll: usize, theme: &Theme) -> Vec<Span<'static>> {
    match &row.kind {
        // One cell of margin, like every other panel row.
        RowKind::Meta(text) => vec![Span::styled(
            format!(" {}", skip_chars(text, hscroll)),
            theme.fg(Tok::Muted),
        )],
        RowKind::Hunk(text) => vec![Span::styled(
            format!(" {}", skip_chars(text, hscroll)),
            theme.fg(Tok::Accent),
        )],
        RowKind::Line {
            side,
            old,
            new,
            text,
        } => vec![
            Span::styled(
                format!("{} {} ", num_cell(*old), num_cell(*new)),
                theme.fg(Tok::Dim),
            ),
            Span::styled(
                format!("{} {}", sign(*side), skip_chars(text, hscroll)),
                theme.fg(side_tok(*side)),
            ),
        ],
        RowKind::Pair { .. } => Vec::new(),
    }
}

fn cell_spans(col: Option<&Col>, hscroll: usize, theme: &Theme) -> Vec<Span<'static>> {
    match col {
        None => Vec::new(),
        Some(col) => vec![
            Span::styled(format!("{:>4} ", col.num), theme.fg(Tok::Dim)),
            Span::styled(
                format!("{} {}", sign(col.side), skip_chars(&col.text, hscroll)),
                theme.fg(side_tok(col.side)),
            ),
        ],
    }
}

fn split_spans(row: &Row, width: usize, hscroll: usize, theme: &Theme) -> Vec<Span<'static>> {
    match &row.kind {
        RowKind::Pair { left, right } => {
            let left_w = width.saturating_sub(1) / 2;
            let right_w = width.saturating_sub(1 + left_w);
            let mut spans = pad(cell_spans(left.as_ref(), hscroll, theme), left_w);
            spans.push(Span::styled("│", theme.fg(Tok::BorderMuted)));
            spans.extend(pad(cell_spans(right.as_ref(), hscroll, theme), right_w));
            spans
        }
        _ => inline_spans(row, hscroll, theme),
    }
}

/// One side of a wrapped split row, `width` columns wide. The line number and sign show on the
/// first visual line only; continuation lines keep the gutter blank.
fn wrapped_cell(
    col: Option<&Col>,
    first: bool,
    text: String,
    width: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let Some(col) = col else {
        return pad(Vec::new(), width);
    };
    let mut spans = if first {
        vec![
            Span::styled(format!("{:>4} ", col.num), theme.fg(Tok::Dim)),
            Span::styled(format!("{} ", sign(col.side)), theme.fg(side_tok(col.side))),
        ]
    } else {
        vec![Span::raw(" ".repeat(view::CELL_GUTTER))]
    };
    spans.push(Span::styled(text, theme.fg(side_tok(col.side))));
    pad(spans, width)
}

fn row_mark(row: &Row) -> Option<Side> {
    match &row.kind {
        RowKind::Line {
            side: Side::Context,
            ..
        } => None,
        RowKind::Line { side, .. } => Some(*side),
        RowKind::Pair { left, right } => match (left, right) {
            (
                _,
                Some(Col {
                    side: Side::Added, ..
                }),
            ) => Some(Side::Added),
            (
                Some(Col {
                    side: Side::Removed,
                    ..
                }),
                _,
            ) => Some(Side::Removed),
            _ => None,
        },
        _ => None,
    }
}

fn kind_mark(kind: Kind) -> (&'static str, Tok) {
    match kind {
        Kind::Modified => ("M", Tok::Warning),
        Kind::Added => ("A", Tok::Success),
        Kind::Deleted => ("D", Tok::Error),
        Kind::Renamed => ("R", Tok::Accent),
        Kind::Untracked => ("?", Tok::Muted),
        Kind::Conflicted => ("U", Tok::Error),
    }
}

fn area_label(change: &Change, pinned: bool) -> &'static str {
    if pinned {
        return "commit";
    }
    match (change.area, change.kind) {
        (Area::Staged, _) => "staged",
        (_, Kind::Untracked) => "untracked",
        (_, Kind::Conflicted) => "conflicted",
        _ => "unstaged",
    }
}
