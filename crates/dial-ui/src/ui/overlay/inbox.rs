//! Bounded, session-local notification inbox. Transcript delivery is independent of read state.
use super::palette::{ChoiceItem, ChoiceSpec};

const LIMIT: usize = 200;

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
}

impl Inbox {
    /// Exact repeats do not become unread again, even after the user has read them.
    pub fn push(&mut self, title: &str, text: &str) {
        self.push_category(title, text, Category::General);
    }

    pub fn push_category(&mut self, title: &str, text: &str, category: Category) {
        // A busy turn does not poll `returned`, so the absence is also marked on arrival.
        if !crate::ui::termcaps::terminal_focused() {
            self.mark_away();
        }
        if text.trim().is_empty()
            || self
                .entries
                .iter()
                .any(|entry| entry.title == title && entry.text == text)
        {
            return;
        }
        self.next_id = self.next_id.saturating_add(1);
        if self.entries.len() == LIMIT {
            self.entries.remove(0);
        }
        self.entries.push(Notice {
            id: self.next_id,
            title: title.to_owned(),
            text: text.to_owned(),
            unread: true,
            category,
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
            .filter(|entry| entry.id > since)
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

    /// The inbox picker. Enter on a notice reads it at once (shows it in the transcript and marks
    /// it read); unread notices carry the picker's `unread` mark and the cursor starts on the
    /// newest one. Mark-all-read and the notification settings for each category present follow
    /// the notices. Every row runs `/inbox open <value>`.
    pub fn choices(&self) -> ChoiceSpec {
        let unread = self.unread();
        let mut items: Vec<ChoiceItem> = self
            .entries
            .iter()
            .rev()
            .map(|entry| ChoiceItem {
                value: entry.id.to_string(),
                label: entry.title.clone(),
                detail: entry.text.clone(),
                current: entry.unread,
            })
            .collect();
        if unread > 0 {
            items.push(ChoiceItem {
                value: "read-all".into(),
                label: "Mark all read".into(),
                detail: format!(
                    "Mark {unread} notice{} read without showing them",
                    if unread == 1 { "" } else { "s" }
                ),
                current: false,
            });
        }
        for (category, label) in [
            (Category::Error, "Error notification settings"),
            (Category::Completion, "Completion notification settings"),
        ] {
            if let Some(entry) = self.entries.iter().rev().find(|e| e.category == category) {
                items.push(ChoiceItem {
                    value: format!("notifications {}", entry.id),
                    label: label.into(),
                    detail: "Change these notification preferences without reading notices".into(),
                    current: false,
                });
            }
        }
        items.push(ChoiceItem {
            value: "notifications".into(),
            label: "Notification settings".into(),
            detail: "Change notification preferences without reading or dismissing notices".into(),
            current: false,
        });
        ChoiceSpec {
            title: format!("Inbox · {unread} unread"),
            command: "inbox".into(),
            verb: "open",
            summary: "Session notifications; Enter shows a notice and marks it read. Opening the inbox or preferences never marks notices read.".into(),
            current_tag: Some("unread".into()),
            items,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;
