//! Request-tail notes on preserved-thinking routes.
//!
//! Harness notes (mode notice, task context, guidance, background and hook notices) are request
//! transients: they follow the history of one request and are rebuilt for the next. On a model
//! with preserved thinking, a reply's signed thinking is bound to everything sent before it, so
//! dropping those notes on the next request is a history edit that the API rejects once that
//! thinking is replayed. Here the notes a request actually carried are recorded once its reply
//! arrives, in a branch-local session record that never enters stored history, and re-sent at the
//! same place on every later request of the same history epoch. A note whose kind and text equal
//! the latest re-sent note of that kind is not sent again, so restated state (the mode notice, an
//! unchanged task context) accumulates only when it changes. A compaction or context reset starts
//! a new epoch; its replacement history already unbinds retained thinking.
use crate::agent::Agent;
use crate::config::compaction::CompactionIdentity;
use crate::storage::transcript::is_history;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Session custom-record type: the notes one request carried after its history.
pub(in crate::agent) const RECORD: &str = "dial_sent_notes";
/// A re-sent note in a request; its value is the note's kind (role and harness markers).
pub(in crate::agent) const MARKER: &str = "dial_sent_note";

/// The note's identity apart from its text: its role and every harness marker it carries.
fn kind(message: &Value) -> String {
    let mut kind = message["role"].as_str().unwrap_or_default().to_owned();
    if let Some(object) = message.as_object() {
        let mut markers: Vec<_> = object
            .iter()
            .filter(|(key, _)| key.starts_with("dial_") && *key != "dial_transient")
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        markers.sort();
        for marker in markers {
            kind.push('|');
            kind.push_str(&marker);
        }
    }
    kind
}

/// The form a sent note is re-sent in: the same wire message (role and content), no longer a
/// transient, so it is history-positioned and may carry a cache breakpoint like any message.
fn resend(message: &Value) -> Value {
    let mut note = json!({"role": message["role"].clone(), "content": message["content"].clone()});
    note[MARKER] = json!(kind(message));
    note
}

/// History identity: a compaction or context reset replaces the first message of the history.
fn epoch(messages: &[Value]) -> Option<String> {
    let first = messages
        .iter()
        .find(|message| is_history(message) && message.get(MARKER).is_none())?;
    let digest = Sha256::digest(serde_json::to_vec(first).ok()?);
    Some(
        digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// The trailing notes of a sent request, in their re-send form.
fn sent_tail(messages: &[Value]) -> Vec<Value> {
    let tail = crate::agent::run::guidance::tail_start(messages);
    messages[tail..]
        .iter()
        .filter(|message| {
            message["dial_transient"] == true
                && matches!(message["role"].as_str(), Some("user" | "assistant"))
        })
        .map(resend)
        .collect()
}

/// Drop trailing notes whose kind and text equal the latest re-sent note of that kind.
pub(in crate::agent) fn drop_repeated(messages: &mut Vec<Value>) {
    let tail = crate::agent::run::guidance::tail_start(messages);
    let (sent, notes) = messages.split_at(tail);
    let repeated: Vec<bool> = notes
        .iter()
        .map(|note| {
            let kind = kind(note);
            sent.iter()
                .rev()
                .find(|message| message[MARKER].as_str() == Some(kind.as_str()))
                .is_some_and(|previous| previous["content"] == note["content"])
        })
        .collect();
    let mut index = 0;
    messages.retain(|_| {
        let keep = index < tail || !repeated[index - tail];
        index += 1;
        keep
    });
}

/// Re-insert recorded notes into `messages` (system, history, then this request's tail) after
/// the history message each followed. Positions count `history`'s leading session messages, so
/// a preview with pending input keeps them; a different history (another epoch) gets none.
fn reinsert(messages: &mut Vec<Value>, records: &[(usize, Value)], session_messages: usize) {
    let history: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_history(message))
        .map(|(index, _)| index)
        .collect();
    if history.len() < session_messages {
        return;
    }
    let Some(epoch) = epoch(messages) else {
        return;
    };
    let mut inserts: Vec<(usize, Vec<Value>)> = Vec::new();
    for (position, record) in records {
        if record["epoch"].as_str() != Some(epoch.as_str()) || *position > session_messages {
            continue;
        }
        let Some(notes) = record["notes"].as_array() else {
            continue;
        };
        let at = match position.checked_sub(1) {
            Some(last) => history[last] + 1,
            None => history.first().copied().unwrap_or(messages.len()),
        };
        match inserts.last_mut() {
            Some((previous, batch)) if *previous == at => batch.extend(notes.iter().cloned()),
            _ => inserts.push((at, notes.clone())),
        }
    }
    // Records come in branch order, so positions never decrease; insert from the back.
    for (at, notes) in inserts.into_iter().rev() {
        messages.splice(at..at, notes);
    }
}

impl Agent<'_> {
    fn preserved_thinking_route(&self) -> bool {
        let selection = self.auth_config.selection();
        !self.hermetic
            && CompactionIdentity {
                provider: &selection.provider,
                model: &selection.model,
                agent: None,
            }
            .preserved_thinking()
    }

    /// Projection step: re-send the notes earlier requests of this history carried.
    pub(in crate::agent) fn project_sent_notes(&self, messages: &mut Vec<Value>) {
        if !self.preserved_thinking_route() {
            return;
        }
        let records = self.session.custom_records(RECORD);
        if !records.is_empty() {
            reinsert(messages, &records, self.session.message_count());
        }
    }

    /// Before sending: do not repeat a note whose identical text is already re-sent above.
    pub(in crate::agent) fn drop_repeated_notes(&self, messages: &mut Vec<Value>) {
        if self.preserved_thinking_route() {
            drop_repeated(messages);
        }
    }

    /// After a reply to `sent` (the exact provider request) arrived: record its trailing notes,
    /// which that reply's thinking is now bound to. Recording needs the owning durable session;
    /// without one, later requests send the notes only once, as on other routes.
    pub(in crate::agent) fn record_sent_notes(
        &mut self,
        sent: &[Value],
        ui: &mut dyn crate::agent::Ui,
    ) {
        if !self.preserved_thinking_route() || !self.session.owns_journal() {
            return;
        }
        let notes = sent_tail(sent);
        let history = sent
            .iter()
            .filter(|message| is_history(message) && message.get(MARKER).is_none())
            .count();
        // The notes followed exactly the session's current history; anything else (input that
        // arrived during the request) would misplace them.
        if notes.is_empty() || history != self.session.message_count() {
            return;
        }
        let Some(epoch) = epoch(sent) else {
            return;
        };
        if let Err(error) = self
            .session
            .append_custom_entry(RECORD, json!({"version":1,"epoch":epoch,"notes":notes}))
        {
            ui.diagnostic(&format!(
                "Request notes were not recorded ({error:#}); the next request may edit history this model's thinking is bound to."
            ));
        }
    }
}

#[cfg(test)]
mod tests;
