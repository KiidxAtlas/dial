//! History browsing and incremental history search.
use super::super::{
    Rows, chrome,
    keys::Action,
    text,
    theme::{Sym, Tok},
};
use super::render::{finish_row, list_rows, render_list, substring_hits};
use super::{Composer, ComposerCtx, ComposerEvent, HistoryProvider, Search};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{style::Modifier, text::Span};

/// Sent drafts whose chips are kept for recall.
const MAX_RECALLS: usize = 20;

impl Composer {
    pub fn set_history_provider(&mut self, provider: Option<HistoryProvider>) {
        self.history_provider = provider;
        self.history_lookup.epoch = self.history_lookup.epoch.wrapping_add(1);
        self.history_lookup.pending = None;
        self.history_lookup.query = None;
        self.refresh_history_search();
    }

    pub fn history(&self) -> Vec<String> {
        self.history.clone()
    }

    pub fn push_history(&mut self, entry: String) {
        let entry = text::sanitize(&entry);
        if entry.trim().is_empty() {
            return;
        }
        self.remember_chips(&entry);
        if self.history.last() == Some(&entry) {
            return;
        }
        self.history.push(entry);
        self.trim_history();
    }

    /// When the draft being sent holds paste or image chips, keeps it so recalling `sent`
    /// brings the chips back rather than flooding the box with the expanded text.
    fn remember_chips(&mut self, sent: &str) {
        let used: Vec<(String, String)> = self
            .pastes
            .iter()
            .filter(|(marker, _)| self.editor.text.contains(marker.as_str()))
            .cloned()
            .collect();
        if used.is_empty() || super::paste::expand_pastes(&self.editor.text, &used) != sent {
            return;
        }
        self.recalls.retain(|(text, _, _)| text != sent);
        self.recalls
            .push((sent.to_owned(), self.editor.text.clone(), used));
        if self.recalls.len() > MAX_RECALLS {
            self.recalls.remove(0);
        }
    }

    /// The text to show for a recalled history entry: the draft with its chips when it was sent
    /// from this session with chips (their payloads registered again), else the entry itself.
    pub(super) fn recall(&mut self, entry: &str) -> String {
        let Some((_, draft, chips)) = self.recalls.iter().find(|(sent, _, _)| sent == entry) else {
            return entry.to_owned();
        };
        // A marker now standing for different text would expand to the wrong content.
        let clash = chips.iter().any(|(marker, body)| {
            self.pastes
                .iter()
                .any(|(live, live_body)| live == marker && live_body != body)
        });
        if clash {
            return entry.to_owned();
        }
        let draft = draft.clone();
        for chip in chips.clone() {
            if !self.pastes.contains(&chip) {
                self.pastes.push(chip);
            }
        }
        draft
    }

    /// Applies a changed `ui.history_limit` to the live history, dropping the oldest entries first.
    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_limit = limit;
        self.trim_history();
    }

    pub(super) fn trim_history(&mut self) {
        if self.history_limit != 0 {
            let excess = self.history.len().saturating_sub(self.history_limit);
            self.history.drain(..excess);
            // Browsing indexes refer to the old history. Rebase surviving entries and clamp
            // removed ones to the oldest remaining entry without losing the saved draft.
            if let Some(index) = self.history_index {
                self.history_index = Some(index.saturating_sub(excess));
            }
        }
    }

    /// Up/Down past the first/last line browse history. The unsent draft (and its cursor) is kept and
    /// comes back when browsing past the newest entry; edits to a browsed entry are not saved.
    pub(super) fn browse_history(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        if up {
            let index = match self.history_index {
                Some(index) => index.saturating_sub(1),
                None => {
                    self.saved_draft = Some(std::mem::take(&mut self.editor));
                    self.history.len() - 1
                }
            };
            self.history_index = Some(index);
            self.history_index = Some(index);
            let shown = self.recall(&self.history[index].clone());
            self.editor.set(&shown);
        } else if let Some(index) = self.history_index {
            if index + 1 < self.history.len() {
                self.history_index = Some(index + 1);
                let shown = self.recall(&self.history[index + 1].clone());
                self.editor.set(&shown);
            } else {
                self.leave_history();
            }
        }
    }

    /// Leaves history browsing and gives back the draft that was stashed when browsing began.
    pub(super) fn leave_history(&mut self) {
        self.history_index = None;
        self.editor = self.saved_draft.take().unwrap_or_default();
    }

    pub(super) fn search_matches(&self) -> Vec<usize> {
        let Some(search) = self.search.as_ref() else {
            return Vec::new();
        };
        let query = search.query.text.to_lowercase();
        search
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    pub(super) fn search_key(&mut self, key: KeyEvent, ctx: &ComposerCtx) -> ComposerEvent {
        let keys = ctx.keys;
        let rows = list_rows(ctx.ui);
        let matches = self.search_matches();
        let Some(search) = self.search.as_mut() else {
            return ComposerEvent::None;
        };
        let total = matches.len();
        if keys.matches(Action::Dismiss, &key) || keys.matches(Action::ClearDraft, &key) {
            self.search = None;
            return ComposerEvent::None;
        }
        let accept = keys.matches(Action::Submit, &key)
            || (keys.matches(Action::Complete, &key) && key.code != KeyCode::Right);
        if accept {
            let entry = matches
                .get(search.selected)
                .map(|&i| search.entries[i].clone());
            self.search = None;
            if let Some(entry) = entry {
                let shown = self.recall(&entry);
                self.set_draft(&shown);
            }
            return ComposerEvent::None;
        }
        let step = |search: &mut Search, delta: isize| {
            if total > 0 {
                search.selected =
                    (search.selected as isize + delta).clamp(0, total as isize - 1) as usize;
            }
            search.scroll = chrome::keep_in_view(search.selected, search.scroll, rows, total);
        };
        if keys.matches(Action::SelectUp, &key) {
            step(search, -1);
        } else if keys.matches(Action::SelectDown, &key)
            || keys.matches(Action::HistorySearch, &key)
        {
            step(search, 1);
        } else if keys.matches(Action::SelectPageUp, &key) {
            step(search, -(rows as isize));
        } else if keys.matches(Action::SelectPageDown, &key) {
            step(search, rows as isize);
        } else if search.query.key_with(key, keys) {
            search.selected = 0;
            search.scroll = 0;
        }
        ComposerEvent::None
    }

    pub(super) fn refresh_history_search(&mut self) -> bool {
        let query = self.search.as_ref().map(|search| search.query.text.clone());
        let lookup = &mut self.history_lookup;
        if lookup.query != query {
            lookup.epoch = lookup.epoch.wrapping_add(1);
            lookup.query = query.clone();
            lookup.pending = query
                .filter(|_| self.history_provider.is_some())
                .map(|query| {
                    (
                        lookup.epoch,
                        query,
                        std::time::Instant::now() + std::time::Duration::from_millis(75),
                    )
                });
        }
        let mut changed = false;
        while let Ok((epoch, entries)) = lookup.rx.try_recv() {
            lookup.running = false;
            if epoch == lookup.epoch
                && let Some(entries) = entries
                && let Some(search) = &mut self.search
            {
                // Results arrive after the user may have moved; keep the highlighted entry
                // when it is still a match, otherwise start at the newest.
                let query = search.query.text.to_lowercase();
                let kept =
                    search_selection(&search.entries, &query, search.selected).and_then(|entry| {
                        entries
                            .iter()
                            .filter(|e| e.to_lowercase().contains(&query))
                            .position(|e| *e == entry)
                    });
                search.entries = entries;
                search.selected = kept.unwrap_or(0);
                if kept.is_none() {
                    search.scroll = 0;
                }
                changed = true;
            }
        }
        if !lookup.running
            && lookup
                .pending
                .as_ref()
                .is_some_and(|(_, _, due)| *due <= std::time::Instant::now())
            && let Some(provider) = self.history_provider.clone()
            && let Some((epoch, query, _)) = lookup.pending.take()
        {
            let tx = lookup.tx.clone();
            // A provider that panics still reports back, so later queries keep searching.
            let spawned = std::thread::Builder::new()
                .name("history-search".into())
                .spawn(move || {
                    let entries = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        provider(&query, 500)
                    }))
                    .ok();
                    let _ = tx.send((epoch, entries));
                });
            lookup.running = spawned.is_ok();
        }
        changed
    }

    /// The search prompt row, then the matching history entries (newest first). Returns the cursor
    /// column inside the prompt row, which is the first returned row.
    pub(super) fn search_rows(
        &self,
        search: &Search,
        w: usize,
        ctx: &ComposerCtx,
    ) -> (Rows, usize) {
        let theme = ctx.theme;
        let label = "search";
        let prompt = theme.sym(Sym::Prompt);
        let head = vec![
            Span::styled(format!("{label} "), theme.fg(Tok::Muted)),
            Span::styled(format!("{prompt} "), theme.fg(Tok::Accent)),
            Span::raw(search.query.text.clone()),
        ];
        let col = text::width(label)
            + 1
            + text::width(prompt)
            + 1
            + text::width(&search.query.text[..search.query.cursor]);
        let mut rows = vec![finish_row(head, w)];

        let matches = self.search_matches();
        if matches.is_empty() {
            rows.push(finish_row(
                vec![Span::styled("  No matching history", theme.fg(Tok::Dim))],
                w,
            ));
            return (rows, col.min(w - 1));
        }
        let total = matches.len();
        let shown = list_rows(ctx.ui).min(total);
        let start = chrome::keep_in_view(search.selected, search.scroll, shown, total);
        let pointer = theme.sym(Sym::Pointer);
        let blank_pointer = " ".repeat(text::width(pointer));
        let accent = theme.fg(Tok::Accent);
        let hit = accent.add_modifier(Modifier::BOLD);
        let query = search.query.text.to_lowercase();
        rows.extend(render_list(
            theme,
            w,
            total,
            shown,
            start,
            search.selected,
            |i, budget, selected| {
                let entry = text::one_line(&search.entries[matches[i]]);
                let entry = text::ellipsize(&entry, budget.saturating_sub(2), theme.ascii());
                let hits = substring_hits(&entry, &query);
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
                ];
                spans.extend(chrome::highlighted(&entry, &hits, theme.fg(Tok::Text), hit));
                spans
            },
        ));
        (rows, col.min(w - 1))
    }
}

/// The entry at `selected` among those of `entries` matching the lowercased `query`.
fn search_selection(entries: &[String], query: &str, selected: usize) -> Option<String> {
    entries
        .iter()
        .filter(|e| e.to_lowercase().contains(query))
        .nth(selected)
        .cloned()
}
