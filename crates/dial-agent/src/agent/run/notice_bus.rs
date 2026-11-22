//! Single request-tail notice bus for one parent turn.
//!
//! Owns transient notices queued for the next model request (`pending`), notes awaiting
//! confirmation that the last reply reached durable history (`delivered` + `reply_mark`), and
//! advisory deduplication (`Emissions`). Every advisory, protocol, hook, coordination, and
//! monitor note flows through one flush boundary (`flush_to_request`) that runs
//! `guidance::assemble` once per request.
use crate::agent::run::guidance::{self, Emissions, Priority};
use serde_json::{Value, json};

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(in crate::agent) struct NoticeBus {
    pending: Vec<Value>,
    delivered: Vec<Value>,
    reply_mark: Option<usize>,
    emissions: Emissions,
}

impl NoticeBus {
    /// Queue a prioritized advisory note for the next request.
    pub(in crate::agent) fn push_advisory(&mut self, priority: Priority, content: impl Into<String>) {
        self.pending.push(guidance::note(priority, content));
    }

    /// Queue an untagged transient protocol or continuation notice (never filtered as advice).
    pub(in crate::agent) fn push_protocol(&mut self, content: impl Into<String>) {
        self.pending.push(json!({
            "role": "user",
            "dial_transient": true,
            "content": content.into(),
        }));
    }

    /// Queue a pre-built transient message (marked context, review, or hook note).
    pub(in crate::agent) fn push_raw(&mut self, note: Value) {
        self.pending.push(note);
    }

    /// Queue several transient messages in order.
    pub(in crate::agent) fn extend(&mut self, notes: impl IntoIterator<Item = Value>) {
        self.pending.extend(notes);
    }

    /// Keep only pending notes matching `predicate`.
    pub(in crate::agent) fn retain(&mut self, predicate: impl FnMut(&Value) -> bool) {
        self.pending.retain(predicate);
    }

    /// Direct read view of currently queued notes.
    #[cfg(test)]
    pub(in crate::agent) fn pending(&self) -> &[Value] {
        &self.pending
    }

    /// Mutable access for lifecycle hooks that append directly into a note buffer.
    pub(in crate::agent) fn pending_mut(&mut self) -> &mut Vec<Value> {
        &mut self.pending
    }

    /// Record that a reply answered the last request: hold its queued notes until the reply is
    /// confirmed in durable history (`settle_delivered`).
    pub(in crate::agent) fn reply_received(&mut self, history_len: usize) {
        self.delivered = std::mem::take(&mut self.pending);
        self.reply_mark = Some(history_len);
    }

    /// Confirm or restore the previous request's notes before building the next request.
    /// Returns `Some(delivered)` when the previous reply reached durable history; returns `None`
    /// and re-queues those notes ahead of newer ones when that reply was discarded.
    pub(in crate::agent) fn settle_delivered(
        &mut self,
        assistants_since: impl FnOnce(usize) -> usize,
    ) -> Option<Vec<Value>> {
        let mark = self.reply_mark.take()?;
        if assistants_since(mark) > 0 {
            self.emissions.confirm();
            Some(std::mem::take(&mut self.delivered))
        } else {
            let mut again = std::mem::take(&mut self.delivered);
            again.append(&mut self.pending);
            self.pending = again;
            self.emissions.retract();
            None
        }
    }

    /// Apply a turn transition: clear pending reply confirmation, optionally clear advisory
    /// repeat memory (steering), and optionally drop non-safety advice (steering or resumed).
    pub(in crate::agent) fn reset(
        &mut self,
        clear_emissions: bool,
        reset_pending_for_steering: bool,
    ) {
        self.delivered.clear();
        self.reply_mark = None;
        if clear_emissions {
            self.emissions = Emissions::default();
        }
        if reset_pending_for_steering {
            guidance::reset_for_steering(&mut self.pending);
        }
    }

    /// Append queued notes onto `messages`, invoke `build_extra` with the full message view
    /// (avoiding any history clone), append the returned extra notes, retract any unconfirmed
    /// advisory emission from an unanswered prior request, and run the single `guidance::assemble`
    /// pass from `notes_start`. Returns the dropped advisory notes.
    pub(in crate::agent) fn flush_with(
        &mut self,
        messages: &mut Vec<Value>,
        notes_start: usize,
        build_extra: impl FnOnce(&[Value]) -> Vec<Value>,
    ) -> Vec<Value> {
        messages.extend(self.pending.iter().cloned());
        let extra_notes = build_extra(messages);
        messages.extend(extra_notes);
        self.emissions.retract();
        guidance::assemble(messages, notes_start, &mut self.emissions)
    }

    /// Convenience wrapper around [`Self::flush_with`] for static extra notes.
    #[cfg(test)]
    pub(in crate::agent) fn flush_to_request(
        &mut self,
        messages: &mut Vec<Value>,
        notes_start: usize,
        extra_notes: impl IntoIterator<Item = Value>,
    ) -> Vec<Value> {
        let extra: Vec<Value> = extra_notes.into_iter().collect();
        self.flush_with(messages, notes_start, |_| extra)
    }
}
