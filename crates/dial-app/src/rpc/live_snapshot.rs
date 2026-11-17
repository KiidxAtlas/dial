//! Recoverable interaction state, reduced under the event-envelope lock.
use serde_json::{Value, json};
use std::io::{self, Write};

// The Rust client accepts at most 64 MiB per frame. Leave 2 MiB for the response envelope,
// including a plain-RPC request id (the complete incoming request is limited to 1 MiB).
pub(super) const MAX_RECOVERY_SNAPSHOT_BYTES: usize = 62 * 1024 * 1024;

/// Refuse oversized recovery results before any response bytes reach the transport. Counting
/// through serde_json measures escaping and UTF-8 exactly without allocating another JSON copy.
pub(super) fn bounded(snapshot: Value, limit: usize) -> anyhow::Result<Value> {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| io::Error::other("recovery snapshot exceeds its response budget"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), &snapshot).map_err(|_| {
        super::coded(
            super::ErrorCode::Refused,
            format!("recovery snapshot exceeds the {limit}-byte response budget; history was not truncated; use messages_page for history and state.live for interactions (these do not clear a full-history gap)"),
        )
    })?;
    Ok(snapshot)
}

#[derive(Default)]
pub(super) struct LiveSnapshot {
    approvals: Vec<Value>,
    question: Option<Value>,
    todo: Vec<Value>,
}

impl LiveSnapshot {
    pub(super) fn observe(&mut self, event: &Value) {
        match event["event"].as_str() {
            Some("ready" | "active_session") => *self = Self::default(),
            Some("approval") => {
                self.approvals
                    .retain(|old| old["approval_id"] != event["approval_id"]);
                self.approvals.push(event.clone());
            }
            Some("approval_resolved") => {
                self.approvals
                    .retain(|old| old["approval_id"] != event["approval_id"]);
            }
            Some("question") => self.question = Some(event["request"].clone()),
            Some("question_closed") => {
                if self
                    .question
                    .as_ref()
                    .is_some_and(|question| question["question_id"] == event["question_id"])
                {
                    self.question = None;
                }
            }
            Some("todo") => self.todo = event["tasks"].as_array().cloned().unwrap_or_default(),
            _ => {}
        }
    }

    pub(super) fn snapshot(&self, session_id: Option<&str>, seq: u64) -> Value {
        json!({"session_id":session_id,"seq":seq,"pending_approvals":self.approvals,"question":self.question,"todo":self.todo})
    }
}

#[cfg(test)]
mod tests;
