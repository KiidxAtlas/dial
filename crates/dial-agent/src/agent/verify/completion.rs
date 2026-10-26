//! Stop-time handling for open work and missing evidence. Text alone never closes pending todos:
//! an accepted completed answer closes only the in-progress task (see
//! `TodoList::complete_reported_work`), and not in goal mode. Edits with no passing check earn
//! one reminder per round; after it the answer is accepted, with the gap disclosed, not hidden.
use crate::agent::workflow::todo::{Status, TodoList};
use serde_json::{Value, json};

pub use crate::runtime::Phase as TurnOutcome;

pub struct Finish {
    pub outcome: TurnOutcome,
    pub text: String,
    pub summary: String,
    pub question: Option<String>,
    pub options: Vec<String>,
}

impl Finish {
    pub fn parse(args: &Value) -> Result<Self, String> {
        let outcome = match args["outcome"].as_str() {
            Some("completed") => TurnOutcome::Completed,
            Some("blocked") => TurnOutcome::Blocked,
            Some("needs_input") => TurnOutcome::NeedsInput,
            _ => return Err("finish outcome must be completed, blocked or needs_input".into()),
        };
        let summary = args["summary"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 8192)
            .ok_or("finish requires a nonempty summary of at most 8192 bytes")?
            .to_owned();
        let question = if outcome == TurnOutcome::Completed {
            if crate::agent::safety::stream_guards::announces_unexpected_stop(&summary)
                || crate::agent::safety::stream_guards::reports_unfinished_work(&summary)
            {
                return Err(
                    "completed requires a report of finished work, not a promise to act".into(),
                );
            }
            None
        } else {
            Some(
                args["reason"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && s.len() <= 8192)
                    .ok_or("blocked/needs_input requires reason: the concrete missing prerequisite or question for the user")?
                    .to_owned(),
            )
        };
        let options = if outcome == TurnOutcome::NeedsInput {
            match args.get("options").filter(|value| !value.is_null()) {
                None => Vec::new(),
                Some(Value::Array(values)) if (1..=8).contains(&values.len()) => values
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::trim)
                            .filter(|option| !option.is_empty() && option.len() <= 512)
                            .map(str::to_owned)
                            .ok_or(
                                "needs_input options must be nonempty strings of at most 512 bytes",
                            )
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(Value::Array(_)) => {
                    return Err("needs_input options must contain between 1 and 8 choices".into());
                }
                Some(_) => return Err("needs_input options must be an array of strings".into()),
            }
        } else {
            if args.get("options").is_some_and(|value| !value.is_null()) {
                return Err("options are only valid with outcome=needs_input".into());
            }
            Vec::new()
        };
        let text = if outcome == TurnOutcome::Completed {
            summary.clone()
        } else {
            let label = if outcome == TurnOutcome::Blocked {
                "Blocked"
            } else {
                "Needs input"
            };
            format!(
                "{label}: {summary}\n\n{}",
                question.as_deref().unwrap_or_default()
            )
        };
        Ok(Self {
            outcome,
            text,
            summary,
            question,
            options,
        })
    }
}

// Providers reject top-level schema composition. Keep conditional requirements
// in the description and enforce them in Finish::parse instead.
pub fn schema() -> Value {
    json!({"type":"function","function":{
        "name":"finish",
        "description":"Optional structured outcome; ordinary work completes with a plain final answer and needs no finish call. completed reports finished work and requires no live tasks still running. Todos are optional progress bookkeeping; an accepted completed report closes only the in-progress task and leaves pending entries open. Explicitly blocked tasks must be resolved or reported blocked. A review-pending response continues the turn: inspect the supplied diff and requested evidence, then call finish again. blocked means a concrete missing prerequisite prevents further work. needs_input asks a specific question and waits for the user's response inside the tool call, then continues the same turn; optionally provide 1–8 choices in options, and the interactive UI always offers an Other typed response. Both blocked and needs_input require reason and leave todos open. Use as the last call in a batch. Never claim completed for promised or unverified work.",
        "parameters":{"type":"object","properties":{
            "outcome":{"type":"string","enum":["completed","blocked","needs_input"]},
            "summary":{"type":"string","minLength":1,"maxLength":8192},
            "reason":{"type":"string","minLength":1,"maxLength":8192},
            "options":{"type":"array","items":{"type":"string","minLength":1,"maxLength":512},"minItems":1,"maxItems":8}
        },"required":["outcome","summary"],"additionalProperties":false}
    }})
}

/// Every task still owed, blocked ones with their reason: what `finish completed` refuses to leave behind.
pub fn open_work(todo: &TodoList) -> String {
    lines(todo, Status::is_open)
}

fn lines(todo: &TodoList, keep: impl Fn(Status) -> bool) -> String {
    todo.tasks()
        .iter()
        .enumerate()
        .filter(|(_, task)| keep(task.status))
        .map(|(index, task)| format!("- {}", task.label(index + 1)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `Blocked` carries the blocker list: every open task is blocked, so stopping is the honest outcome.
pub enum StopAction {
    Accept,
    Blocked(String),
}

/// What a completed answer owes for edits that no passing check covers.
#[derive(Debug, PartialEq, Eq)]
pub enum EvidenceGate {
    Accept,
    /// Ask once for a check, or for an explanation of why none can run.
    Remind(String),
    /// The reminder was spent: accept, and append this disclosure to the answer.
    Disclose(String),
}

/// Explicit labels in plain reports use the same outcomes as typed finish calls.
pub(in crate::agent) fn declared_outcome(text: &str) -> Option<TurnOutcome> {
    let first = text
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if first.starts_with("blocked:") || first.starts_with("blocked on ") {
        Some(TurnOutcome::Blocked)
    } else if first.starts_with("incomplete:") {
        Some(TurnOutcome::Incomplete)
    } else if first.starts_with("failed:") {
        Some(TurnOutcome::Failed)
    } else if first.starts_with("needs input:") {
        Some(TurnOutcome::NeedsInput)
    } else {
        None
    }
}

/// Recognize an explicit final request for clarification, not arbitrary question marks
/// in a report, code sample, or rhetorical explanation. Open todos must survive it.
pub(in crate::agent) fn awaiting_input(text: &str) -> bool {
    let last = text
        .trim()
        .rsplit("\n\n")
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !last.ends_with('?') || last.contains("```") {
        return false;
    }
    [
        "could you clarify",
        "can you clarify",
        "could you provide",
        "can you provide",
        "could you confirm",
        "can you confirm",
        "which option would you",
        "which would you like",
        "what would you like me to",
        "should i proceed with",
    ]
    .iter()
    .any(|prefix| last.starts_with(prefix))
}

/// One reminder for edits that no passing check covers, per round of a turn. Unstarted todos earn
/// none: an answer may cover them without another request, and the turn discloses open items.
#[derive(Default)]
pub struct CompletionGuard {
    evidence_reminded: bool,
}
impl CompletionGuard {
    pub fn stopped(&mut self, todo: &TodoList) -> StopAction {
        let blocked = lines(todo, |status| status == Status::Blocked);
        if blocked.is_empty() {
            StopAction::Accept
        } else {
            StopAction::Blocked(format!("Blocked tasks:\n{blocked}"))
        }
    }

    /// `unverified` names the applicable checks when this turn's edits have no passing check on the
    /// current revision, `None` when evidence is not owed. Without `can_remind`, or once the
    /// reminder was spent, the gap is disclosed instead of blocking the answer.
    pub fn evidence(&mut self, unverified: Option<&[String]>, can_remind: bool) -> EvidenceGate {
        let Some(checks) = unverified else {
            return EvidenceGate::Accept;
        };
        let applicable = checks.join(", ");
        if !can_remind || std::mem::replace(&mut self.evidence_reminded, true) {
            return EvidenceGate::Disclose(format!(
                "\n\nUnverified: no relevant check passed after the last edit (applicable: {applicable})."
            ));
        }
        EvidenceGate::Remind(format!(
            "[Dial] Files changed this turn, but no relevant check has passed on the current revision (applicable: {applicable}). Run the relevant check now. If it cannot run here, say so and why in the final answer. This reminder is sent once."
        ))
    }

    pub fn reset_for_steering(&mut self) {
        *self = Self::default();
    }
}

/// Plain reports receive actionable rejection feedback, just as explicit finish calls do.
/// This allowance is independent of max_steps, so unlimited work cannot loop on rejected finals.
#[derive(Default)]
pub(in crate::agent) struct CompletionRecovery {
    attempts: u8,
}

impl CompletionRecovery {
    pub fn retry(&mut self) -> bool {
        const ATTEMPTS: u8 = 3;
        if self.attempts >= ATTEMPTS {
            return false;
        }
        self.attempts += 1;
        true
    }
}

#[cfg(test)]
mod tests;
