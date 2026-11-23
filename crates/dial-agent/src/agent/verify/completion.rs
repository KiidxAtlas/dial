//! Shared candidate reconciliation against observed work, with one durable recovery allowance.
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
            Some("answered") => TurnOutcome::Answered,
            Some("needs_input") => TurnOutcome::NeedsInput,
            _ => {
                return Err(
                    "finish outcome must be completed, answered, blocked or needs_input".into(),
                );
            }
        };
        let summary = args["summary"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 8192)
            .ok_or("finish requires a nonempty summary of at most 8192 bytes")?
            .to_owned();
        let question = if matches!(outcome, TurnOutcome::Completed | TurnOutcome::Answered) {
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
        let text = if matches!(outcome, TurnOutcome::Completed | TurnOutcome::Answered) {
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
        "description":"End the request; call last. `summary` is the whole reply the user sees, not a log of your steps: for answered, the answer or findings themselves; for completed, what changed and how it was checked. A plain final reply works the same. completed: work and steps done, jobs finished, checks passing. answered: information only. blocked: name the missing prerequisite in reason. needs_input: ask the unresolved question in reason; optionally give 1–8 options.",
        "parameters":{"type":"object","properties":{
            "outcome":{"type":"string","enum":["completed","answered","blocked","needs_input"]},
            "summary":{"type":"string","minLength":1,"maxLength":8192,"description":"The user-facing reply itself: the answer or findings, not a recap of looking them up."},
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

/// A bounded, separate notice, never a rewrite of the answer or a declaration of success.
pub(in crate::agent) fn outstanding_notice(
    todo: &TodoList,
    working: &Value,
    outcome: TurnOutcome,
) -> Option<String> {
    crate::agent::workflow::task_ledger::TaskLedger::outstanding_notice(todo, working, outcome)
}
/// A candidate is reconciled with controller observations before either delivery surface ends.
pub(in crate::agent) enum Decision {
    Accept(TurnOutcome),
    Continue(String),
    Incomplete(String),
}

pub(in crate::agent) const UNCHECKED_AFTER_LAUNCH_GAP: &str = "A validation command exited 127, and the current edits have no passing check. Test the saved artifact through its intended consumer with positive and negative cases (bash check:true or verify acceptance). For standalone files, an Eval cell can bind assertions to each saved file with dial.check(name, passed, dial.read receipt). A different engine gives only partial coverage; disclose an unavailable consumer.";

/// Nudge for a plain reply that asks the user a question while `finish` is offered: the typed pause is
/// the supported protocol, so the model is asked to use it once before the question is accepted.
pub(in crate::agent) const TYPED_PAUSE_NUDGE: &str = "This reply asks the user a question. Pause through the typed protocol instead: call finish with outcome=needs_input, put the question in reason, and offer up to 8 options when the choices are known. Do not end the reply with a plain question.";

/// How a completion candidate reached the controller. Only plain replies may pause through the
/// clarification path; explicit calls carry their own outcome.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum ReplyChannel {
    /// A `finish` call, a goal completion or a side-question check: the outcome is explicit.
    Explicit,
    /// A plain final reply. `signals` is classified once before any background wait; `typed_pause`
    /// says whether `finish` is offered on this turn.
    Plain {
        typed_pause: bool,
        asks_user: bool,
        signals: Option<crate::agent::verify::reply_signals::ReplySignals>,
    },
}

/// Require an observation only for a direct instruction to investigate the current workspace.
/// A failure noun, an explanation request, or a quoted example isn't such an instruction.
/// Ambiguous requests keep the ordinary advisory nudge, not a new delivery obligation.
fn requests_workspace_investigation(instruction: &str) -> bool {
    // A snippet, diagnostic or other supplied context can answer the request without a tool.
    // Only promote a bare imperative, not a request accompanied by an example or quotation.
    if instruction.trim().contains(['`', '"', ':', '\n']) {
        return false;
    }
    let lower = instruction.to_ascii_lowercase();
    let mut words = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty());
    let verb = words.by_ref().find(|word| *word != "please");
    if !matches!(verb, Some("diagnose" | "investigate" | "debug" | "inspect")) {
        return false;
    }
    let mut local = false;
    for word in words {
        if local
            && matches!(
                word,
                "project" | "workspace" | "repo" | "repository" | "codebase"
            )
        {
            return true;
        }
        local = matches!(word, "this" | "current");
    }
    false
}

impl crate::agent::Agent<'_> {
    /// Lean routes add investigation guidance. Execution obligations apply on every route.
    pub(in crate::agent) fn lean_completion_route(&self) -> bool {
        self.lean_local() || self.local_escalated()
    }

    /// One instruction-scoped nudge for missing work or observations. Wording never grants
    /// completion, and rebuilding an inner loop cannot replenish this allowance.
    pub(in crate::agent) fn completion_decision(
        &mut self,
        requested: TurnOutcome,
        channel: ReplyChannel,
        ui: &mut dyn crate::agent::Ui,
    ) -> Decision {
        if self.hermetic || self.plan_mode {
            return match self.validate_completion(ui) {
                Err(reason)
                    if matches!(requested, TurnOutcome::Completed | TurnOutcome::Answered) =>
                {
                    Decision::Incomplete(reason)
                }
                _ => Decision::Accept(requested),
            };
        }
        let lean = self.lean_completion_route();
        let mut control = self.session.control_state();
        let instruction = control["current_instruction_entry_id"].clone();
        let nudged = control["completion_gate"]["instruction"] == instruction
            && control["completion_gate"]["nudged"] == true;
        // A plain question for the user is a clarification. Where `finish` is offered, one nudge asks
        // for the typed pause; otherwise, or after that nudge, the question stands as needs_input. The
        // lean investigation nudge still runs first when no observation has been made.
        if let ReplyChannel::Plain {
            typed_pause,
            asks_user: true,
            ..
        } = channel
            && requested == TurnOutcome::NeedsInput
            && typed_pause
            && !nudged
            && !(lean && self.step_checks.observations == 0)
        {
            control["completion_gate"] =
                json!({"instruction":instruction,"nudged":true,"reason":TYPED_PAUSE_NUDGE});
            if let Err(error) = self.session.set_control_state(control) {
                return Decision::Incomplete(format!(
                    "Completion recovery could not be recorded: {error:#}"
                ));
            }
            self.efficiency_metrics.completion_nudges += 1;
            ui.notice(&format!(
                "Completion deferred for a plain question instead of finish(needs_input): {TYPED_PAUSE_NUDGE}"
            ));
            return Decision::Continue(format!("\n[Dial completion] {TYPED_PAUSE_NUDGE}"));
        }
        let delivery = matches!(requested, TurnOutcome::Completed | TurnOutcome::Answered);
        let obligations =
            crate::agent::workflow::task_ledger::TaskLedger::obligations(&self.todo, &control);
        let has_open_steps = obligations.has_open_steps();
        let has_open_deliverables = obligations.has_open_deliverables();
        let externally_blocked = obligations.has_external_blocker();
        // shared boundary reports them (keeping each blocked step's recorded reason).
        let mut blocker = if delivery {
            self.validate_completion(ui).err()
        } else {
            None
        };
        let hard_blocker =
            delivery && blocker.is_some() && !has_open_steps && !has_open_deliverables;
        if delivery && blocker.is_none() {
            let gate = self.gate_unverified_edits(!nudged, ui);
            if gate.unverified.is_some() || gate.blocking {
                let lead = match &gate.unverified {
                    Some(checks) if gate.has_pass && gate.blocking => format!(
                        "Current edits have a failing applicable check. Still lacking a pass: {}. ",
                        checks.join(", ")
                    ),
                    Some(checks) if gate.has_pass => format!(
                        "Current edits still lack passing evidence for: {}. ",
                        checks.join(", ")
                    ),
                    Some(checks) => format!(
                        "Current edits have no passing applicable check: {}. ",
                        checks.join(", ")
                    ),
                    None => "Current edits have a failing applicable check. ".to_owned(),
                };
                let detail = if gate.failures.is_empty() && nudged {
                    control["completion_gate"]["reason"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                } else {
                    gate.failures.join("\n")
                };
                blocker = Some(format!("{lead}{detail}").trim_end().to_owned());
            } else if let Some(reminder) = self.artifact_check_reminder() {
                blocker = Some(reminder.to_owned());
            } else if let Some(gap) = self.unattended_verification_gap() {
                blocker = Some(gap.to_owned());
            }
        }
        // Neither an honest blocker nor an explicit request for input claims investigation.
        let plain_question = matches!(
            channel,
            ReplyChannel::Plain {
                asks_user: true,
                ..
            }
        );
        let prompt = control["current_instruction"].as_str().unwrap_or_default();
        let missing_observation = lean
            && (delivery || plain_question)
            && !self.step_checks.autonomous_continuation
            && self.step_checks.observations == 0
            && !self.interrupt.load(std::sync::atomic::Ordering::SeqCst)
            && crate::learn::task_policy::Kind::classify(prompt)
                != crate::learn::task_policy::Kind::General
            && {
                // Directory availability supplies no task evidence.
                std::fs::read_dir(self.workspace.root())
                    .ok()
                    .is_some_and(|entries| {
                        entries.filter_map(Result::ok).any(|entry| {
                            !matches!(entry.file_name().to_str(), Some(".git" | ".dial" | ".tmp"))
                        })
                    })
            };
        if requested == TurnOutcome::NeedsInput && self.step_checks.observations == 0 {
            self.efficiency_metrics.questions_before_investigation += 1;
        }
        let investigation_only = blocker.is_none() && missing_observation;
        let missing = blocker.or_else(|| missing_observation.then(||
            "No investigation has been observed for this request. Inspect available workspace evidence before asking the user or claiming completion; if it cannot answer the question, state the concrete missing information.".to_owned()));
        let Some(reason) = missing else {
            return self.accept_completion(requested, ui);
        };
        // Closing-line hints only guide recovery once an independent gap exists. In particular,
        // an explicit finish decision never inspects the summary to invent or clear a blocker.
        let unfinished = matches!(channel, ReplyChannel::Plain { signals: Some(s), .. } if s.unfinished_closing());
        if nudged || hard_blocker || externally_blocked || self.stop_boundary().is_some() {
            // Ambiguous work gets advice, not a hard tool requirement. For a direct workspace
            // investigation, neither prose nor a typed finish replaces the missing observation.
            if investigation_only
                && !unfinished
                && !(delivery && requests_workspace_investigation(prompt))
            {
                return self.accept_completion(requested, ui);
            }
            self.efficiency_metrics.incomplete_stops += 1;
            // A later blocker can preempt the reminder's own check run; keep its observed output.
            let reminded = control["completion_gate"]["reason"]
                .as_str()
                .filter(|earlier| nudged && !earlier.is_empty() && *earlier != reason);
            return Decision::Incomplete(match reminded {
                Some(earlier) => format!("{reason}\nEarlier completion reminder: {earlier}"),
                None => reason,
            });
        }
        control = self.session.control_state();
        control["completion_gate"] =
            json!({"instruction":instruction,"nudged":true,"reason":reason});
        if let Err(error) = self.session.set_control_state(control) {
            return Decision::Incomplete(format!(
                "Completion recovery could not be recorded: {error:#}"
            ));
        }
        self.efficiency_metrics.completion_nudges += 1;
        ui.notice(&format!(
            "Completion deferred for unfinished work or investigation: {reason}"
        ));
        let wording = if unfinished {
            crate::agent::safety::stream_guards::UNEXPECTED_STOP_RETRY.to_owned()
        } else {
            String::new()
        };
        Decision::Continue(format!(
            "{wording}\n[Dial completion] {reason}\nContinue the permitted work or report a concrete blocker. This is the single completion nudge for this request."
        ))
    }

    /// Accept `requested`, disclosing what the answer leaves unverified. Accepted steps are never
    /// closed implicitly: open execution steps refused the delivery before this point.
    fn accept_completion(
        &mut self,
        requested: TurnOutcome,
        ui: &mut dyn crate::agent::Ui,
    ) -> Decision {
        if !matches!(requested, TurnOutcome::Completed | TurnOutcome::Answered) {
            return Decision::Accept(requested);
        }
        let gaps = self.disclosed_gaps();
        if !gaps.is_empty() {
            ui.notice(&format!(
                "Accepted with disclosed gaps (not verified, not caused by this turn's edits):\n{}",
                gaps.join("\n")
            ));
        }
        Decision::Accept(requested)
    }
}

#[cfg(test)]
pub(in crate::agent) use crate::agent::verify::reply_signals::awaiting_input;
pub(in crate::agent) use crate::agent::verify::reply_signals::{
    declared_outcome, denies_capability,
};
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
pub(in crate::agent) const CAPABILITY_SEARCH_REMINDER: &str = "Your answer says something is not possible or not available, but you have not searched the tool catalog this turn. Tools you were not offered may provide it (an interactive terminal, a browser, a debugger and more are loadable). Call `tool_search` with the capability you need and load:true, or names:[exact match], then use it; if nothing fits, give the same answer again and name what you searched for.";

/// Preserve a candidate as unaccepted work alongside the controller's missing prerequisite.
/// This is a terminal report for the user, never feedback injected into another model request,
/// so it states the status first and keeps only the user-relevant part of the reason.
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
    format!("{label}: {}\n\n{}", user_reason(reason), candidate.trim())
}

/// Most characters of the reason a rejected report shows.
const USER_REASON_CHARS: usize = 600;

/// The controller reason without its model-directed recovery instructions: the first sentence of
/// each statement, blocking check names and the first compiler/test error with its location.
/// Repeated reminders and full diagnostics stay in the session record.
fn user_reason(reason: &str) -> String {
    let main = reason
        .split("\nEarlier completion reminder:")
        .next()
        .unwrap_or(reason);
    let mut parts: Vec<String> = Vec::new();
    for line in main.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("Recovery call:") || is_diagnostic(line) {
            continue;
        }
        let line = line.strip_prefix("Cannot complete: ").unwrap_or(line);
        let sentence = first_sentence(line);
        if !parts.iter().any(|part| part == sentence) {
            parts.push(sentence.to_owned());
        }
    }
    let lines: Vec<&str> = reason.lines().map(str::trim).collect();
    let compiler = lines
        .iter()
        .position(|line| line.starts_with("error[") || line.starts_with("error:"));
    // Without a compiler error, the first line a failed check printed after its command.
    let output = || {
        let command = lines.iter().position(|line| line.starts_with("Command:"))?;
        lines[command + 1..]
            .iter()
            .position(|line| !line.is_empty() && !line.starts_with('['))
            .map(|offset| command + 1 + offset)
    };
    if let Some(at) = compiler.or_else(output) {
        let location = lines[at + 1..]
            .iter()
            .take(2)
            .find_map(|line| line.strip_prefix("--> "));
        parts.push(match location {
            Some(location) => format!("First error: {} ({location})", lines[at]),
            None => format!("First error: {}", lines[at]),
        });
    }
    let text = parts.join(" ");
    if text.chars().count() <= USER_REASON_CHARS {
        return text;
    }
    let cut: String = text.chars().take(USER_REASON_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Compiler and test output lines, which the first-error summary represents.
fn is_diagnostic(line: &str) -> bool {
    line.starts_with("error")
        || line.starts_with("warning")
        || line.starts_with("-->")
        || line.starts_with('|')
        || line.starts_with('[')
        || line.starts_with("Command:")
        || line.starts_with("For more information")
        || line.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Text up to and including the first `. ` sentence break, or the whole line.
fn first_sentence(line: &str) -> &str {
    line.find(". ").map_or(line, |end| &line[..=end])
}

#[cfg(test)]
mod tests;
