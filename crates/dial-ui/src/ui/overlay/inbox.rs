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
}

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

    pub fn choices(&self) -> ChoiceSpec {
        ChoiceSpec {
            title: format!("Inbox · {} unread", self.unread()),
            command: "inbox".into(),
            summary: "Session notifications; Enter opens a notice's Read and notification settings actions. Opening the inbox or preferences never marks notices read.".into(),
            items: self.entries.iter().rev().map(|entry| ChoiceItem {
                value: format!("notice {}", entry.id),
                label: format!("{}{}", if entry.unread { "Unread · " } else { "" }, entry.title),
                detail: entry.text.clone(),
                current: false,
            }).chain(std::iter::once(ChoiceItem {
                value: "notifications".into(),
                label: "Notification settings".into(),
                detail: "Change notification preferences without reading or dismissing notices".into(),
                current: false,
            })).collect(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;
