//! At most one advisory harness note per model request.
//!
//! Advisory notes (advisor blockers, loop warnings, failure coaching, review and todo reminders) are tagged with a
//! priority and assembled at the request tail. Several can become due in the same request; small
//! models follow one clear instruction better than a stack of competing ones, so only the
//! highest-priority note is sent. Dropping a note only removes model-facing text: refusals,
//! withheld calls, completion gates and stops are enforced by their own mechanisms.
//!
//! Untagged transient notes are protocol or data, not advice, and are always delivered: tool
//! contract restatements, continuation prompts after truncated/stalled/empty output, unreadable
//! tool-call notices, the planning gate, job completion notices, coach consultation results and
//! marked context (rules, memory, context notes, task packets and steering).
use serde_json::{Value, json};

const KEY: &str = "dial_guidance";

/// Selection order: earlier variants win. Within a variant the note assembled first wins, so the
/// choice is deterministic for a given request. `Repetition` and `Loop` together form the
/// loop/repetition tier; the identical-call guard is its most specific signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::agent) enum Priority {
    /// An uncertain effect or enforced boundary the model must account for. Today's tail has no
    /// such note: uncertain effects stop the turn and refusals arrive as tool results.
    Safety,
    /// An unresolved advisor blocker: the coach judged the work cannot be correct as it stands.
    Blocker,
    /// Recovery from failed or rejected calls: failed rounds, rejected todo updates, contract
    /// breaks.
    Failure,
    /// The same call repeated with the same arguments.
    Repetition,
    /// Withheld unchanged calls, narration without progress and wasted rounds.
    Loop,
    /// Completion and review reminders.
    Review,
    /// The current-todo reminder after the newest tool result.
    Todo,
}

impl Priority {
    pub(in crate::agent) const ALL: [Self; 7] = [
        Self::Safety,
        Self::Blocker,
        Self::Failure,
        Self::Repetition,
        Self::Loop,
        Self::Review,
        Self::Todo,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Safety => "safety",
            Self::Blocker => "blocker",
            Self::Failure => "failure",
            Self::Repetition => "repetition",
            Self::Loop => "loop",
            Self::Review => "review",
            Self::Todo => "todo",
        }
    }

    fn of(message: &Value) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|priority| message[KEY] == priority.as_str())
    }
}

/// A transient advisory note for the next request.
pub(in crate::agent) fn note(priority: Priority, content: impl Into<String>) -> Value {
    let mut message = json!({"role":"user","content":content.into(),"dial_transient":true});
    tag(&mut message, priority);
    message
}

/// Mark an already built transient message as advisory guidance of `priority`.
pub(in crate::agent) fn tag(message: &mut Value, priority: Priority) {
    message[KEY] = json!(priority.as_str());
}

/// Whether `key` is the advisory marker, which does not make a harness note special.
pub(in crate::agent) fn is_marker(key: &str) -> bool {
    key == KEY
}

/// Steering replaces the task's recovery context, not protocol data, worker findings or safety
/// boundaries. Do not queue the previous task's failure/completion advice for its replacement.
pub(in crate::agent) fn reset_for_steering(messages: &mut Vec<Value>) {
    messages.retain(|message| {
        Priority::of(message).is_none_or(|priority| priority == Priority::Safety)
    });
}

/// Keep only the highest-priority advisory note among this request's trailing transient
/// messages and return the dropped ones. Stored history before the tail is never touched.
pub(in crate::agent) fn cap(messages: &mut Vec<Value>) -> Vec<Value> {
    let tail = messages
        .iter()
        .rposition(|message| message["dial_transient"] != true)
        .map_or(0, |index| index + 1);
    let Some((_, keep)) = (tail..messages.len())
        .filter_map(|index| Priority::of(&messages[index]).map(|priority| (priority, index)))
        .min()
    else {
        return Vec::new();
    };
    let mut dropped = Vec::new();
    // Removing from the end keeps every lower index, including `keep`, valid.
    for index in (tail..messages.len()).rev() {
        if index != keep && Priority::of(&messages[index]).is_some() {
            dropped.push(messages.remove(index));
        }
    }
    dropped.reverse();
    dropped
}

#[cfg(test)]
mod tests;
