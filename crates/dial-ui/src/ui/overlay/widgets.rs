//! Shared drawing and key-label helpers for the full-screen overlay panels.
use super::panel_keys::{Edge, list_move};
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

/// The character a key types into a text field (see [`crate::ui::keys::typed_char`]).
pub(in crate::ui::overlay) fn typed_char(key: &KeyEvent) -> Option<char> {
    crate::ui::keys::typed_char(key)
}

pub(in crate::ui::overlay) fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(k) if k.eq_ignore_ascii_case(&c))
}

/// Truncate spans to `max` cells, never splitting a grapheme.
pub(in crate::ui::overlay) fn fit(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    text::truncate_spans(spans, max, "")
}

/// A row of exactly `width` cells whose unused part carries `fill` (selection backgrounds).
pub(in crate::ui::overlay) fn filled(
    spans: Vec<Span<'static>>,
    width: usize,
    fill: Style,
) -> Line<'static> {
    let mut spans = fit(spans, width);
    let used = text::spans_width(&spans);
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), fill));
    }
    Line::from(spans).style(fill)
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

/// A single-choice option, as distinct from the action cursor on a menu row.
pub(super) fn radio_row(
    theme: &Theme,
    selected: bool,
    label: &str,
    label_width: usize,
    note: &str,
    width: usize,
) -> Line<'static> {
    let radio = theme.sym(if selected {
        Sym::RadioOn
    } else {
        Sym::RadioOff
    });
    list_row(
        theme,
        vec![
            Span::styled(format!("   {radio} "), theme.fg(Tok::Accent)),
            Span::styled(text::pad(label, label_width), theme.fg(Tok::Text)),
            Span::styled(note.to_owned(), theme.fg(Tok::Muted)),
        ],
        width,
        selected,
    )
}

/// Wrap an empty list's recovery message once, bounded by the available rows.
pub(super) fn empty_rows(
    theme: &Theme,
    message: &str,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    text::wrap_words(message.trim(), width.saturating_sub(2).max(1))
        .into_iter()
        .take(height)
        .map(|s| {
            filled(
                vec![Span::styled(format!(" {s}"), theme.fg(Tok::Muted))],
                width,
                Style::default(),
            )
        })
        .collect()
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

/// Interleave headings with selectable fields and position the viewport. A single-row
/// viewport always shows the field itself, never just its heading.
pub(super) fn section_rows<'a>(
    sections: impl Iterator<Item = &'a str>,
    cursor: usize,
    scroll: &mut usize,
    height: usize,
) -> Vec<(Option<&'a str>, usize)> {
    let mut rows = Vec::new();
    let mut previous = None;
    let mut selected = 0;
    for (i, section) in sections.enumerate() {
        if previous != Some(section) {
            rows.push((Some(section), i));
            previous = Some(section);
        }
        if i == cursor {
            selected = rows.len();
        }
        rows.push((None, i));
    }
    *scroll = chrome::keep_in_view(selected, *scroll, height, rows.len());
    if height > 1 && selected > 0 && *scroll == selected && rows[selected - 1].0.is_some() {
        *scroll -= 1;
    }
    rows
}

/// A section header row of a settings list: a short rule, the name in label style, then the rule
/// to `width`.
pub(crate) fn section_rule(theme: &Theme, name: &str, width: usize) -> Line<'static> {
    let rule = theme.sym(Sym::BoxH);
    let head = format!(" {rule}{rule} ");
    let title = chrome::label(theme, name);
    let pad = width.saturating_sub(text::width(&head) + text::width(&title.content) + 1);
    Line::from(fit(
        vec![
            Span::styled(head, theme.fg(Tok::BorderMuted)),
            title,
            Span::styled(format!(" {}", rule.repeat(pad)), theme.fg(Tok::BorderMuted)),
        ],
        width,
    ))
}

/// The two-column layout used by settings fields and model suggestions.
pub(super) struct SettingColumns {
    width: usize,
    label_w: usize,
}

impl SettingColumns {
    pub(super) fn new<'a>(width: usize, labels: impl Iterator<Item = &'a str>) -> Self {
        let label_w = labels
            .map(text::width)
            .max()
            .unwrap_or(10)
            .min(width.saturating_sub(12).max(8));
        Self { width, label_w }
    }

    pub(super) fn value_width(&self) -> usize {
        // Pointer, marker and gaps before the label (5), column gap (2), right margin (1).
        self.width.saturating_sub(self.label_w + 8)
    }

    pub(super) fn row(
        &self,
        theme: &Theme,
        selected: bool,
        marker: &str,
        label: &str,
        label_style: Style,
        value: impl IntoIterator<Item = Span<'static>>,
    ) -> Line<'static> {
        let label_w = self.label_w;
        let mut spans = vec![
            Span::styled(
                format!(
                    " {} ",
                    if selected {
                        theme.sym(Sym::Pointer)
                    } else {
                        " "
                    }
                ),
                theme.fg(Tok::Accent),
            ),
            Span::styled(marker.to_owned(), theme.fg(Tok::Accent)),
            Span::raw(" "),
            Span::styled(
                format!(
                    "{:<label_w$}",
                    text::ellipsize(label, label_w, theme.ascii())
                ),
                label_style,
            ),
            Span::raw("  "),
        ];
        spans.extend(value);
        if selected {
            chrome::selected_row(theme, spans, self.width)
        } else {
            Line::from(fit(spans, self.width))
        }
    }
}

/// An adjustable setting uses arrows only while selected. Custom values stay bold at rest.
pub(super) fn setting_value(
    theme: &Theme,
    shown: String,
    width: usize,
    show_arrows: bool,
    custom: bool,
) -> Span<'static> {
    let (shown, style) = if show_arrows {
        let (left, right) = if theme.ascii() {
            ("<", ">")
        } else {
            ("‹", "›")
        };
        (
            format!("{left} {shown} {right}"),
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
        )
    } else if custom {
        (shown, theme.fg(Tok::Text).add_modifier(Modifier::BOLD))
    } else {
        (shown, theme.fg(Tok::Muted))
    };
    Span::styled(text::ellipsize(&shown, width, theme.ascii()), style)
}

/// The highlighted action/list cursor (not a radio choice), and the fill for its other cells.
pub(super) fn selection_marker(
    theme: &Theme,
    selected: bool,
    indent: &str,
) -> (Span<'static>, Style) {
    let (glyph, fill) = if selected {
        (theme.sym(Sym::Pointer), theme.bg(Tok::SelectedBg))
    } else {
        (" ", Style::default())
    };
    (
        Span::styled(
            format!("{indent}{glyph} "),
            theme.fg(Tok::Accent).patch(fill),
        ),
        fill,
    )
}

/// The help panel under a settings list, exactly `rows` lines: the field's title, its wrapped
/// `help` (an ellipsis marks a cut), then `footer` (range, choices or the reason it is blocked) on
/// the last row.
pub(crate) fn field_detail(
    theme: &Theme,
    label: &str,
    help: &str,
    footer: Line<'static>,
    rows: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(fit(
        vec![chrome::label(theme, &format!(" {label}"))],
        width,
    ))];
    let help_rows = rows.saturating_sub(2);
    let wrapped = text::wrap_words(help, width.saturating_sub(2).max(8));
    for (index, row) in wrapped.iter().take(help_rows).enumerate() {
        let mut shown = format!(" {row}");
        if index + 1 == help_rows && wrapped.len() > help_rows {
            shown.push_str(&format!(" {}", theme.sym(Sym::Ellipsis)));
        }
        lines.push(Line::from(fit(
            vec![Span::styled(
                text::ellipsize(&shown, width, theme.ascii()),
                theme.fg(Tok::Muted),
            )],
            width,
        )));
    }
    lines.push(footer);
    lines.truncate(rows);
    while lines.len() < rows {
        lines.push(Line::default());
    }
    lines
}

/// The footer of a settings form. While a value is typed it shows the save and cancel keys;
/// otherwise the row keys, the form's own `extra` keys, then the way out. `scope` is the scope a
/// save would leave (`s` switches to the other one).
pub(crate) fn form_footer(
    theme: &Theme,
    keys: &Keymap,
    editing: bool,
    width: usize,
    scope: Scope,
    extra: &[String],
) -> Line<'static> {
    let segments: Vec<String> = if editing {
        vec![
            hint(keys, Action::SelectConfirm, "save"),
            hint(keys, Action::SelectCancel, "cancel"),
        ]
    } else {
        let mut segments = vec![
            format!(
                "{} move",
                pair_label(keys, Action::SelectUp, Action::SelectDown)
            ),
            format!("{} change", arrows(theme, Axis::Horizontal)),
            hint(keys, Action::SelectConfirm, "edit"),
            "r reset".to_owned(),
            format!("s save to {}", scope_name(other_scope(scope))),
        ];
        segments.extend(extra.iter().cloned());
        segments.push(hint(keys, Action::SelectCancel, "close"));
        segments
    };
    hint_line(theme, &segments, width)
}

/// The reply `form_key` gives a key that a settings form's shared layer routed.
#[derive(Debug, PartialEq)]
pub(crate) enum FormKey {
    /// Not a shared key: the form's own bindings handle it.
    Ignored,
    /// The open value editor took the key; `true` when its text changed.
    Typed(bool),
    /// Confirm in the open value editor: save this text.
    Commit(String),
    /// Cancel closed the open value editor.
    Discarded,
    /// Cancel with no editor open: leave the form.
    Close,
    /// The cursor moved (list keys, Home or End).
    Moved,
    /// Confirm on a row with no editor open: the form decides what that row does.
    Confirm,
    /// A key release: forms act on presses only, so the release is consumed and does nothing.
    Released,
}

/// Routes one key for a settings form's rows and its open value editor, through the user's keymap:
/// cancel discards the editor or closes the form, confirm commits the editor or activates the row,
/// and list movement moves `cursor` through `len` rows, a page at a time for `page`. Everything else
/// is the form's own.
pub(crate) fn form_key(
    editor: &mut Option<Editor>,
    key: &KeyEvent,
    keys: &Keymap,
    cursor: &mut usize,
    len: usize,
    page: usize,
) -> FormKey {
    if !pressed(key) {
        return FormKey::Released;
    }
    if keys.matches(Action::SelectCancel, key) {
        return if editor.take().is_some() {
            FormKey::Discarded
        } else {
            FormKey::Close
        };
    }
    if let Some(open) = editor {
        if keys.matches(Action::SelectConfirm, key) {
            return FormKey::Commit(open.text.clone());
        }
        return FormKey::Typed(open.key_with(*key, keys));
    }
    if len == 0 {
        return FormKey::Ignored;
    }
    if let Some(next) = list_move(keys, key, *cursor, len, page, Edge::Stop) {
        *cursor = next;
        return FormKey::Moved;
    }
    if keys.matches(Action::SelectConfirm, key) {
        return FormKey::Confirm;
    }
    match key.code {
        KeyCode::Home => {
            *cursor = 0;
            FormKey::Moved
        }
        KeyCode::End => {
            *cursor = len - 1;
            FormKey::Moved
        }
        _ => FormKey::Ignored,
    }
}

/// The status row under a settings form: its last message (red for an error, green for a notice),
/// or blank.
pub(crate) fn form_status(
    theme: &Theme,
    message: Option<&(String, bool)>,
    width: usize,
) -> Line<'static> {
    let Some((message, error)) = message else {
        return Line::default();
    };
    let (sym, tok) = if *error {
        (Sym::Cross2, Tok::Error)
    } else {
        (Sym::Check, Tok::Success)
    };
    Line::from(fit(
        vec![Span::styled(
            format!(" {} {message}", theme.sym(sym)),
            theme.fg(tok),
        )],
        width,
    ))
}

/// Pasted text goes into the open value editor, if one is open.
pub(crate) fn form_paste(editor: &mut Option<Editor>, pasted: &str) {
    if let Some(open) = editor {
        open.insert(pasted);
    }
}

/// The message for a reset on a row that already uses its default.
pub(crate) const ALREADY_DEFAULT: &str = "Already using the default";

/// Delete, Backspace or `r`: reset the selected row to its default.
pub(crate) fn is_reset_key(key: &KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('r')
    )
}

/// `s` (or Ctrl+S): switch the scope saves go to, see [`scope_switched`].
pub(crate) fn is_scope_key(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('s') => true,
        KeyCode::Char('S') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

/// Switches the scope a form saves to and returns the notice that says so.
pub(crate) fn scope_switched(scope: &mut Scope) -> String {
    *scope = other_scope(*scope);
    format!("Changes now save to {} settings", scope_name(*scope))
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
pub(crate) fn pair_label(keys: &Keymap, a: Action, b: Action) -> String {
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
    super::panel_keys::segment(&keys.label(action), what)
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
    let destination = match scope {
        Scope::Global => "All projects".to_owned(),
        Scope::Project => format!("This project ({project})"),
    };
    let caption = format!("Save to: {destination}");
    let alternative = match scope {
        Scope::Global => format!("This project ({project})"),
        Scope::Project => "All projects".to_owned(),
    };
    let full = format!("{caption} · Ctrl+B: {alternative}");
    let compact = format!("{caption} · Ctrl+B");
    if text::width(&full) <= max_cells {
        full
    } else if text::width(&compact) <= max_cells {
        compact
    } else {
        text::clip(&caption, max_cells)
    }
}

/// A search label and editable query. Give the caret room before clipping the placeholder.
pub(super) fn search_field(
    theme: &Theme,
    query: &str,
    cursor: usize,
    placeholder: &str,
    width: usize,
) -> Vec<Span<'static>> {
    let label = " Search: ";
    let mut spans = fit(
        vec![Span::styled(label, theme.fg(Tok::Muted))],
        width.saturating_sub(1),
    );
    let room = width.saturating_sub(text::spans_width(&spans));
    spans.extend(cursor_spans(theme, query, cursor, room, false));
    if query.is_empty() {
        spans.push(Span::styled(placeholder.to_owned(), theme.fg(Tok::Dim)));
    }
    fit(spans, width)
}

/// Cursor-aware rendering of a single-line editor inside `width` cells (the tail is kept visible).
pub(in crate::ui::overlay) fn editor_spans(
    theme: &Theme,
    editor: &Editor,
    width: usize,
    mask: bool,
) -> Vec<Span<'static>> {
    cursor_spans(theme, &editor.text, editor.cursor, width, mask)
}

fn cursor_spans(
    theme: &Theme,
    value: &str,
    cursor: usize,
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
    let (before, rest) = value.split_at(cursor.min(value.len()));
    let before = shown(before);
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
pub(crate) fn tab_strip(
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
    // A left overflow marker must not displace the active label on tiny panels.
    if w < 2 + mark_w + text::width(names[active]) {
        return Line::from(fit(
            vec![Span::styled(
                names[active].to_owned(),
                chrome::tab_style(theme, highlight),
            )],
            w,
        ));
    }
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
        // Spelled out: a bare `…2` reads like the override count other tabs carry.
        let more_label = format!("+{hidden} more");
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
            chrome::tab_style(theme, highlight && i == active),
        ));
        used += need;
    }
    Line::from(fit(spans, w))
}

/// Compute and Improve retain their bracketed active label in ASCII mode.
pub(super) fn panel_tabs(
    theme: &Theme,
    names: &[&str],
    active: usize,
    width: usize,
) -> Line<'static> {
    if theme.ascii() && active < names.len() {
        let bracketed = format!("[{}]", names[active]);
        let mut labels = names.to_vec();
        labels[active] = &bracketed;
        tab_strip(theme, &labels, active, true, width)
    } else {
        tab_strip(theme, names, active, true, width)
    }
}

/// The pointer and live dot that open a roster row (jobs, peers). A live row shows the running
/// state's glyph and tone ([`chrome::StateMark`]); an idle one the plain dot.
pub(in crate::ui::overlay) fn roster_lead(
    theme: &Theme,
    selected: bool,
    live: bool,
) -> [Span<'static>; 2] {
    let dot = if live {
        chrome::StateMark::Running.glyph(theme)
    } else {
        theme.sym(Sym::Dot)
    };
    let pointer = if selected {
        theme.sym(Sym::Pointer)
    } else {
        " "
    };
    [
        Span::styled(format!(" {pointer} "), theme.fg(Tok::Accent)),
        Span::styled(format!("{dot} "), theme.fg(live_tok(live))),
    ]
}

/// The tone of a live row: the running state's color, muted when idle.
pub(in crate::ui::overlay) fn live_tok(live: bool) -> Tok {
    if live {
        chrome::StateMark::Running.tok()
    } else {
        Tok::Muted
    }
}

/// A roster column `n` cells wide: clipped with an ellipsis to leave one cell of gap, then padded.
pub(in crate::ui::overlay) fn roster_cell(value: &str, n: usize, ascii: bool) -> String {
    text::pad(&text::ellipsize(value, n.saturating_sub(1), ascii), n)
}

#[cfg(test)]
mod tests;
