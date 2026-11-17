//! Session-local identities for queued input. Recovery keeps its existing tuple format;
//! restored input receives new identities so an old panel can never target recovered intent.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

type Message = (String, bool);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub(in crate::ui::composer) struct Items {
    entries: VecDeque<(u64, Message)>,
}

impl Items {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Message> {
        self.entries.iter().map(|(_, message)| message)
    }
    pub fn snapshot(&self) -> Vec<(u64, String, bool)> {
        self.entries
            .iter()
            .map(|(id, (text, follow_up))| (*id, text.clone(), *follow_up))
            .collect()
    }
    pub fn index(&self, id: u64) -> Option<usize> {
        self.entries
            .iter()
            .position(|(candidate, _)| *candidate == id)
    }
    pub fn insert(&mut self, index: usize, message: Message) {
        self.entries
            .insert(index, (NEXT_ID.fetch_add(1, Ordering::Relaxed), message));
    }
    pub fn push_front(&mut self, message: Message) {
        self.insert(0, message);
    }
    pub fn push_back(&mut self, message: Message) {
        self.insert(self.len(), message);
    }
    pub fn pop_front(&mut self) -> Option<Message> {
        self.entries.pop_front().map(|(_, message)| message)
    }
    pub fn remove(&mut self, index: usize) -> Option<Message> {
        self.entries.remove(index).map(|(_, message)| message)
    }
    pub fn take_steering(&mut self) -> Vec<String> {
        let mut steering = Vec::new();
        let mut kept = VecDeque::new();
        for (id, (text, follow_up)) in self.entries.drain(..) {
            if follow_up {
                kept.push_back((id, (text, true)));
            } else {
                steering.push(text);
            }
        }
        self.entries = kept;
        steering
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    #[cfg(test)]
    pub fn back(&self) -> Option<&Message> {
        self.entries.back().map(|(_, message)| message)
    }
}

impl From<Vec<Message>> for Items {
    fn from(messages: Vec<Message>) -> Self {
        let mut items = Self::default();
        for message in messages {
            items.push_back(message);
        }
        items
    }
}

impl Extend<Message> for Items {
    fn extend<T: IntoIterator<Item = Message>>(&mut self, messages: T) {
        for message in messages {
            self.push_back(message);
        }
    }
}
