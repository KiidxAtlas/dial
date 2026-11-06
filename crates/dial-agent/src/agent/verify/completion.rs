//! Stop-time handling for open work and missing evidence. Text alone never closes pending todos:
//! an accepted completed answer closes only the in-progress task (see
//! `TodoList::complete_reported_work`), and not in goal mode. Tests are never mandatory: edits with
//! no passing check are accepted with the gap disclosed; only a failure the gate itself observed
//! earns one reminder per round.
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
        "description":"Optional structured outcome; ordinary work ends with a plain final answer instead. completed reports finished, verified work and requires no live tasks; it closes only the in-progress todo. A review-pending response continues the turn: inspect the supplied diff and evidence, then call finish again. blocked names a concrete missing prerequisite. needs_input asks a specific question (optional 1–8 options) and continues the turn with the answer. blocked and needs_input require reason. Use as the last call in a batch.",
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

/// Whether an answer says a capability is unavailable to the agent ("I cannot provide an
/// interactive terminal", "a browser is not available in this environment"): one sentence pairs a
/// denial with a capability noun. Code is ignored. A heuristic for one search reminder per turn,
/// never a reason to reject an answer outright.
pub(in crate::agent) fn denies_capability(text: &str) -> bool {
    const DENIALS: &[&str] = &[
        "cannot",
        "can't",
        "can not",
        "unable to",
        "not available",
        "isn't available",
        "don't have",
        "do not have",
        "no access",
        "not possible",
        "isn't possible",
        "not supported",
        "lack ",
    ];
    const CAPABILITIES: &[&str] = &[
        "tool",
        "environment",
        "terminal",
        "tty",
        "access to",
        "capabilit",
        "ability",
        "interactive",
        "browser",
        "internet",
        "network",
        "display",
        "screen",
        "gui",
    ];
    text.split("```")
        .step_by(2)
        .flat_map(|prose| prose.split(['.', '!', '?', '\n']))
        .map(str::to_lowercase)
        .any(|sentence| {
            DENIALS.iter().any(|denial| sentence.contains(denial))
                && CAPABILITIES
                    .iter()
                    .any(|capability| sentence.contains(capability))
        })
}

/// One reminder per instruction to search the tool catalog before an answer says a capability is
/// unavailable. Spent once the model searched or loaded tools itself, or after one reminder.
#[derive(Default)]
pub(in crate::agent) struct CapabilitySearch {
    searched: bool,
    reminded: bool,
}

impl CapabilitySearch {
    /// The model called `tool_search` or `tool_load` during this instruction.
    pub(in crate::agent) fn searched(&mut self) {
        self.searched = true;
    }

    /// Whether `answer` earns the reminder: it denies a capability, unoffered tools exist, and
    /// neither a search nor an earlier reminder happened. Spends the reminder when it does.
    pub(in crate::agent) fn remind(&mut self, answer: &str, loadable_tools: bool) -> bool {
        if self.searched || self.reminded || !loadable_tools || !denies_capability(answer) {
            return false;
        }
        self.reminded = true;
        true
    }
}

/// The reminder `CapabilitySearch::remind` sends back with the rejected answer.
pub(in crate::agent) const CAPABILITY_SEARCH_REMINDER: &str = "Your answer says something is not possible or not available, but you have not searched the tool catalog this turn. Tools you were not offered may provide it (an interactive terminal, a browser, a debugger and more are loadable). Call `tool_search` with the capability you need, load a match with `tool_load` and use it; if nothing fits, give the same answer again and name what you searched for.";

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

    /// Whether this round's evidence reminder was already sent.
    pub fn reminder_spent(&self) -> bool {
        self.evidence_reminded
    }

    /// `unverified` names the applicable checks when this turn's edits have no passing check on the
    /// current revision, `None` when evidence is not owed. `failures` is what the gate's own run of
    /// those checks observed. A missing check is disclosed, never demanded; only an observed failure
    /// earns one reminder (carrying its output), and without `can_remind` or once spent it is
    /// disclosed too.
    pub fn evidence(
        &mut self,
        unverified: Option<&[String]>,
        failures: &[String],
        can_remind: bool,
    ) -> EvidenceGate {
        let Some(checks) = unverified else {
            return EvidenceGate::Accept;
        };
        let applicable = checks.join(", ");
        if failures.is_empty()
            || !can_remind
            || std::mem::replace(&mut self.evidence_reminded, true)
        {
            return EvidenceGate::Disclose(format!(
                "\n\nUnverified: no relevant check passed after the last edit (applicable: {applicable})."
            ));
        }
        EvidenceGate::Remind(format!(
            "[Dial] The completion gate ran the applicable checks after this turn's edits and they failed (applicable: {applicable}). Fix the failure, or explain it in the final answer. This reminder is sent once.\nThe completion gate ran the applicable checks:\n{}",
            failures.join("\n")
        ))
    }

    pub fn reset_for_steering(&mut self) {
        *self = Self::default();
    }
}

/// A successful `eval` cell whose code asserts its results is verification the model already ran:
/// a failed assertion makes the cell an error. It counts like a passing shell check.
pub(in crate::agent) fn asserting_eval(code: &str) -> bool {
    code.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word == "assert")
        || code.contains("assert.")
        || code.contains("assert(")
}

/// Preserve a candidate as unaccepted work alongside the controller's exact missing prerequisite.
/// This is a terminal report for the user, never feedback injected into another model request.
pub(in crate::agent) fn rejected_report(
    candidate: &str,
    reason: &str,
    outcome: crate::agent::TurnOutcome,
) -> String {
    let label = if outcome == crate::agent::TurnOutcome::Blocked {
        "Blocked"
    } else {
        "Incomplete"
    };
    format!(
        "{label}: completion requirements were not met. No completion was recorded.\n\nCandidate summary (not accepted as completed):\n{candidate}\n\nUnmet prerequisite:\n{reason}"
    )
}

#[cfg(test)]
mod tests;
