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
        "description":"End the request with an explicit outcome; a plain final reply works the same way. completed: the requested work is done; first finish or block planned steps and deliverables, wait for running jobs, and get required checks passing. answered: you are delivering information, not claiming changes are done. blocked or needs_input: put the concrete missing prerequisite or question in reason, after checking what the workspace can tell you; needs_input may offer up to 8 options. Call it last in a batch.",
        "parameters":{"type":"object","properties":{
            "outcome":{"type":"string","enum":["completed","answered","blocked","needs_input"]},
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

/// A bounded, separate notice, never a rewrite of the answer or a declaration of success.
pub(in crate::agent) fn outstanding_notice(
    todo: &TodoList,
    working: &Value,
    outcome: TurnOutcome,
) -> Option<String> {
    let steps = todo
        .tasks()
        .iter()
        .filter(|task| {
            task.status.is_open() && task.kind == crate::agent::workflow::todo::Kind::Step
        })
        .count();
    let follow_ups = todo
        .tasks()
        .iter()
        .filter(|task| {
            task.status.is_open() && task.kind == crate::agent::workflow::todo::Kind::FollowUp
        })
        .count();
    let deliverables = crate::agent::workflow::working_state::open_deliverables(working);
    if steps == 0 && follow_ups == 0 && deliverables.is_empty() {
        return None;
    }
    let mut notice = format!(
        "Outstanding work retained: {steps} execution step(s), {} required deliverable(s), {follow_ups} optional follow-up(s).",
        deliverables.len()
    );
    if outcome == TurnOutcome::Answered {
        notice.push_str(" This answer does not mark that work complete.");
    }
    for (index, task) in todo
        .tasks()
        .iter()
        .enumerate()
        .filter(|(_, task)| task.status.is_open())
        .take(3)
    {
        notice.push_str(&format!(
            "\n{}",
            crate::agent::base::helpers::byte_prefix(&task.label(index + 1), 300)
        ));
    }
    Some(notice)
}

/// A candidate is reconciled with controller observations before either delivery surface ends.
pub(in crate::agent) enum Decision {
    Accept(TurnOutcome),
    Continue(String),
    Incomplete(String),
}

impl crate::agent::Agent<'_> {
    /// Small local models keep every completion guard rail: open execution steps block, the
    /// lexical investigation nudge applies and long observation runs get the forced research
    /// checkpoint. Hosted and full-tier local routes rely on the state gate alone.
    pub(in crate::agent) fn lean_completion_route(&self) -> bool {
        self.lean_local() || self.local_escalated()
    }

    /// Whether an accepted `completed` answer closes open execution steps (`settle_open_steps`),
    /// so the todo focus need not ask for a closing `todo done`.
    pub(in crate::agent) fn finish_closes_steps(&self) -> bool {
        !(self.hermetic || self.plan_mode || self.lean_completion_route())
    }

    /// One instruction-scoped nudge for missing work or observations. Wording never grants
    /// completion, and rebuilding an inner loop cannot replenish this allowance.
    pub(in crate::agent) fn completion_decision(
        &mut self,
        requested: TurnOutcome,
        candidate: &str,
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
        let delivery = matches!(requested, TurnOutcome::Completed | TurnOutcome::Answered);
        let open_steps = self
            .todo
            .tasks()
            .iter()
            .filter(|task| {
                task.kind == crate::agent::workflow::todo::Kind::Step && task.status.is_open()
            })
            .map(|task| task.content.as_str())
            .collect::<Vec<_>>();
        let has_open_steps = !open_steps.is_empty();
        // Lean routes refuse a final reply over open steps. Other routes settle the remaining
        // blockers first and, once the answer is accepted, close or list the steps (`settle_open_steps`).
        let open = (lean && has_open_steps).then(|| format!(
            "Execution steps remain open: {}. Complete or explicitly block them; a final reply does not close them.",
            open_steps.join("; ")));
        let mut blocker = if delivery {
            open.or_else(|| self.validate_completion_scoped(ui, lean).err())
        } else {
            None
        };
        let hard_blocker = delivery
            && blocker.is_some()
            && !has_open_steps
            && crate::agent::workflow::working_state::open_deliverables(&control["working_state"])
                .is_empty();
        if delivery && blocker.is_none() {
            let gate = self.gate_unverified_edits(!nudged, ui);
            if gate.unverified.is_some() || gate.blocking {
                let lead = match &gate.unverified {
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
            }
        }
        // The lexical investigation nudge is a guard rail for small local models only.
        let missing_observation = lean && {
            // Inspecting the directory is only an availability check. It supplies no task evidence.
            let workspace_available =
                std::fs::read_dir(self.workspace.root())
                    .ok()
                    .is_some_and(|entries| {
                        entries.filter_map(Result::ok).any(|entry| {
                            !matches!(entry.file_name().to_str(), Some(".git" | ".dial" | ".tmp"))
                        })
                    });
            let work_request = control["current_instruction"]
                .as_str()
                .is_some_and(|prompt| {
                    crate::learn::task_policy::Kind::classify(prompt)
                        != crate::learn::task_policy::Kind::General
                });
            !self.step_checks.autonomous_continuation
                && self.step_checks.observations == 0
                && workspace_available
                && work_request
                && !self.interrupt.load(std::sync::atomic::Ordering::SeqCst)
        };
        if requested == TurnOutcome::NeedsInput && self.step_checks.observations == 0 {
            self.efficiency_metrics.questions_before_investigation += 1;
        }
        let investigation_only = blocker.is_none() && missing_observation;
        let missing = blocker.or_else(|| missing_observation.then(||
            "No investigation has been observed for this request. Inspect available workspace evidence before asking the user or claiming completion; if it cannot answer the question, state the concrete missing information.".to_owned()));
        let Some(reason) = missing else {
            return self.accept_completion(requested, lean, ui);
        };
        let externally_blocked = self.todo.blocked_steps().next().is_some()
            || control["working_state"]["deliverables"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["status"] == "blocked"));
        let unfinished = crate::agent::safety::stream_guards::announces_unexpected_stop(candidate)
            || crate::agent::safety::stream_guards::reports_unfinished_work(candidate);
        if nudged || hard_blocker || externally_blocked || self.stop_boundary().is_some() {
            // The keyword-classified investigation nudge advises; it never ends a turn Incomplete
            // on its own. A candidate that itself says the work is unfinished is not an answer.
            if investigation_only && !unfinished {
                return self.accept_completion(requested, lean, ui);
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

    /// Accept `requested`, disclosing what the answer leaves unverified. On non-lean routes a
    /// delivered answer also settles open execution steps (`settle_open_steps`).
    fn accept_completion(
        &mut self,
        requested: TurnOutcome,
        lean: bool,
        ui: &mut dyn crate::agent::Ui,
    ) -> Decision {
        if !matches!(requested, TurnOutcome::Completed | TurnOutcome::Answered) {
            return Decision::Accept(requested);
        }
        if !lean {
            self.settle_open_steps(requested, ui);
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

    /// Non-lean routes only: an accepted `completed` answer closes every pending or in-progress
    /// execution step, so the model spends no request on closing bookkeeping. Blocked steps never
    /// reach here (they refuse completion) and optional follow-ups stay listed by `outstanding_notice`.
    fn settle_open_steps(&mut self, requested: TurnOutcome, ui: &mut dyn crate::agent::Ui) {
        if requested != TurnOutcome::Completed {
            return;
        }
        let mut next = self.todo.clone();
        let closed = next.close_open_steps();
        if closed.is_empty() {
            return;
        }
        let mut state = self.session.control_state();
        state["todo"] = next.snapshot();
        if let Err(error) = self.session.set_control_state(state) {
            ui.diagnostic(&format!("could not persist the closed steps: {error:#}"));
            return;
        }
        self.todo = next;
        ui.todo(&self.todo);
        ui.notice(&format!(
            "Closed {} open step(s) with the accepted completion: {}",
            closed.len(),
            closed.join("; ")
        ));
    }
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

/// Whether an answer says a capability is unavailable to the agent itself ("I cannot provide an
/// interactive terminal", "a browser is not available to me"): one sentence pairs a first-person
/// subject or object with a denial and a capability noun, so prose about the project ("this env
/// var is not available in tests") does not match. Code is ignored. A heuristic for one search
/// reminder per turn, never a reason to reject an answer outright.
pub(in crate::agent) fn denies_capability(text: &str) -> bool {
    const FIRST_PERSON: &[&str] = &["i", "i'm", "im", "i've", "me", "my", "myself"];
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
        .map(|sentence| sentence.to_lowercase().replace('\u{2019}', "'"))
        .any(|sentence| {
            sentence
                .split(|c: char| !c.is_alphanumeric() && c != '\'')
                .any(|word| FIRST_PERSON.contains(&word))
                && DENIALS.iter().any(|denial| sentence.contains(denial))
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
pub(in crate::agent) const CAPABILITY_SEARCH_REMINDER: &str = "Your answer says something is not possible or not available, but you have not searched the tool catalog this turn. Tools you were not offered may provide it (an interactive terminal, a browser, a debugger and more are loadable). Call `tool_search` with the capability you need and load:true, or names:[exact match], then use it; if nothing fits, give the same answer again and name what you searched for.";

/// Actual runtime check callbacks, not assertion words in source or printed claims. These are
/// local check outcomes, never evidence that unrelated files or task requirements passed.
pub(in crate::agent) fn eval_checks(
    output: &crate::runtime::tool_output::ToolOutput,
) -> impl Iterator<Item = bool> {
    output
        .data
        .as_ref()
        .and_then(|data| data["checks"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|check| check["passed"].as_bool())
}

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
