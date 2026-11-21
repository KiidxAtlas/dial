//! Shared panel navigation and contextual help; text editors retain punctuation input.
//!
//! [`PanelKeys`] owns the `?`/F1 key sheet every panel offers, and [`sheet`] draws any panel's key
//! reference in the one look (the git panel's richer sheet uses it too).
use super::DrawCtx;
use super::widgets::{pair_label, panel};
use crate::ui::{
    chrome::{self, Section},
    keys::{Action, Keymap},
    text,
    theme::{Theme, Tok},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    style::Modifier,
    text::{Line, Span},
};

#[derive(Default)]
pub struct PanelKeys {
    help: bool,
    scroll: std::cell::Cell<usize>,
    page: std::cell::Cell<usize>,
    total: std::cell::Cell<usize>,
    /// The panel's full footer while its key sheet is open (see [`Self::set_footer`]).
    footer: Vec<String>,
}

/// Fixed panel keys remain available alongside customized legacy bindings.
pub fn matches(keys: &Keymap, action: Action, key: &KeyEvent) -> bool {
    keys.matches(action, key)
        || match action {
            Action::SelectCancel => key.code == KeyCode::Esc,
            Action::SelectConfirm => key.code == KeyCode::Enter,
            Action::TabNext => key.code == KeyCode::Tab,
            Action::TabPrev => key.code == KeyCode::BackTab,
            _ => false,
        }
}

/// One group of a key sheet: a title and its `(keys, what they do)` rows.
pub struct SheetGroup<'a> {
    pub title: &'a str,
    pub rows: Vec<(String, String)>,
}

/// A key reference body `width` cells wide: each group under an uppercase label, its keys in one
/// bold accent column and their meaning beside them, then `notes` (one paragraph per line) wrapped
/// in the secondary color, and the way back as the last row.
pub fn sheet(
    theme: &Theme,
    groups: &[SheetGroup<'_>],
    notes: &str,
    back: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let key_w = groups
        .iter()
        .flat_map(|group| &group.rows)
        .map(|(keys, _)| text::width(keys))
        .max()
        .unwrap_or(0);
    let mut out = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            out.push(Line::default());
        }
        out.push(Line::from(vec![
            Span::raw(" "),
            chrome::label(theme, group.title),
        ]));
        for (keys, label) in &group.rows {
            let indent = 3.min(width.saturating_sub(1));
            if key_w + 5 < width {
                let prefix = format!("{}{}  ", " ".repeat(indent), text::pad(keys, key_w));
                let label_rows =
                    text::wrap_words(label, width.saturating_sub(text::width(&prefix)).max(1));
                for (i, row) in label_rows.into_iter().enumerate() {
                    out.push(Line::from(vec![
                        Span::styled(
                            if i == 0 {
                                prefix.clone()
                            } else {
                                " ".repeat(text::width(&prefix))
                            },
                            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(row, theme.fg(Tok::Text)),
                    ]));
                }
            } else {
                for row in text::wrap_words(keys, width.saturating_sub(indent).max(1)) {
                    out.push(Line::from(Span::styled(
                        format!("{}{row}", " ".repeat(indent)),
                        theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                    )));
                }
                for row in text::wrap_words(label, width.saturating_sub(1).max(1)) {
                    out.push(Line::from(Span::styled(
                        format!(" {row}"),
                        theme.fg(Tok::Text),
                    )));
                }
            }
        }
    }
    let notes: Vec<&str> = notes.lines().filter(|l| !l.trim().is_empty()).collect();
    if !notes.is_empty() {
        out.push(Line::default());
        for note in notes {
            for row in text::wrap_words(note.trim(), width.saturating_sub(2).max(1)) {
                out.push(Line::from(Span::styled(
                    format!(" {row}"),
                    theme.fg(Tok::Muted),
                )));
            }
        }
    }
    out.push(Line::default());
    let mut footer = vec![Span::raw(" ")];
    footer.extend(
        chrome::hint_line(theme, &[(back, "back to panel")], width.saturating_sub(1)).spans,
    );
    out.push(Line::from(footer));
    text::wrap(out, width.max(1))
}

/// Most hints a panel footer shows; the rest live in the key sheet.
pub const FOOTER_HINTS: usize = 4;

/// A footer of at most [`FOOTER_HINTS`] `"key label"` hints: the primary action, the most useful
/// finding hint (search, then filter, then preview; else the next hint), `sheet` (how to open the
/// key sheet), and the way out. The full list stays reachable through [`PanelKeys::set_footer`].
pub fn capped_footer(hints: Vec<String>, sheet: &str) -> Vec<String> {
    let hints: Vec<String> = hints.into_iter().filter(|h| !h.is_empty()).collect();
    if hints.len() <= FOOTER_HINTS {
        return hints;
    }
    let middle = &hints[1..hints.len() - 1];
    let labelled = |word: &str| {
        middle.iter().find(|h| {
            h.split_once(' ')
                .is_some_and(|(_, label)| label.contains(word))
        })
    };
    let second = labelled("search")
        .or_else(|| labelled("filter"))
        .or_else(|| labelled("preview"))
        .unwrap_or(&middle[0]);
    vec![
        hints[0].clone(),
        second.clone(),
        sheet.to_owned(),
        hints[hints.len() - 1].clone(),
    ]
}

/// One `key label` segment for a footer or key sheet. Omit unbound keys.
pub(crate) fn segment(key: &str, label: &str) -> String {
    match (key.is_empty(), label.is_empty()) {
        (true, _) => String::new(),
        (false, true) => key.to_owned(),
        (false, false) => format!("{key} {label}"),
    }
}

/// Summarize a panel's bindings; F1 exposes the complete list. Surfaces that display
/// every applicable action use `widgets::hint_line` directly.
pub(crate) fn sheet_footer(theme: &Theme, segments: Vec<String>, width: usize) -> Line<'static> {
    super::widgets::hint_line(theme, &capped_footer(segments, "F1 keys"), width)
}

/// The row a list cursor moves to for the shared Up/Down/PageUp/PageDown keys over `len` rows;
/// `None` when the key is not list movement. `Edge` decides what Up and Down do at either end.
/// `page` is the paging step as the list measured it: zero pages nothing, so a list with no room
/// keeps its existing paging. Home and End stay with each list, since not every list takes them.
pub(crate) fn list_move(
    keys: &Keymap,
    key: &KeyEvent,
    cursor: usize,
    len: usize,
    page: usize,
    edge: Edge,
) -> Option<usize> {
    let last = len.saturating_sub(1);
    if keys.matches(Action::SelectUp, key) {
        Some(match (edge, cursor) {
            (Edge::Wrap, 0) => last,
            _ => cursor.saturating_sub(1),
        })
    } else if keys.matches(Action::SelectDown, key) {
        Some(match edge {
            Edge::Wrap if cursor >= last => 0,
            _ => cursor.saturating_add(1).min(last),
        })
    } else if keys.matches(Action::SelectPageUp, key) {
        Some(cursor.saturating_sub(page))
    } else if keys.matches(Action::SelectPageDown, key) {
        Some(cursor.saturating_add(page).min(last))
    } else {
        None
    }
}

/// Scroll a text pane before its next draw measures and clamps the offset.
pub(crate) fn scroll_text(
    keys: &Keymap,
    key: &KeyEvent,
    offset: usize,
    page: usize,
) -> Option<usize> {
    match key.code {
        KeyCode::Home => Some(0),
        KeyCode::End => Some(usize::MAX),
        _ => list_move(keys, key, offset, usize::MAX, page, Edge::Stop),
    }
}

/// Shared viewport for a destructive review. The caller owns the consequences and
/// which keys confirm; this only scrolls the copy and gates confirmation on space.
#[derive(Default)]
pub(super) struct RemovalReview {
    offset: usize,
    page: usize,
    too_small: bool,
}

impl RemovalReview {
    pub(super) fn rewind(&mut self) {
        self.offset = 0;
    }

    pub(super) fn can_confirm(&self) -> bool {
        !self.too_small
    }

    pub(super) fn scroll(&mut self, keys: &Keymap, key: &KeyEvent) {
        if let Some(next) = scroll_text(keys, key, self.offset, self.page) {
            self.offset = next;
        }
    }

    pub(super) fn rows(
        &mut self,
        body: Vec<Line<'static>>,
        width: usize,
        height: usize,
        footer: Line<'static>,
        resize_hint: &'static str,
    ) -> Vec<Line<'static>> {
        self.too_small = width < 20 || height < 3;
        let room = height.saturating_sub(1);
        self.page = room.max(1);
        self.offset = self.offset.min(body.len().saturating_sub(room));
        let mut out: Vec<_> = body.into_iter().skip(self.offset).take(room).collect();
        out.resize(room, Line::default());
        if height > 0 {
            out.push(if self.too_small {
                Line::from(resize_hint)
            } else {
                footer
            });
        }
        out
    }
}

/// What Up and Down do at the first and last row of a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edge {
    /// Stop at the ends.
    Stop,
    /// Continue from the other end.
    Wrap,
}

/// The first key bound to `action` as its footer and sheet label, or `fallback` when nothing is
/// bound; the keymap decides the spelling, so remapped bindings show as the user set them.
pub(crate) fn label_or(keys: &Keymap, action: Action, fallback: &'static str) -> String {
    match keys.label(action) {
        label if label.is_empty() => fallback.to_owned(),
        label => label,
    }
}

impl PanelKeys {
    /// Returns true when the help layer consumed the event. F1 always toggles the key sheet; `?`
    /// does too unless a text field has focus (then it is typed). Every other key, `/` included,
    /// belongs to the panel.
    pub fn handle_key(&mut self, event: &Event, text_focused: bool) -> bool {
        let Event::Key(key) = event else {
            return self.help;
        };
        if key.kind == KeyEventKind::Release {
            return self.help;
        }
        if self.help {
            let max = self.total.get().saturating_sub(self.page.get());
            let step = self.page.get().max(1);
            match key.code {
                KeyCode::Down => self
                    .scroll
                    .set(self.scroll.get().saturating_add(1).min(max)),
                KeyCode::Up => self.scroll.set(self.scroll.get().saturating_sub(1)),
                KeyCode::PageDown => self
                    .scroll
                    .set(self.scroll.get().saturating_add(step).min(max)),
                KeyCode::PageUp => self.scroll.set(self.scroll.get().saturating_sub(step)),
                KeyCode::Home => self.scroll.set(0),
                KeyCode::End => self.scroll.set(max),
                _ => {}
            }
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::Char('?')
            ) {
                self.help = false;
            }
            return true;
        }
        if key.code == KeyCode::F(1) || (key.code == KeyCode::Char('?') && !text_focused) {
            self.scroll.set(0);
            self.help = true;
            return true;
        }
        false
    }

    /// Bound a rendered key sheet to the available rows, keeping scroll hints pinned.
    pub(crate) fn viewport(
        &self,
        mut lines: Vec<Line<'static>>,
        w: usize,
        h: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        if h == 0 {
            return Vec::new();
        }
        // The way back is chrome, not part of the scrollable reference.
        lines.pop();
        let page = h.saturating_sub(1);
        self.page.set(page);
        self.total.set(lines.len());
        let offset = self.scroll.get().min(lines.len().saturating_sub(page));
        self.scroll.set(offset);
        let mut out: Vec<_> = lines.into_iter().skip(offset).take(page).collect();
        out.resize(page, Line::default());
        out.push(chrome::hint_line(
            theme,
            &[("↑↓/PgUp/PgDn", "scroll"), ("Esc", "back")],
            w,
        ));
        out
    }

    /// Whether the key sheet is showing.
    pub fn open(&self) -> bool {
        self.help
    }

    /// Before drawing a picker body, handle empty/tiny terminals or its open help sheet.
    /// Screen-specific bindings are built only when the sheet is visible.
    pub(crate) fn picker_sheet(
        &self,
        theme: &Theme,
        keys: &Keymap,
        size: (usize, usize),
        title: &str,
        resize_hint: &str,
        actions: impl FnOnce() -> Vec<(String, String)>,
    ) -> Option<Vec<Line<'static>>> {
        let (cols, height) = size;
        if height == 0 || cols == 0 {
            return Some(Vec::new());
        }
        if height < 5 || cols < 12 {
            return Some(vec![Line::from(Span::styled(
                text::ellipsize(resize_hint, cols, theme.ascii()),
                theme.fg(Tok::Muted),
            ))]);
        }
        if !self.open() {
            return None;
        }
        let mut rows = Self::movement_rows(keys);
        rows.extend(actions());
        rows.extend([
            ("Ctrl+U".into(), "clear the search".into()),
            ("F1".into(), "show or hide these keys".into()),
            ("Esc".into(), "clear the search, then close".into()),
        ]);
        let inner = cols.saturating_sub(4);
        let body = sheet(
            theme,
            &[SheetGroup {
                title: "Keys",
                rows,
            }],
            "",
            "F1/Esc",
            inner,
        );
        let body = self.viewport(body, inner, height.saturating_sub(2), theme);
        Some(chrome::framed_lines(
            theme,
            Section::Dial,
            &format!("{title} · keys"),
            "",
            body,
            cols,
        ))
    }

    fn movement_rows(keys: &Keymap) -> Vec<(String, String)> {
        vec![
            (
                pair_label(keys, Action::SelectUp, Action::SelectDown),
                "move the selection".to_owned(),
            ),
            (
                pair_label(keys, Action::SelectPageUp, Action::SelectPageDown),
                "move a page".to_owned(),
            ),
        ]
    }

    /// The common rows of the key sheet: list movement, the main action, tabs when the panel has
    /// them, `/` when it opens a search, this sheet and the way back.
    fn rows(keys: &Keymap, tabs: bool, search: bool) -> Vec<(String, String)> {
        let mut rows = Self::movement_rows(keys);
        rows.push((
            keys.label(Action::SelectConfirm),
            "open or apply the selected row".to_owned(),
        ));
        if tabs {
            rows.push(("Tab/Shift+Tab".into(), "switch tabs".into()));
        }
        if search {
            rows.push(("/".into(), "search (Enter keeps the results)".into()));
        }
        rows.push(("?/F1".into(), "show or hide these keys".into()));
        rows.push((
            keys.label(Action::SelectCancel),
            "back or close (clears a search first)".into(),
        ));
        rows.retain(|(keys, _)| !keys.is_empty());
        rows
    }

    /// The panel's full footer (`"key label"` hints), listed first under "This panel" in the key
    /// sheet so a footer trimmed by [`capped_footer`] loses no key.
    pub fn set_footer(&mut self, footer: Vec<String>) {
        self.footer = footer;
    }

    /// Render the key sheet instead of the underlying panel while help is open: the panel's own
    /// footer keys, the shared rows, then the panel's `extra` notes (one paragraph per line).
    pub fn draw(
        &self,
        frame: &mut Frame,
        ctx: &DrawCtx,
        title: &str,
        extra: &str,
        tabs: bool,
        search: bool,
    ) -> bool {
        if !self.help {
            return false;
        }
        let local: Vec<(String, String)> = self
            .footer
            .iter()
            .filter(|h| !h.is_empty())
            .map(|h| match h.split_once(' ') {
                Some((key, label)) => (key.to_owned(), label.to_owned()),
                None => (h.clone(), String::new()),
            })
            .collect();
        let mut groups = Vec::new();
        if !local.is_empty() {
            groups.push(SheetGroup {
                title: "This panel",
                rows: local,
            });
        }
        groups.push(SheetGroup {
            title: "Keys",
            rows: Self::rows(ctx.keys, tabs, search),
        });
        let title = format!("{title} · keys");
        panel(frame, ctx.theme, Section::Dial, &title, "", |w, h| {
            self.viewport(
                sheet(ctx.theme, &groups, extra, "Esc", usize::from(w)),
                usize::from(w),
                usize::from(h),
                ctx.theme,
            )
        });
        true
    }
}

#[cfg(test)]
mod tests;

/// Hit targets from the last rendered panel, in terminal coordinates. Only real selectable rows
/// are recorded: headers, padding, clipped rows and modal editors cannot accidentally select.
#[derive(Default)]
pub(super) struct RowHits {
    area: ratatui::layout::Rect,
    rows: Vec<(ratatui::layout::Rect, usize)>,
}
impl RowHits {
    pub fn begin(&mut self, area: ratatui::layout::Rect) {
        self.area = if area.width >= 5 && area.height >= 5 {
            area.inner(ratatui::layout::Margin::new(1, 1))
        } else {
            area
        };
        self.rows.clear();
    }
    pub fn row(&mut self, x: usize, y: usize, width: usize, index: usize) {
        if y >= usize::from(self.area.height) {
            return;
        }
        let rect = ratatui::layout::Rect::new(
            self.area.x.saturating_add(x as u16),
            self.area.y.saturating_add(y as u16),
            width.min(usize::from(self.area.width).saturating_sub(x)) as u16,
            1,
        );
        self.rows.push((rect, index));
    }
    pub fn hit(&self, x: u16, y: u16) -> Option<usize> {
        self.rows
            .iter()
            .find(|(rect, _)| rect.contains((x, y).into()))
            .map(|(_, index)| *index)
    }
}
