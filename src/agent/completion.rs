//! Stop-time handling for open work. Text alone does not close a todo list; continuations are bounded
//! and need new tool evidence or a changed task list before another reminder is allowed.
use super::todo::{Status, TodoList};
use crate::llm::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

const MAX_REMINDERS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Blocked,
    NeedsInput,
    Incomplete,
    Interrupted,
}

pub struct Finish {
    pub outcome: TurnOutcome,
    pub text: String,
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
            .ok_or("finish requires a nonempty summary of at most 8192 bytes")?;
        let text = if outcome == TurnOutcome::Completed {
            summary.to_owned()
        } else {
            let reason = args["reason"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 8192)
                .ok_or("blocked/needs_input requires reason: the concrete missing prerequisite or question for the user")?;
            let label = if outcome == TurnOutcome::Blocked {
                "Blocked"
            } else {
                "Needs input"
            };
            format!("{label}: {summary}\n\n{reason}")
        };
        Ok(Self { outcome, text })
    }
}

pub fn schema() -> Value {
    json!({"type":"function","function":{
        "name":"finish",
        "description":"Request the end of the current turn. completed requires all todos to be completed or abandoned and no live tasks still running. A review-pending response continues the turn: inspect the supplied diff and requested evidence, then call finish again. blocked means a concrete missing prerequisite prevents further work; needs_input asks the user a specific question. Both require reason and leave todos open. Use as the last call in a batch. Never claim completed for promised or unverified work.",
        "parameters":{"type":"object","properties":{
            "outcome":{"type":"string","enum":["completed","blocked","needs_input"]},
            "summary":{"type":"string","minLength":1,"maxLength":8192},
            "reason":{"type":"string","minLength":1,"maxLength":8192}
        },"required":["outcome","summary"],"additionalProperties":false}
    }})
}

/// Every task still owed, blocked ones with their reason: what `finish completed` refuses to leave behind.
pub fn open_work(todo: &TodoList) -> String {
    lines(todo, Status::is_open)
}

/// Tasks the agent can act on now: pending or in progress.
fn actionable_work(todo: &TodoList) -> String {
    lines(todo, |status| {
        matches!(status, Status::Pending | Status::InProgress)
    })
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
    Continue(String),
    Blocked(String),
    Incomplete(String),
}

#[derive(Default)]
pub struct CompletionGuard {
    reminders: u32,
    progress: bool,
    evidence: HashSet<String>,
}

impl CompletionGuard {
    pub fn observe(&mut self, call: &ToolCall, succeeded: bool, todo_changed: bool) {
        if !succeeded {
            return;
        }
        if call.name == "todo" {
            self.progress |= todo_changed;
        } else if !matches!(
            call.name.as_str(),
            "finish" | "working_state" | "tool_search" | "tool_load"
        ) {
            // Repeating the same read is not new evidence and must not rearm the reminder loop.
            self.progress |= self
                .evidence
                .insert(format!("{}\0{}", call.name, call.arguments));
        }
    }

    pub fn stopped(&mut self, todo: &TodoList) -> StopAction {
        let work = actionable_work(todo);
        if work.is_empty() {
            let blocked = open_work(todo);
            return if blocked.is_empty() {
                StopAction::Accept
            } else {
                StopAction::Blocked(format!("Blocked tasks:\n{blocked}"))
            };
        }
        if self.reminders >= MAX_REMINDERS || (self.reminders > 0 && !self.progress) {
            return StopAction::Incomplete(format!(
                "Stopped with unfinished work: the model did not close its todos after bounded continuation.\n{work}\n\nContinue with a concrete next action, or report a blocker or question with finish."
            ));
        }
        self.reminders += 1;
        self.progress = false;
        StopAction::Continue(format!(
            "You stopped with unfinished todo items:\n{work}\n\nContinue the authorized work through tools and keep todos current. Mark tasks done only after doing and checking them. If a task is genuinely blocked, mark it with todo op=block and a concrete reason, then carry on with the rest; if waiting for user input, use finish (when offered) with outcome=needs_input and a concrete reason. Do not merely promise to continue. Reminder {}/{MAX_REMINDERS}.",
            self.reminders
        ))
    }

    pub fn reset_for_steering(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo() -> TodoList {
        let mut todo = TodoList::default();
        assert!(
            !todo
                .call(&json!({"op":"set","tasks":[{"content":"write code"}]}))
                .is_error
        );
        todo
    }

    fn read(path: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: json!({"path":path}),
            malformed_arguments: false,
        }
    }

    #[test]
    fn repeated_reads_and_failed_calls_do_not_rearm_continuations() {
        let todo = todo();
        let mut guard = CompletionGuard::default();
        assert!(matches!(guard.stopped(&todo), StopAction::Continue(_)));
        guard.observe(&read("a"), true, false);
        assert!(matches!(guard.stopped(&todo), StopAction::Continue(_)));
        guard.observe(&read("a"), true, false);
        guard.observe(&read("b"), false, false);
        assert!(matches!(guard.stopped(&todo), StopAction::Incomplete(_)));
    }

    #[test]
    fn progress_cannot_extend_the_reminder_budget_forever() {
        let todo = todo();
        let mut guard = CompletionGuard::default();
        for n in 0..MAX_REMINDERS {
            guard.observe(&read(&n.to_string()), true, false);
            assert!(matches!(guard.stopped(&todo), StopAction::Continue(_)));
        }
        guard.observe(&read("another"), true, false);
        assert!(matches!(guard.stopped(&todo), StopAction::Incomplete(_)));
    }

    #[test]
    fn todo_views_do_not_count_as_progress() {
        let mut guard = CompletionGuard::default();
        let todo = todo();
        assert!(matches!(guard.stopped(&todo), StopAction::Continue(_)));
        let call = ToolCall {
            name: "todo".into(),
            arguments: json!({"op":"view"}),
            ..read("a")
        };
        guard.observe(&call, true, false);
        assert!(matches!(guard.stopped(&todo), StopAction::Incomplete(_)));
        assert!(matches!(
            guard.stopped(&TodoList::default()),
            StopAction::Accept
        ));
    }

    #[test]
    fn stopping_with_only_blocked_tasks_reports_the_blockers() {
        let mut todo = todo();
        assert!(
            !todo
                .call(&json!({"op":"block","reason":"needs the staging password"}))
                .is_error
        );
        match CompletionGuard::default().stopped(&todo) {
            StopAction::Blocked(text) => assert!(
                text.contains("1. write code (blocked: needs the staging password)"),
                "{text}"
            ),
            _ => panic!("only blocked work remains"),
        }
        assert!(
            open_work(&todo).contains("write code"),
            "finish completed must still refuse blocked work"
        );
    }

    #[test]
    fn explicit_outcomes_require_concrete_nonempty_details() {
        for outcome in ["blocked", "needs_input"] {
            assert!(Finish::parse(&json!({"outcome":outcome,"summary":"Need help"})).is_err());
            assert!(
                Finish::parse(&json!({"outcome":outcome,"summary":"Need help","reason":" "}))
                    .is_err()
            );
            assert!(Finish::parse(&json!({"outcome":outcome,"summary":"Need help","reason":"Which output directory?"})).is_ok());
        }
        assert!(Finish::parse(&json!({"outcome":"completed","summary":" "})).is_err());
        assert!(Finish::parse(&json!({"outcome":"unknown","summary":"Done"})).is_err());
    }
}
