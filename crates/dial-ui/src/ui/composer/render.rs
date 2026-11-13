//! Composer drawing: the bordered box, the busy chooser, the strip and the layout of the draft.
use super::super::{
    Rows,
    keys::Action,
    status::InputMode,
    statusline, text,
    theme::{Sym, Theme, Tok},
};
use super::queue::{queued_route, steer_trail};
use super::{
    BusyChoice, Choice, Composer, ComposerCtx, ComposerView, Explicit, SteerKind, SteerStage,
};
use crate::llm::model_config::{ComposerShape, UiConfig};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;

/// Draft rows shown at once; the window follows the cursor.
pub(super) const MAX_INPUT_ROWS: usize = 10;

/// Under this many cells the composer degrades to a bare input row.
pub(super) const MIN_BOX_WIDTH: usize = 8;

pub(super) const MAX_NAME_COLUMN: usize = 24;

/// One row of the steer/queue strip above the composer, indented like the status row:
/// ` ○ Steer  pending · applies after the current step  “message”`. The glyph and bold label carry
/// the state; the message is muted and dropped first when the row is narrow.
pub(super) fn strip_row(
    theme: &Theme,
    width: usize,
    lead: Vec<Span<'static>>,
    (label, tok): (&str, Tok),
    detail: &str,
    message: &str,
) -> Line<'static> {
    let lead_w: usize = lead.iter().map(|s| text::width(&s.content)).sum();
    if lead_w >= width {
        return Line::from(Span::styled(
            text::ellipsize(&format!(" {label}"), width, theme.ascii()),
            theme.fg(tok).add_modifier(Modifier::BOLD),
        ));
    }
    let head = format!(" {label}  ");
    let room = width - lead_w;
    let head = text::ellipsize(&head, room, theme.ascii());
    let room = room.saturating_sub(text::width(&head));
    let detail = text::ellipsize(detail, room, theme.ascii());
    let room = room.saturating_sub(text::width(&detail));
    let quoted = if room > 6 {
        let (open, close) = if theme.ascii() {
            ('"', '"')
        } else {
            ('“', '”')
        };
        text::ellipsize(
            &format!("  {open}{}{close}", text::one_line(message)),
            room,
            theme.ascii(),
        )
    } else {
        String::new()
    };
    let mut spans = lead;
    spans.extend([
        Span::styled(head, theme.fg(tok).add_modifier(Modifier::BOLD)),
        Span::styled(detail, theme.fg(tok)),
        Span::styled(quoted, theme.fg(Tok::Muted)),
    ]);
    Line::from(spans)
}

/// The launch/compose placeholder, longest first; the first that fits is shown whole.
pub(super) const PLACEHOLDERS: [&str; 3] = [
    "Ask anything, or describe what to build",
    "Ask or describe a task",
    "Ask anything",
];

/// The launch input asks the home screen's question itself, so the hero carries no second copy.
pub(super) const LANDING_PLACEHOLDERS: [&str; 2] = ["What are we building?", "Ask anything"];

/// Text rows the launch input shows even when empty (see `view_with`).
pub(super) const LANDING_INPUT_ROWS: usize = 2;

// ---------------------------------------------------------------------------------------------
// Layout and rendering helpers
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Text,
    Ghost,
    Marker,
}

impl Kind {
    pub(super) fn style(self, theme: &Theme) -> Style {
        match self {
            Kind::Text => theme.fg(Tok::Text),
            Kind::Ghost => theme.fg(Tok::Dim).add_modifier(Modifier::DIM),
            Kind::Marker => theme.fg(Tok::ShellMode),
        }
    }
}

pub(super) struct Layout {
    pub(super) rows: Vec<Vec<(String, Kind)>>,
    /// `(row, column)` of the text cursor; always inside `width` cells.
    pub(super) cursor: (usize, usize),
}

/// Hard-wrap `text` followed by the dim `ghost` at `width` cells, tracking where the cursor (a byte
/// offset into `text`) lands. A wide grapheme that does not fit moves to the next row; one wider than
/// the row becomes `?`. A cursor that would sit past the last cell of a full row wraps to a new row.
pub(super) fn layout(text: &str, ghost: &str, cursor: usize, width: usize, shell: bool) -> Layout {
    let width = width.max(1);
    let split = text.len();
    let graphemes = text
        .grapheme_indices(true)
        .map(|(i, g)| (i, g, false))
        .chain(
            ghost
                .grapheme_indices(true)
                .map(|(i, g)| (split + i, g, true)),
        );
    let mut rows: Vec<Vec<(String, Kind)>> = vec![Vec::new()];
    let mut col = 0;
    let mut at: Option<(usize, usize)> = None;
    for (offset, grapheme, is_ghost) in graphemes {
        let here = offset == cursor;
        if grapheme == "\n" {
            if here && col >= width {
                rows.push(Vec::new());
                col = 0;
            }
            if here {
                at = Some((rows.len() - 1, col));
            }
            rows.push(Vec::new());
            col = 0;
            continue;
        }
        let shown = if text::width(grapheme) > width {
            "?"
        } else {
            grapheme
        };
        let size = text::width(shown);
        if col + size > width {
            rows.push(Vec::new());
            col = 0;
        }
        if here {
            at = Some((rows.len() - 1, col));
        }
        let kind = if is_ghost {
            Kind::Ghost
        } else if shell && offset == 0 && shown == "!" {
            Kind::Marker
        } else {
            Kind::Text
        };
        let row = rows.last_mut().expect("rows is never empty");
        match row.last_mut() {
            Some((run, k)) if *k == kind => run.push_str(shown),
            _ => row.push((shown.to_owned(), kind)),
        }
        col += size;
    }
    let cursor = at.unwrap_or_else(|| {
        if col >= width {
            rows.push(Vec::new());
            col = 0;
        }
        (rows.len() - 1, col)
    });
    Layout { rows, cursor }
}

/// One bottom-border hint: an optional key (drawn amber bold) and its label (muted). A `warn`
/// hint is drawn whole in the warning color: the next press of its key has a lasting effect.
pub(super) struct Hint {
    pub(super) key: Option<String>,
    pub(super) what: String,
    pub(super) warn: bool,
}

impl Hint {
    pub(super) fn text(&self) -> String {
        match &self.key {
            Some(key) => format!("{key} {}", self.what),
            None => self.what.clone(),
        }
    }
}

/// `╰─ hint ──────╯`, exactly `w` cells; lower-priority hint items are dropped whole.
pub(super) fn bottom_border(
    w: usize,
    hints: &[Hint],
    left: &str,
    right: &str,
    bar: &str,
    border: Style,
    theme: &Theme,
) -> Line<'static> {
    let frame = text::width(left) + text::width(right) + 2 * text::width(bar);
    let room = w.saturating_sub(frame + 2);
    let sep = format!(" {} ", theme.sym(Sym::Dot));
    let joined = |items: &[Hint]| items.iter().map(Hint::text).collect::<Vec<_>>().join(&sep);
    let mut keep = hints.len();
    while keep > 0 && text::width(&joined(&hints[..keep])) > room {
        keep -= 1;
    }
    let kept = &hints[..keep];
    if kept.is_empty() {
        let corners = text::width(left) + text::width(right);
        return Line::from(Span::styled(
            format!("{left}{}{right}", bar.repeat(w.saturating_sub(corners))),
            border,
        ));
    }
    let fill = room - text::width(&joined(kept));
    let mut spans = vec![Span::styled(format!("{left}{bar} "), border)];
    for (i, hint) in kept.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(sep.clone(), theme.fg(Tok::Muted)));
        }
        let label = if hint.warn {
            theme.fg(Tok::Warning).add_modifier(Modifier::BOLD)
        } else {
            theme.fg(Tok::Muted)
        };
        let key_style = if hint.warn {
            theme.fg(Tok::Warning).add_modifier(Modifier::BOLD)
        } else {
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD)
        };
        match &hint.key {
            Some(key) => {
                spans.push(Span::styled(key.clone(), key_style));
                spans.push(Span::styled(format!(" {}", hint.what), label));
            }
            None => spans.push(Span::styled(hint.what.clone(), label)),
        }
    }
    spans.push(Span::styled(
        format!(" {}{bar}{right}", bar.repeat(fill)),
        border,
    ));
    Line::from(spans)
}

pub(super) fn list_rows(ui: &UiConfig) -> usize {
    ui.autocomplete_items.max(1)
}

/// First visible item for a window of `rows` over `total` items that keeps `selected` in view,
/// moving from the previous `scroll` as little as possible.
pub(super) fn window(selected: usize, scroll: usize, rows: usize, total: usize) -> usize {
    let rows = rows.max(1).min(total.max(1));
    let max_start = total.saturating_sub(rows);
    let mut start = scroll.min(max_start);
    if selected < start {
        start = selected;
    } else if selected >= start + rows {
        start = selected + 1 - rows;
    }
    start.min(max_start)
}

/// Render `rows` list rows starting at item `start`: selected row on `SelectedBg`, a scrollbar in
/// the last column and an `n/m` counter on the last row when the list scrolls. `build` produces a
/// row's spans for an item given its cell budget; the result is clipped to that budget.
pub(super) fn render_list(
    theme: &Theme,
    w: usize,
    total: usize,
    rows: usize,
    start: usize,
    selected: usize,
    build: impl Fn(usize, usize, bool) -> Vec<Span<'static>>,
) -> Rows {
    let scrolls = total > rows;
    let usable = w.saturating_sub(usize::from(scrolls));
    let counter = if scrolls {
        format!("{}/{total}", selected + 1)
    } else {
        String::new()
    };
    let thumb_len = if scrolls {
        (rows * rows / total).max(1)
    } else {
        0
    };
    let max_start = total.saturating_sub(rows);
    let thumb_start = (start * (rows - thumb_len) + max_start / 2)
        .checked_div(max_start)
        .unwrap_or(0);
    (0..rows)
        .map(|r| {
            let index = start + r;
            let is_selected = index == selected;
            let reserve = if scrolls && r + 1 == rows {
                text::width(&counter) + 1
            } else {
                0
            };
            let budget = usable.saturating_sub(reserve);
            let mut spans = clip_spans(build(index, budget, is_selected), budget);
            if scrolls || is_selected {
                let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
                spans.push(Span::raw(" ".repeat(budget - used)));
                if reserve > 0 {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(counter.clone(), theme.fg(Tok::Dim)));
                }
            }
            if is_selected {
                super::super::chrome::select_spans(theme, &mut spans);
            }
            if scrolls {
                let on = r >= thumb_start && r < thumb_start + thumb_len;
                spans.push(if on {
                    Span::styled(theme.sym(Sym::ScrollThumb).to_owned(), theme.fg(Tok::Muted))
                } else {
                    Span::styled(theme.sym(Sym::ScrollTrack).to_owned(), theme.fg(Tok::Dim))
                });
            }
            Line::from(spans)
        })
        .collect()
}

pub(super) fn finish_row(spans: Vec<Span<'static>>, w: usize) -> Line<'static> {
    Line::from(clip_spans(spans, w))
}

/// Cells before the input in the band: the prompt mark one cell in, then the text column the
/// transcript's answers and tool rows also start at.
pub(super) const BAND_LEAD: usize = 3;

/// One row of the composer band: `spans` on the band's tint, padded to exactly `w` cells.
pub(super) fn band_row(spans: Vec<Span<'static>>, w: usize, tint: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = clip_spans(spans, w)
        .into_iter()
        .map(|mut span| {
            span.style = tint.patch(span.style);
            span
        })
        .collect();
    let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
    if used < w {
        spans.push(Span::styled(" ".repeat(w - used), tint));
    }
    Line::from(spans)
}

/// Puts `rows` above the view's rows and moves its cursor down by the same amount.
pub(super) fn prepend_rows(view: &mut ComposerView, rows: Rows) {
    view.cursor.0 += rows.len() as u16;
    let mut below = std::mem::take(&mut view.rows);
    view.rows = rows;
    view.rows.append(&mut below);
}

pub(super) fn clip_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut left = max;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let cells = text::width(&span.content);
        if cells <= left {
            left -= cells;
            out.push(span);
            continue;
        }
        let kept = text::clip(&span.content, left);
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        break;
    }
    out
}

/// `value` as spans with the characters at `hits` (char indexes) emphasized.
pub(super) fn highlighted(
    value: &str,
    hits: &[usize],
    normal: Style,
    hit: Style,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_hit = false;
    for (index, ch) in value.chars().enumerate() {
        let is_hit = hits.contains(&index);
        if is_hit != run_hit && !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if run_hit { hit } else { normal },
            ));
        }
        run_hit = is_hit;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_hit { hit } else { normal }));
    }
    spans
}

/// Char indexes of the first case-insensitive occurrence of `query_lower` in `value`.
pub(super) fn substring_hits(value: &str, query_lower: &str) -> Vec<usize> {
    if query_lower.is_empty() {
        return Vec::new();
    }
    let lower = value.to_lowercase();
    if lower.chars().count() != value.chars().count() {
        return Vec::new();
    }
    let Some(byte) = lower.find(query_lower) else {
        return Vec::new();
    };
    let first = lower[..byte].chars().count();
    (first..first + query_lower.chars().count()).collect()
}

/// Stable per-session accent token (FNV-1a, so it does not change between runs or builds).
pub(super) fn accent_for(session: &str) -> Tok {
    const ACCENTS: [Tok; 6] = [
        Tok::Accent,
        Tok::ThinkLow,
        Tok::ThinkMedium,
        Tok::ThinkHigh,
        Tok::StatusPath,
        Tok::StatusModel,
    ];
    let hash = session.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    ACCENTS[(hash % ACCENTS.len() as u64) as usize]
}

impl Composer {
    // -----------------------------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------------------------

    /// The composer box plus the menu or history-search rows below it, and the cursor within them.
    pub fn view(&self, width: u16, ctx: &ComposerCtx) -> ComposerView {
        self.view_with(width, ctx, false)
    }

    /// A quiet launch input. Editing, menus, search and notices use the normal composer path,
    /// without repeating the conversation status line and shortcut catalog on the home screen.
    pub fn home_view(&self, width: u16, ctx: &ComposerCtx) -> ComposerView {
        self.view_with(width, ctx, true)
    }

    pub(super) fn view_with(&self, width: u16, ctx: &ComposerCtx, landing: bool) -> ComposerView {
        let w = width as usize;
        let theme = ctx.theme;
        let ghost = self.ghost().unwrap_or("");
        if w < MIN_BOX_WIDTH {
            let mut view = self.bare_view(w, ghost, theme);
            if let Some(choice) = &self.choice
                && w > 0
            {
                let rows = self.choice_rows(choice, w, ctx);
                prepend_rows(&mut view, rows);
            }
            if let Some(notice) = &self.queue_notice {
                view.rows.insert(
                    0,
                    Line::from(Span::styled(
                        text::ellipsize(notice, w, theme.ascii()),
                        theme.fg(Tok::Warning),
                    )),
                );
                view.cursor.0 += 1;
            }
            return view;
        }

        // The launch input is the concept's wide rounded pill; in a conversation the configured
        // shape applies.
        let banded = !landing && ctx.ui.composer_shape == ComposerShape::Band;
        let boxed = landing || ctx.ui.composer_shape == ComposerShape::Box;
        let inner = if boxed {
            w - 4
        } else if banded {
            w - BAND_LEAD - 1
        } else {
            w
        };
        let laid = layout(
            &self.editor.text,
            ghost,
            self.editor.cursor,
            inner,
            self.mode() == InputMode::Shell,
        );
        let start = laid
            .cursor
            .0
            .saturating_sub(MAX_INPUT_ROWS - 1)
            .min(laid.rows.len().saturating_sub(MAX_INPUT_ROWS));
        let border = self.border_style(ctx);
        let (bar, side, top_l, top_r, bottom_l, bottom_r) = if boxed {
            (
                theme.sym(Sym::BoxH),
                theme.sym(Sym::BoxV),
                theme.sym(Sym::BoxTl),
                theme.sym(Sym::BoxTr),
                theme.sym(Sym::BoxBl),
                theme.sym(Sym::BoxBr),
            )
        } else {
            let bar = theme.sym(Sym::BoxH);
            (bar, theme.sym(Sym::BoxV), bar, bar, bar, bar)
        };

        let activity = statusline::activity_row(ctx.ui, ctx.status, theme, width);
        // Idle and busy share one status layout (`statusline::segments_for`), so the border keeps
        // its segments when a turn starts or ends and the expand toggle works while busy.
        let header_ui = ctx.ui;
        let activity_rows = usize::from(activity.is_some());
        let mut rows: Rows = activity.into_iter().collect();
        let tint = theme.bg(Tok::UserBg);
        let band = |spans: Vec<Span<'static>>| band_row(spans, w, tint);
        // The band is a tinted card with an accent edge; the edge carries the input mode (accent
        // when idle, the state color while busy or in shell mode), so no prompt glyph is needed.
        let edge_style = if self.mode() == InputMode::Shell || self.busy {
            border
        } else {
            theme.fg(Tok::Accent)
        };
        let edge = || Span::styled(theme.sym(Sym::Quote).to_owned(), edge_style);
        if banded {
            rows.push(band(vec![edge()]));
        } else if landing {
            // The home keeps its status out of the input; the top edge is a plain rounded rule.
            rows.push(Line::from(Span::styled(
                format!("{top_l}{}{top_r}", bar.repeat(w.saturating_sub(2))),
                border,
            )));
        } else {
            rows.push(statusline::border_line_with(
                header_ui,
                ctx.status,
                theme,
                w as u16,
                border,
                statusline::BorderGlyphs {
                    bar,
                    left_corner: top_l,
                    right_corner: top_r,
                },
            ));
        }
        for row in laid.rows.iter().skip(start).take(MAX_INPUT_ROWS) {
            let mut spans = Vec::new();
            if boxed {
                spans.push(Span::styled(format!("{side} "), border));
            } else if banded {
                spans.push(edge());
                spans.push(Span::raw(" ".repeat(BAND_LEAD - 1)));
            }
            let placeholder = self.editor.text.is_empty();
            let used = if placeholder {
                let options: &[&str] = if self.editing_queue.is_some() {
                    &["Edit the queued message", "Edit queued message"]
                } else if self.busy {
                    &["Add direction while Dial works", "Add direction"]
                } else if landing {
                    &LANDING_PLACEHOLDERS
                } else {
                    &PLACEHOLDERS
                };
                let label = options
                    .iter()
                    .find(|label| text::width(label) <= inner)
                    .map_or_else(
                        || text::ellipsize(options[options.len() - 1], inner, theme.ascii()),
                        |label| (*label).to_owned(),
                    );
                let used = text::width(&label);
                spans.push(Span::styled(label, theme.fg(Tok::Muted)));
                used
            } else {
                let used: usize = row.iter().map(|(t, _)| text::width(t)).sum();
                spans.extend(
                    row.iter()
                        .map(|(t, kind)| Span::styled(t.clone(), kind.style(theme))),
                );
                used
            };
            if boxed {
                spans.push(Span::raw(" ".repeat(inner - used + 1)));
                spans.push(Span::styled(side.to_owned(), border));
            }
            rows.push(if banded {
                band(spans)
            } else {
                Line::from(spans)
            });
        }
        // The launch input keeps a second row, so it reads as a place to write rather than a
        // one-line search field. The cursor stays on the first row; typed text fills the second.
        if landing && boxed && laid.rows.len().saturating_sub(start) < LANDING_INPUT_ROWS {
            for _ in laid.rows.len().saturating_sub(start)..LANDING_INPUT_ROWS {
                rows.push(Line::from(vec![
                    Span::styled(format!("{side} "), border),
                    Span::raw(" ".repeat(inner + 1)),
                    Span::styled(side.to_owned(), border),
                ]));
            }
        }
        let cursor = (
            (activity_rows + 1 + laid.cursor.0 - start) as u16,
            (laid.cursor.1
                + if boxed {
                    2
                } else if banded {
                    BAND_LEAD
                } else {
                    0
                }) as u16,
        );
        let mut hints = self.hint_items(ctx);
        if let Some(hold) = self.recording {
            let key = ctx.keys.label(Action::VoiceInput);
            let what = if hold {
                format!("recording, release {key} to stop")
            } else {
                format!("recording, {key} to stop")
            };
            hints.insert(
                0,
                Hint {
                    key: None,
                    what,
                    warn: false,
                },
            );
        }
        if !landing
            && ctx.ui.hints
            && ctx.status.planning.is_some()
            && statusline::controls_facts(ctx.ui, ctx.status, usize::from(width)).is_empty()
        {
            hints.push(Hint {
                key: Some(ctx.keys.label(Action::StatusFocus)),
                what: "controls".to_owned(),
                warn: false,
            });
        }
        if banded {
            // The card closes on a padded row; the status line and hints sit quietly beneath it.
            rows.push(band(vec![edge()]));
            if !landing {
                rows.push(statusline::border_line_with(
                    header_ui,
                    ctx.status,
                    theme,
                    w as u16,
                    theme.fg(Tok::Dim),
                    statusline::BorderGlyphs {
                        bar: " ",
                        left_corner: " ",
                        right_corner: " ",
                    },
                ));
            }
            if !landing || self.recording.is_some() {
                rows.push(bottom_border(
                    w,
                    &hints,
                    " ",
                    " ",
                    " ",
                    Style::default(),
                    theme,
                ));
            }
        } else {
            // The launch pill carries no shortcut catalog unless a recording needs its stop key.
            let shown: &[Hint] = if landing && self.recording.is_none() {
                &[]
            } else {
                &hints
            };
            rows.push(bottom_border(
                w, shown, bottom_l, bottom_r, bar, border, theme,
            ));
        }

        // The controls key alone is one more footer hint; only real facts earn their own row.
        if !landing
            && ctx.status.planning.is_some()
            && !statusline::controls_facts(ctx.ui, ctx.status, usize::from(width)).is_empty()
        {
            rows.push(statusline::controls_row_with(
                ctx.ui, ctx.status, ctx.keys, theme, width,
            ));
        }
        let mut cursor = cursor;
        // Rows stacked above the activity row (menus, the chooser, a notice), so the steer strip
        // can sit right under the activity it changes instead of above everything.
        let mut lead = 0usize;
        if let Some(search) = &self.search {
            // Like the command menu, history search opens upward, above the input, with its
            // query row next to the box, so it never lands below the footer.
            let (list, col) = self.search_rows(search, w, ctx);
            let mut list = list.into_iter();
            let head = list.next();
            let mut above: Rows = list.collect();
            above.extend(head);
            cursor = ((above.len() - 1) as u16, col as u16);
            lead += above.len();
            above.append(&mut rows);
            rows = above;
        } else if let Some(menu) = &self.menu {
            // The menu opens upward, into the space above the input, so the input never moves
            // down and the closest rows to the cursor are the selected command's details.
            let mut menu_rows = self.menu_rows(menu, w, ctx);
            cursor.0 += menu_rows.len() as u16;
            lead += menu_rows.len();
            menu_rows.append(&mut rows);
            rows = menu_rows;
        }
        if let Some(choice) = &self.choice {
            let mut choice_rows = self.choice_rows(choice, w, ctx);
            cursor.0 += choice_rows.len() as u16;
            lead += choice_rows.len();
            choice_rows.append(&mut rows);
            rows = choice_rows;
        }
        if let Some(notice) = &self.queue_notice {
            rows.insert(
                0,
                Line::from(Span::styled(
                    text::ellipsize(notice, w, theme.ascii()),
                    theme.fg(Tok::Warning),
                )),
            );
            cursor.0 += 1;
            lead += 1;
        }
        // Fading is by time, so a recorded row leaves on a later paint without another event.
        let now = std::time::Instant::now();
        let showing: Vec<&Explicit> = self.explicit.iter().filter(|i| i.visible(now)).collect();
        if !showing.is_empty() {
            let mut strip = Vec::new();
            for item in showing {
                // Indented like the status row, with a stage glyph and a bold label so the stage
                // reads at a glance; the message itself stays muted and quoted.
                let (what, stage, tok) = match (item.kind, item.stage) {
                    (SteerKind::Steer, SteerStage::Pending) => (
                        "Steer",
                        "pending · applies after the current step",
                        Tok::Warning,
                    ),
                    (SteerKind::Steer, SteerStage::Received) => {
                        ("Steer", "received · applying it next", Tok::Accent)
                    }
                    (SteerKind::Steer, SteerStage::Applied) => (
                        "Steer",
                        "recorded · now the current instruction",
                        Tok::Success,
                    ),
                    (SteerKind::Independent, SteerStage::Pending) => (
                        "Worker",
                        "pending · starts after the current step",
                        Tok::Warning,
                    ),
                    (SteerKind::Independent, SteerStage::Received) => {
                        ("Worker", "received · starting", Tok::Accent)
                    }
                    (SteerKind::Independent, SteerStage::Applied) => {
                        ("Worker", "started · this task continues", Tok::Success)
                    }
                };
                strip.push(strip_row(
                    theme,
                    w,
                    steer_trail(theme, item.stage, tok),
                    (what, tok),
                    stage,
                    &item.text,
                ));
            }
            // Under the activity row, directly above the draft: your message reads after what
            // Dial is doing now, next to where you typed it.
            let at = (lead + activity_rows).min(rows.len());
            if usize::from(cursor.0) >= at {
                cursor.0 += strip.len() as u16;
            }
            rows.splice(at..at, strip);
        }
        if !self.queued.is_empty() {
            let mut queued = Vec::new();
            for (message, follow_up) in self.queued.iter().take(3) {
                queued.push(strip_row(
                    theme,
                    w,
                    vec![Span::styled(
                        format!(" {}", theme.sym(Sym::ArrowUp)),
                        theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                    )],
                    ("Queued", Tok::Accent),
                    queued_route(*follow_up),
                    message,
                ));
            }
            let hidden = self.queued.len().saturating_sub(3);
            if hidden > 0 {
                queued.push(Line::from(Span::styled(
                    text::ellipsize(&format!("   +{hidden} more queued"), w, theme.ascii()),
                    theme.fg(Tok::Dim),
                )));
            }
            // Most useful first, so a narrow terminal cuts `/queue manage` rather than the keys.
            let submit = ctx.keys.label(Action::Submit);
            let up = ctx.keys.label(Action::SelectUp);
            let mut footer = vec![format!("{} queued", self.queued.len())];
            if !submit.is_empty() {
                footer.push(format!(
                    "{submit} {}",
                    if self.busy {
                        "stop or keep"
                    } else {
                        "send oldest"
                    }
                ));
            }
            if !up.is_empty() {
                footer.push(format!("{up} edit last"));
            }
            footer.push("/queue manage".to_owned());
            queued.push(Line::from(Span::styled(
                text::ellipsize(
                    &footer.join(&format!(" {} ", theme.sym(Sym::Dot))),
                    w,
                    theme.ascii(),
                ),
                theme.fg(Tok::Dim),
            )));
            cursor.0 += queued.len() as u16;
            queued.extend(rows);
            rows = queued;
        }
        if let Some(summary) = self.attachments.summary() {
            rows.insert(
                0,
                Line::from(Span::styled(
                    text::ellipsize(&summary, w, theme.ascii()),
                    theme.fg(Tok::Dim),
                )),
            );
            cursor.0 += 1;
        }
        ComposerView { rows, cursor }
    }

    /// Fewer than `MIN_BOX_WIDTH` cells: just the input row around the cursor, no chrome.
    pub(super) fn bare_view(&self, w: usize, ghost: &str, theme: &Theme) -> ComposerView {
        if w == 0 {
            return ComposerView {
                rows: vec![Line::default()],
                cursor: (0, 0),
            };
        }
        let laid = layout(&self.editor.text, ghost, self.editor.cursor, w, false);
        let row = laid
            .rows
            .get(laid.cursor.0)
            .map(|r| r.as_slice())
            .unwrap_or_default();
        let spans = row
            .iter()
            .map(|(t, kind)| Span::styled(t.clone(), kind.style(theme)))
            .collect::<Vec<_>>();
        ComposerView {
            rows: vec![Line::from(spans)],
            cursor: (0, laid.cursor.1.min(w - 1) as u16),
        }
    }

    /// The busy chooser: what is being decided, its options and the selected option's effect. On the
    /// band layout it is an inset panel, so it reads as its own menu rather than part of the activity
    /// row under it. Details sit beside the labels when every one fits whole; otherwise only the
    /// selected option's detail shows, on its own row, so labels never lose cells to descriptions.
    pub(super) fn choice_rows(&self, choice: &Choice, w: usize, ctx: &ComposerCtx) -> Rows {
        let theme = ctx.theme;
        let queue = choice.options.contains(&BusyChoice::StopAndSend);
        let title = if queue {
            let queued = self.queued.len();
            format!(
                "{queued} queued {}",
                if queued == 1 { "message" } else { "messages" }
            )
        } else {
            "Dial is still working".to_owned()
        };
        let panel = ctx.ui.composer_shape == ComposerShape::Band && w >= 24;
        let outer = w;
        let w = if panel { w - 4 } else { w };
        let pointer = theme.sym(Sym::Pointer);
        let blank_pointer = " ".repeat(text::width(pointer));
        // Pointer, space, option number, space.
        let prefix = text::width(pointer) + 3;
        // The remembered route carries a ` ·` mark after its label (see below).
        let mark = format!(" {}", theme.sym(Sym::Dot));
        let remembered = |option: BusyChoice| !queue && self.last_draft_choice == Some(option);
        let label_col = choice
            .options
            .iter()
            .map(|option| {
                text::width(option.label())
                    + if remembered(*option) {
                        text::width(&mark)
                    } else {
                        0
                    }
            })
            .max()
            .unwrap_or(0);
        let inline = choice
            .options
            .iter()
            .all(|option| prefix + label_col + 2 + text::width(option.detail()) <= w);
        let accent = theme.fg(Tok::Accent);
        let total = choice.options.len();
        let mut rows = Vec::new();
        let dot = theme.sym(Sym::Dot);
        if !panel {
            rows.push(finish_row(
                vec![Span::styled(title.clone(), theme.fg(Tok::Muted))],
                w,
            ));
        }
        rows.extend(render_list(
            theme,
            w,
            total,
            total,
            0,
            choice.selected,
            |i, budget, selected| {
                let option = choice.options[i];
                let mark_w = if remembered(option) {
                    text::width(&mark)
                } else {
                    0
                };
                let label = text::ellipsize(
                    option.label(),
                    budget.saturating_sub(prefix + mark_w),
                    theme.ascii(),
                );
                let mut spans = vec![
                    Span::styled(
                        if selected {
                            pointer
                        } else {
                            blank_pointer.as_str()
                        }
                        .to_owned(),
                        accent,
                    ),
                    Span::raw(" "),
                    Span::styled(format!("{} ", i + 1), theme.fg(Tok::Muted)),
                    Span::styled(label.clone(), theme.fg(Tok::Text)),
                ];
                // The route picked last time this session, which the chooser opened on.
                if mark_w > 0 {
                    spans.push(Span::styled(mark.clone(), theme.fg(Tok::Accent)));
                }
                let label_w = text::width(&label) + mark_w;
                if inline {
                    spans.push(Span::raw(" ".repeat(label_col.saturating_sub(label_w) + 2)));
                    spans.push(Span::styled(option.detail(), theme.fg(Tok::Muted)));
                }
                spans
            },
        ));
        let selected_option = choice.options.get(choice.selected).copied();
        if !inline && let Some(option) = selected_option {
            rows.push(finish_row(
                vec![Span::styled(
                    format!(
                        "{blank_pointer}   {}",
                        text::ellipsize(option.detail(), w.saturating_sub(prefix), theme.ascii())
                    ),
                    theme.fg(Tok::Muted),
                )],
                w,
            ));
        }
        // A worker starts from this message alone, so show exactly what it will be given.
        if !queue && selected_option == Some(BusyChoice::Independent) {
            let (open, close) = if theme.ascii() {
                ('"', '"')
            } else {
                ('“', '”')
            };
            rows.push(finish_row(
                vec![Span::styled(
                    format!(
                        "{blank_pointer}   {}",
                        text::ellipsize(
                            &format!(
                                "worker gets {open}{}{close}",
                                text::one_line(&self.editor.text)
                            ),
                            w.saturating_sub(prefix),
                            theme.ascii()
                        )
                    ),
                    theme.fg(Tok::Dim),
                )],
                w,
            ));
        }
        if !queue {
            let dash = if theme.ascii() { "-" } else { "–" };
            let mut shortcut = format!("1{dash}{total} pick directly");
            if self.keyboard_enhanced() {
                shortcut.push_str(&format!(" {dot} Ctrl+Enter redirect"));
            }
            shortcut.push_str(&format!(" {dot} /worker <message> starts a worker"));
            // A route chosen every time can be the default: the setting skips the chooser.
            let into = if theme.ascii() { ">" } else { "›" };
            shortcut.push_str(&format!(
                " {dot} default: /settings {into} Enter while working"
            ));
            rows.push(finish_row(
                vec![Span::styled(
                    text::ellipsize(&shortcut, w, theme.ascii()),
                    theme.fg(Tok::Muted),
                )],
                w,
            ));
        }
        if panel {
            rows = super::super::chrome::framed_lines(
                theme,
                super::super::chrome::Section::Dial,
                &title,
                "",
                rows,
                outer,
            );
        }
        rows
    }

    /// Border color by state: shell mode, busy, the optional per-session accent, else the plain border.
    pub(super) fn border_style(&self, ctx: &ComposerCtx) -> Style {
        let theme = ctx.theme;
        let token = if self.mode() == InputMode::Shell {
            Tok::ShellMode
        } else if self.busy {
            Tok::Accent
        } else if ctx.ui.session_accent && !ctx.status.session.is_empty() {
            accent_for(&ctx.status.session)
        } else {
            Tok::Border
        };
        theme.fg(token)
    }
}
