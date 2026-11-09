//! Shared panel navigation and contextual help; text editors retain punctuation input.
//!
//! [`PanelKeys`] owns the `?`/F1 key sheet every panel offers, and [`sheet`] draws any panel's key
//! reference in the one look (the git panel's richer sheet uses it too).
use super::DrawCtx;
use super::settings::{pair_label, panel};
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
            let pad = key_w.saturating_sub(text::width(keys));
            out.push(Line::from(vec![
                Span::styled(
                    format!("   {keys}{}", " ".repeat(pad)),
                    theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {label}"), theme.fg(Tok::Text)),
            ]));
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
    out
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
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::Char('?')
            ) {
                self.help = false;
            }
            return true;
        }
        if key.code == KeyCode::F(1) || (key.code == KeyCode::Char('?') && !text_focused) {
            self.help = true;
            return true;
        }
        false
    }

    /// Whether the key sheet is showing.
    pub fn open(&self) -> bool {
        self.help
    }

    /// The common rows of the key sheet: list movement, the main action, tabs when the panel has
    /// them, `/` when it opens a search, this sheet and the way back.
    fn rows(keys: &Keymap, tabs: bool, search: bool) -> Vec<(String, String)> {
        let mut rows = vec![
            (
                pair_label(keys, Action::SelectUp, Action::SelectDown),
                "move the selection".to_owned(),
            ),
            (
                pair_label(keys, Action::SelectPageUp, Action::SelectPageDown),
                "move a page".to_owned(),
            ),
            (
                keys.label(Action::SelectConfirm),
                "open or apply the selected row".to_owned(),
            ),
        ];
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

    /// Render the key sheet instead of the underlying panel while help is open: the shared rows,
    /// then the panel's own `extra` notes (one paragraph per line).
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
        let groups = [SheetGroup {
            title: "Keys",
            rows: Self::rows(ctx.keys, tabs, search),
        }];
        let title = format!("{title} · keys");
        panel(frame, ctx.theme, Section::Dial, &title, "", |w, _| {
            sheet(ctx.theme, &groups, extra, "Esc", usize::from(w))
        });
        true
    }
}

#[cfg(test)]
mod tests;
