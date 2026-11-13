//! Shared drawing and key-label helpers for the settings panel and the panels built on it.
use crate::config::settings_store::Scope;
use crate::ui::chrome::{self, Section};
use crate::ui::editor::Editor;
use crate::ui::keys::{Action, Keymap};
use crate::ui::text;
use crate::ui::theme::{Sym, Theme, Tok};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;

// ---------------------------------------------------------------------------------------------
// Shared helpers (also used by `models` and `providers`)
// ---------------------------------------------------------------------------------------------

pub(in crate::ui::overlay) fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Global => "global",
        Scope::Project => "project",
    }
}

pub(in crate::ui::overlay) fn other_scope(scope: Scope) -> Scope {
    match scope {
        Scope::Global => Scope::Project,
        Scope::Project => Scope::Global,
    }
}

pub(in crate::ui::overlay) fn pressed(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
}

/// The character a key types into a text field (no Ctrl/Alt/Super chords, no controls).
pub(in crate::ui::overlay) fn typed_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                && !c.is_control() =>
        {
            Some(c)
        }
        _ => None,
    }
}

pub(in crate::ui::overlay) fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(k) if k.eq_ignore_ascii_case(&c))
}

pub(super) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| text::width(&s.content)).sum()
}

/// Truncate spans to `max` cells, never splitting a grapheme.
pub(in crate::ui::overlay) fn fit(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut left = max;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        if left == 0 {
            break;
        }
        let clipped = text::clip(&span.content, left);
        let cells = text::width(&clipped);
        if cells == 0 {
            continue;
        }
        left -= cells;
        out.push(Span::styled(clipped, span.style));
    }
    out
}

/// A row of exactly `width` cells whose unused part carries `fill` (selection backgrounds).
pub(in crate::ui::overlay) fn filled(
    spans: Vec<Span<'static>>,
    width: usize,
    fill: Style,
) -> Line<'static> {
    let mut spans = fit(spans, width);
    let used = spans_width(&spans);
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), fill));
    }
    Line::from(spans).style(fill)
}

/// Spans for `text` with the characters at `positions` (char indexes) emphasized.
pub(in crate::ui::overlay) fn highlighted(
    text: &str,
    positions: &[usize],
    base: Style,
    hl: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut run_hl = false;
    for (i, c) in text.chars().enumerate() {
        let h = positions.contains(&i);
        if h != run_hl && !run.is_empty() {
            out.push(Span::styled(
                std::mem::take(&mut run),
                if run_hl { hl } else { base },
            ));
        }
        run_hl = h;
        run.push(c);
    }
    if !run.is_empty() {
        out.push(Span::styled(run, if run_hl { hl } else { base }));
    }
    out
}

pub(in crate::ui::overlay) fn match_style(theme: &Theme, base: Style) -> Style {
    base.patch(theme.fg(Tok::Accent))
        .add_modifier(Modifier::BOLD)
}

/// A list row of exactly `width` cells: the one shared selected-row look
/// ([`chrome::selected_row`]) when `selected`, otherwise the spans as they are.
pub(in crate::ui::overlay) fn list_row(
    theme: &Theme,
    spans: Vec<Span<'static>>,
    width: usize,
    selected: bool,
) -> Line<'static> {
    if selected {
        chrome::selected_row(theme, spans, width)
    } else {
        filled(spans, width, Style::default())
    }
}

/// Style of a tab-bar cell; shared with every panel through [`chrome::tab_style`].
pub(in crate::ui::overlay) fn tab_style(theme: &Theme, active: bool) -> Style {
    chrome::tab_style(theme, active)
}

/// One scrollbar cell for list row `row` of `rows` visible, `total` items, first visible `offset`.
pub(crate) fn scroll_cell(
    theme: &Theme,
    row: usize,
    rows: usize,
    total: usize,
    offset: usize,
) -> Span<'static> {
    if total <= rows || rows == 0 {
        return Span::raw(" ");
    }
    let thumb = (rows * rows / total).max(1).min(rows);
    let start = if total > rows {
        offset * (rows - thumb) / (total - rows)
    } else {
        0
    };
    if row >= start && row < start + thumb {
        Span::styled(
            theme.sym(Sym::ScrollThumb).to_owned(),
            theme.fg(Tok::Accent),
        )
    } else {
        Span::styled(theme.sym(Sym::ScrollTrack).to_owned(), theme.fg(Tok::Dim))
    }
}

/// Footer hints from `"key label"` segments (the first word is the key), drawn by
/// [`chrome::hint_line`] one cell in from the edge: the last segment (the way out) always stays and
/// the ones before it are dropped whole when they do not fit.
pub(in crate::ui::overlay) fn hint_line(
    theme: &Theme,
    segments: &[String],
    width: usize,
) -> Line<'static> {
    hint_line_after(theme, "", segments, width)
}

/// [`hint_line`] after a muted `lead` (a state such as `read-only` or a list position). The lead
/// is clipped before the exit hint and is never drawn as a key.
pub(in crate::ui::overlay) fn hint_line_after(
    theme: &Theme,
    lead: &str,
    segments: &[String],
    width: usize,
) -> Line<'static> {
    let pairs: Vec<(&str, &str)> = segments
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.split_once(' ').unwrap_or((s, "")))
        .collect();
    let mut spans = vec![Span::raw(" ")];
    spans.extend(chrome::hint_line_after(theme, lead, &pairs, width.saturating_sub(1)).spans);
    Line::from(fit(spans, width))
}

/// `Key` labels for "a/b" style hints; empty when the action is unbound.
pub(in crate::ui::overlay) fn pair_label(keys: &Keymap, a: Action, b: Action) -> String {
    match (keys.label(a), keys.label(b)) {
        (x, y) if x.is_empty() => y,
        (x, y) if y.is_empty() => x,
        (x, y) => format!("{x}/{y}"),
    }
}

/// Which arrow pair a hint shows; ASCII themes get `<-/->` and `^v` instead of the Unicode glyphs.
#[derive(Clone, Copy)]
pub(in crate::ui::overlay) enum Axis {
    Horizontal,
    Vertical,
}

/// A panel's own function key `F<number>`: plain while that chord does not trigger the global
/// `global` action (the user rebound it), Shift+F<number> always. Function keys are global
/// fallbacks for chords some terminals cannot send, so a panel never gives one a second meaning.
pub(in crate::ui::overlay) fn local_function_key(
    key: &KeyEvent,
    keys: &Keymap,
    number: u8,
    global: Action,
) -> bool {
    key.kind != KeyEventKind::Release
        && key.code == KeyCode::F(number)
        && (key.modifiers == KeyModifiers::SHIFT
            || (key.modifiers.is_empty() && !keys.triggers(global, key)))
}

/// The label of [`local_function_key`] for hints: `F<number>` when it is free, else `Shift+F<number>`.
pub(in crate::ui::overlay) fn local_function_label(
    keys: &Keymap,
    number: u8,
    global: Action,
) -> String {
    let plain = KeyEvent::new(KeyCode::F(number), KeyModifiers::NONE);
    if keys.triggers(global, &plain) {
        format!("Shift+F{number}")
    } else {
        format!("F{number}")
    }
}

/// The arrow pair for a hint, following the theme's ASCII mode.
pub(in crate::ui::overlay) fn arrows(theme: &Theme, axis: Axis) -> String {
    match axis {
        Axis::Horizontal if theme.ascii() => "<-/->".to_owned(),
        Axis::Horizontal => "←/→".to_owned(),
        Axis::Vertical => format!("{}{}", theme.sym(Sym::ArrowUp), theme.sym(Sym::ArrowDown)),
    }
}

/// `"Esc close"`; empty when the action is unbound.
pub(in crate::ui::overlay) fn hint(keys: &Keymap, action: Action, what: &str) -> String {
    let label = keys.label(action);
    if label.is_empty() {
        String::new()
    } else {
        format!("{label} {what}")
    }
}

/// Draw the full-screen panel: a rounded box whose top border carries the `◆ <name>` title and
/// `extra` at its right edge. `body` gets the inner size and returns at most that many rows. On a
/// terminal too small for the box the body fills the screen.
pub(in crate::ui::overlay) fn panel(
    frame: &mut Frame,
    theme: &Theme,
    section: Section,
    name: &str,
    extra: &str,
    body: impl FnOnce(u16, u16) -> Vec<Line<'static>>,
) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width < 5 || area.height < 5 {
        let mut lines = body(area.width, area.height);
        lines.truncate(usize::from(area.height));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    let block = chrome::panel_block_titled(theme, section, name, extra);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = body(inner.width, inner.height);
    lines.truncate(usize::from(inner.height));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Keep the editing scope visible without allowing a right-aligned caption to overwrite its title.
pub(in crate::ui::overlay) fn scope_caption(
    scope: Scope,
    project: &str,
    max_cells: usize,
) -> String {
    let full = format!(
        "scope: {}{} · {} · Ctrl+S switch",
        scope_name(scope),
        if scope == Scope::Project {
            format!(" ({project})")
        } else {
            String::new()
        },
        if scope == Scope::Global {
            format!("project ({project})")
        } else {
            "global".into()
        }
    );
    if crate::ui::text::width(&full) <= max_cells {
        full
    } else {
        crate::ui::text::clip(
            &format!("scope: {} ({project})", scope_name(scope)),
            max_cells,
        )
    }
}

/// Cursor-aware rendering of a single-line editor inside `width` cells (the tail is kept visible).
pub(in crate::ui::overlay) fn editor_spans(
    theme: &Theme,
    editor: &Editor,
    width: usize,
    mask: bool,
) -> Vec<Span<'static>> {
    let shown = |s: &str| {
        if mask {
            "•".repeat(s.graphemes(true).count())
        } else {
            s.to_owned()
        }
    };
    let before = shown(&editor.text[..editor.cursor.min(editor.text.len())]);
    let rest = &editor.text[editor.cursor.min(editor.text.len())..];
    let (at, after) = match rest.graphemes(true).next() {
        Some(g) => (shown(g), shown(&rest[g.len()..])),
        None => (" ".to_owned(), String::new()),
    };
    let at_w = text::width(&at).max(1);
    let budget = width.saturating_sub(at_w);
    // Keep the tail of `before`, then as much of `after` as still fits.
    let mut tail: Vec<&str> = Vec::new();
    let mut used = 0;
    for g in before.graphemes(true).rev() {
        let gw = text::width(g);
        if used + gw > budget {
            break;
        }
        used += gw;
        tail.push(g);
    }
    tail.reverse();
    let before: String = tail.concat();
    let after = text::clip(&after, budget.saturating_sub(used));
    let base = theme.fg(Tok::Text);
    fit(
        vec![
            Span::styled(before, base),
            Span::styled(at, base.add_modifier(Modifier::REVERSED)),
            Span::styled(after, base),
        ],
        width,
    )
}

/// A one-row tab strip of plain names two cells apart. It windows around `active` (drawn in the
/// active style when `highlight`), and an ellipsis on either side says more tabs are there
/// instead of clipping a name mid-word.
pub(in crate::ui::overlay) fn tab_strip(
    theme: &Theme,
    names: &[&str],
    active: usize,
    highlight: bool,
    w: usize,
) -> Line<'static> {
    if names.is_empty() {
        return Line::default();
    }
    let gap = 2;
    let mark = theme.sym(Sym::Ellipsis);
    let mark_w = text::width(mark);
    let active = active.min(names.len() - 1);
    let span_w = |from: usize, to: usize| {
        names[from..=to]
            .iter()
            .map(|n| text::width(n))
            .sum::<usize>()
            + gap * (to - from)
    };
    let needs = |start: usize| {
        1 + if start > 0 { mark_w + 1 } else { 0 }
            + span_w(start, active)
            + if active + 1 < names.len() {
                gap + mark_w + 2
            } else {
                0
            }
    };
    let mut start = 0;
    while start < active && needs(start) > w {
        start += 1;
    }
    let more = theme.fg(Tok::Dim);
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    if start > 0 {
        spans.push(Span::styled(format!("{mark} "), more));
        used += mark_w + 1;
    }
    for (i, name) in names.iter().enumerate().skip(start) {
        let sep = if i > start { gap } else { 0 };
        let hidden = names.len() - i;
        let more_label = format!("{mark}{hidden}");
        let reserve = if i + 1 < names.len() {
            gap + text::width(&more_label)
        } else {
            0
        };
        let need = sep + text::width(name);
        if i > active && used + need + reserve > w {
            // Say how many tabs are off the edge, not just that some are.
            spans.push(Span::styled(
                format!("{}{more_label}", " ".repeat(sep)),
                more,
            ));
            break;
        }
        spans.push(Span::raw(" ".repeat(sep)));
        spans.push(Span::styled(
            (*name).to_owned(),
            tab_style(theme, highlight && i == active),
        ));
        used += need;
    }
    Line::from(fit(spans, w))
}
