//! Bounded, session-local notification inbox. Transcript delivery is independent of read state.
use super::palette::{ChoiceItem, ChoiceSpec};
use crate::ui::text;

const LIMIT: usize = 200;
const COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// Explicit producer metadata, never inferred from a title or message body.
/// Categories without their own supported preference retain the general destination.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Category {
    #[default]
    General,
    Completion,
    Error,
}

impl Category {
    fn settings_path(self) -> &'static str {
        match self {
            Self::Error => "ui.notify_errors",
            Self::General | Self::Completion => "ui.notify",
        }
    }
}

#[derive(Default)]
pub struct Inbox {
    next_id: u64,
    entries: Vec<Notice>,
    overflow_unread: u64,
    /// A focus report has said the terminal is focused; before that the state is unknown.
    seen_focus: bool,
    /// The newest notice id when the terminal lost focus.
    away_since: Option<u64>,
}

/// Notices one return digest names before it summarizes the rest.
const DIGEST_ROWS: usize = 3;

pub struct Notice {
    pub id: u64,
    pub title: String,
    pub text: String,
    pub unread: bool,
    pub category: Category,
    pub occurrences: u64,
    first_seen: std::time::Instant,
    last_arrival: u64,
}

impl Inbox {
    /// Repeats within one minute coalesce, updating recency and reopening read notices.
    pub fn push(&mut self, title: &str, text: &str) {
        self.push_category(title, text, Category::General);
    }

    pub fn push_category(&mut self, title: &str, text: &str, category: Category) {
        self.push_at(title, text, category, std::time::Instant::now());
    }

    fn push_at(&mut self, title: &str, text: &str, category: Category, now: std::time::Instant) {
        if text.trim().is_empty() {
            return;
        }
        // A busy turn does not poll `returned`, so the absence is also marked on arrival.
        if !crate::ui::termcaps::terminal_focused() {
            self.mark_away();
        }
        self.next_id = self.next_id.saturating_add(1);
        if let Some(index) = self.entries.iter().rposition(|entry| {
            entry.title == title
                && entry.text == text
                && entry.category == category
                && now.saturating_duration_since(entry.first_seen) <= COALESCE_WINDOW
        }) {
            let mut entry = self.entries.remove(index);
            entry.last_arrival = self.next_id;
            entry.unread = true;
            entry.occurrences = entry.occurrences.saturating_add(1);
            self.entries.push(entry);
            return;
        }
        if self.entries.len() == LIMIT {
            let index = self
                .entries
                .iter()
                .position(|entry| !entry.unread)
                .unwrap_or(0);
            if self.entries.remove(index).unread {
                self.overflow_unread = self.overflow_unread.saturating_add(1);
            }
        }
        self.entries.push(Notice {
            id: self.next_id,
            title: title.to_owned(),
            text: text.to_owned(),
            unread: true,
            category,
            occurrences: 1,
            first_seen: now,
            last_arrival: self.next_id,
        });
    }

    fn mark_away(&mut self) {
        if self.seen_focus && self.away_since.is_none() {
            self.away_since = Some(self.next_id);
        }
    }

    /// Tracks terminal focus. On the return from a real focus loss, lists the notices that
    /// arrived meanwhile, newest first; `None` when nothing arrived. An unknown focus state
    /// reads as unfocused, so absence only counts after one focused report.
    pub fn returned(&mut self, focused: bool) -> Option<String> {
        if !focused {
            self.mark_away();
            return None;
        }
        self.seen_focus = true;
        let since = self.away_since.take()?;
        let arrived: Vec<&Notice> = self
            .entries
            .iter()
            .rev()
            .filter(|entry| entry.last_arrival > since)
            .collect();
        if arrived.is_empty() {
            return None;
        }
        let mut lines: Vec<String> = arrived
            .iter()
            .take(DIGEST_ROWS)
            .map(|entry| {
                let first = entry.text.lines().next().unwrap_or_default();
                format!("{}: {}", entry.title, crate::ui::text::one_line(first))
            })
            .collect();
        if arrived.len() > DIGEST_ROWS {
            lines.push(format!(
                "+{} more; /inbox lists them",
                arrived.len() - DIGEST_ROWS
            ));
        }
        Some(lines.join("\n"))
    }

    pub fn unread(&self) -> usize {
        self.entries.iter().filter(|entry| entry.unread).count()
    }

    pub fn read(&mut self, id: u64) -> Option<(&str, &str)> {
        let entry = self.entries.iter_mut().find(|entry| entry.id == id)?;
        entry.unread = false;
        Some((&entry.title, &entry.text))
    }

    /// Opening a notice menu or its preferences never marks it read.
    pub fn notice_choices(&self, id: u64) -> Option<ChoiceSpec> {
        let entry = self.entries.iter().find(|entry| entry.id == id)?;
        let (label, detail) = match entry.category {
            Category::Error => (
                "Error notification settings",
                "Change error notification preferences, not notice read state",
            ),
            Category::Completion => (
                "Completion notification settings",
                "Change completion/question notification preferences, not notice read state",
            ),
            Category::General => (
                "Notification settings",
                "No category-specific preference is recorded; open general notification preferences",
            ),
        };
        Some(ChoiceSpec {
            title: entry.title.clone(),
            command: "inbox".into(),
            summary: entry.text.clone(),
            items: vec![
                ChoiceItem {
                    value: id.to_string(),
                    label: "Read notice".into(),
                    detail: "Show this notice in the transcript and mark it read".into(),
                    current: false,
                },
                ChoiceItem {
                    value: format!("notifications {id}"),
                    label: label.into(),
                    detail: detail.into(),
                    current: false,
                },
            ],
            ..Default::default()
        })
    }

    pub fn settings_path(&self, id: u64) -> Option<&'static str> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.category.settings_path())
    }

    /// Marks every notice read; returns how many were unread.
    pub fn read_all(&mut self) -> usize {
        let mut count = 0;
        for entry in self.entries.iter_mut().filter(|entry| entry.unread) {
            entry.unread = false;
            count += 1;
        }
        count
    }

    /// The inbox picker. Bulk actions precede notices; unread notices carry a searchable mark.
    /// Every row runs `/inbox open <value>`.
    pub fn choices(&self) -> ChoiceSpec {
        self.choices_filtered(false)
    }

    pub fn choices_filtered(&self, unread_only: bool) -> ChoiceSpec {
        let unread = self.unread();
        let mut items = Vec::new();
        if unread > 0 {
            items.push(ChoiceItem {
                value: "read-all".into(),
                label: "Mark all read".into(),
                detail: format!(
                    "Mark {} read without showing them",
                    text::counted(unread, "notice")
                ),
                current: false,
            });
        }
        items.push(ChoiceItem {
            value: if unread_only { "all" } else { "unread" }.into(),
            label: if unread_only {
                "Show all notices"
            } else {
                "Show unread only"
            }
            .into(),
            detail: "Filter retained notices without changing their read state".into(),
            current: false,
        });
        items.extend(
            self.entries
                .iter()
                .rev()
                .filter(|entry| !unread_only || entry.unread)
                .map(|entry| {
                    let mut detail = if entry.unread {
                        "unread · ".to_owned()
                    } else {
                        String::new()
                    };
                    if entry.occurrences > 1 {
                        detail.push_str(&format!("{} occurrences · ", entry.occurrences));
                    }
                    detail.push_str(&entry.text);
                    ChoiceItem {
                        value: entry.id.to_string(),
                        label: entry.title.clone(),
                        detail,
                        current: entry.unread,
                    }
                }),
        );
        // One settings row. It opens settings at `ui.notify`, beside `ui.notify_errors`, so every
        // category's preference is one step away without a settings row per kind of notice.
        items.push(ChoiceItem {
            value: "notifications".into(),
            label: "Notification settings".into(),
            detail: "Change when Dial notifies you; notices keep their read state".into(),
            current: false,
        });
        ChoiceSpec {
            command: "inbox".into(),
            title: format!(
                "Inbox{} · {unread} unread",
                if unread_only { " · unread only" } else { "" }
            ),
            verb: "open",
            summary: format!(
                "Enter shows a notice and marks it read; opening the inbox or settings marks nothing read.{}",
                if self.overflow_unread > 0 {
                    format!(
                        " {} evicted at the {LIMIT}-notice limit; transcript delivery is unchanged.",
                        text::counted(self.overflow_unread, "unread notice"),
                    )
                } else {
                    String::new()
                }
            ),
            current_tag: Some("unread".into()),
            items,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;
