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

/// Plain, request-only harness text, not marked context, evidence or coordination. Share the
/// identity rule with coalescing so a note cannot become persistent merely by being merged.
pub(in crate::agent) fn is_plain_note(message: &Value) -> bool {
    message["role"] == "user"
        && message["dial_transient"] == true
        && message["content"].is_string()
        && message.as_object().is_some_and(|object| {
            object
                .keys()
                .all(|key| !key.starts_with("dial_") || key == "dial_transient" || is_marker(key))
        })
}

/// Retire delivered request-only notes before new feedback is appended. Match note identity,
/// not tail indices: projection/compaction can move notes and tool traffic can bury them.
/// Failed transport is not delivery. Safety boundaries and specially marked data remain.
pub(in crate::agent) fn retire_delivered(messages: &mut Vec<Value>, delivered: bool) {
    if delivered {
        messages.retain(|message| {
            !is_plain_note(message) || Priority::of(message) == Some(Priority::Safety)
        });
    }
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

/// Requests a delivered advisory note stays suppressed for when it repeats.
const REPEAT_WINDOW: u64 = 3;
/// Delivered notes remembered; older ones can no longer fall inside the window anyway.
const REMEMBERED: usize = 16;

/// Case- and punctuation-folded form of a note: every run of non-alphanumeric characters becomes
/// one space, so `"Looks good!"` and `"  looks   GOOD "` compare equal.
pub(in crate::agent) fn normalize(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalized phrases a coach emits that carry no actionable content: self-stops, completion
/// self-talk, "nothing to flag" and endorsements. Anything longer or more specific is real advice.
const FILLER: &[&str] = &[
    "stop",
    "stop here",
    "stop now",
    "halt",
    "abort",
    "done",
    "task done",
    "task complete",
    "complete",
    "finished",
    "ok",
    "okay",
    "no issue",
    "no issues",
    "no issue continue",
    "no issues continue",
    "no concern",
    "no concerns",
    "no problems",
    "no notes",
    "nothing to add",
    "nothing to flag",
    "nothing to report",
    "no further advice",
    "no further input",
    "lgtm",
    "looks good",
    "all good",
    "on track",
    "agent is on track",
    "continue",
    "carry on",
];

/// Whether `text` says nothing: empty, only punctuation, or generic filler.
pub(in crate::agent) fn is_content_free(text: &str) -> bool {
    let normalized = normalize(text);
    normalized.is_empty() || FILLER.contains(&normalized.as_str())
}

/// Advisory notes already delivered, so the same advice is not repeated every round. Safety and
/// failure-recovery notes are never counted or suppressed: they state enforced boundaries and
/// recovery the model must always see.
///
/// `curate` remembers its selected note immediately. A controller that can discard a request
/// before the model answers calls [`Emissions::retract`] before curating the retry, so an
/// undelivered note is not suppressed as a repeat of itself, and [`Emissions::confirm`] once a
/// reply proves delivery.
#[derive(Debug, Default)]
pub(in crate::agent) struct Emissions {
    requests: u64,
    delivered: std::collections::VecDeque<(u64, String)>,
    /// The newest remembered note belongs to a request not yet confirmed as delivered.
    unconfirmed: bool,
}

impl Emissions {
    fn exempt(priority: Priority) -> bool {
        matches!(priority, Priority::Safety | Priority::Failure)
    }

    fn repeated(&self, key: &str) -> bool {
        self.delivered
            .iter()
            .any(|(at, seen)| seen == key && self.requests - at < REPEAT_WINDOW)
    }

    /// The last curated request reached the model.
    pub(in crate::agent) fn confirm(&mut self) {
        self.unconfirmed = false;
    }

    /// Forget the note selected for a request that was never answered.
    pub(in crate::agent) fn retract(&mut self) {
        if std::mem::take(&mut self.unconfirmed) {
            self.delivered.pop_back();
        }
    }
}

/// `cap` for one request with emission hygiene: content-free and recently delivered advisory
/// notes are removed first (so they cannot crowd out a real lower-priority note), the highest
/// priority survivor is kept, and that survivor is remembered for the next requests. Returns every
/// removed note.
pub(in crate::agent) fn curate(messages: &mut Vec<Value>, emissions: &mut Emissions) -> Vec<Value> {
    emissions.requests += 1;
    let tail = messages
        .iter()
        .rposition(|message| message["dial_transient"] != true)
        .map_or(0, |index| index + 1);
    let mut seen_now: Vec<String> = Vec::new();
    let mut remove = Vec::new();
    // Forward, so the first assembled note of a duplicate pair is the one kept.
    for (index, message) in messages.iter().enumerate().skip(tail) {
        let Some(priority) = Priority::of(message) else {
            continue;
        };
        if Emissions::exempt(priority) {
            continue;
        }
        let text = message["content"].as_str().unwrap_or_default();
        let key = normalize(text);
        if is_content_free(text) || emissions.repeated(&key) || seen_now.contains(&key) {
            remove.push(index);
        } else {
            seen_now.push(key);
        }
    }
    // Removing from the end keeps every lower index valid.
    let mut dropped: Vec<Value> = remove
        .into_iter()
        .rev()
        .map(|i| messages.remove(i))
        .collect();
    dropped.reverse();
    dropped.extend(cap(messages));
    emissions.unconfirmed = false;
    if let Some(kept) = messages[tail.min(messages.len())..]
        .iter()
        .find(|message| Priority::of(message).is_some_and(|priority| !Emissions::exempt(priority)))
    {
        let key = normalize(kept["content"].as_str().unwrap_or_default());
        let requests = emissions.requests;
        emissions.delivered.push_back((requests, key));
        emissions.unconfirmed = true;
        while emissions.delivered.len() > REMEMBERED {
            emissions.delivered.pop_front();
        }
    }
    dropped
}

#[cfg(test)]
mod tests;
