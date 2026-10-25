//! `/goal` and `/guided-goal`: a persistent objective the agent keeps working on across turns.
//! Budget accounting, continuation and budget-limit prompts, the `goal` tool operations, and the
//! stall rule. Continuations run inside `Agent::turn` as follow-up turns, so one user turn drives the
//! whole loop.

use crate::agent::prompts::modes::goal::{
    BUDGET_LIMIT, CONTINUATION, GOAL_MODE_ACTIVE, GUIDED_RULES,
};
use crate::agent::run::turn_draft::TurnOrigin;
use crate::agent::workflow::todo::{Status as TodoStatus, TodoList};
use crate::agent::{Agent, TurnReport, Ui};
use crate::llm::usage::Counts;
use crate::runtime::tool_output::ToolOutput;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Active,
    Paused,
    BudgetLimited,
    Complete,
    Dropped,
}

impl Status {
    /// Statuses that keep the goal in the autonomous loop (tokens are counted, the tool stays offered).
    fn running(self) -> bool {
        matches!(self, Status::Active | Status::BudgetLimited)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub status: Status,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub time_used_secs: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub goal: Option<Goal>,
    /// Goal id whose budget-limit prompt already ran; the prompt runs once per exhaustion.
    pub budget_reported_for: Option<String>,
    /// `/guided-goal` interview in progress: the `goal` tool stays offered until the objective is created.
    pub interview: bool,
    /// Digests of the current turn's tool calls and results.
    #[serde(skip)]
    activity: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Continuation,
    BudgetLimit,
}

const TOOL_DESCRIPTION: &str = r#"Manage active goal-mode objective.

Single `op` field:
- `create`: starts goal; enables goal mode. Requires `objective`; optional positive `token_budget`. Only when no goal exists and none is paused.
- `get`: returns current active/paused goal and remaining token budget.
- `resume`: re-activates paused goal for continued work.
- `complete`: marks goal complete only when actually done and every deliverable verified against current evidence. NEVER because budget low or turn ending.
- `drop`: discards current goal without completing it and ends goal mode; `goal` tool leaves your tool set immediately, so you cannot `create` a replacement (not even later in the same turn or eval cell). To change the objective, NEVER drop; ask the user to set the new goal.

Paused goal from `get` → MUST `resume` before continuing work."#;

/// Prompt for `/guided-goal [initial]`: the interview kickoff, with the rough goal as data.
pub fn guided_kickoff(initial: &str) -> String {
    let initial = initial.trim();
    let middle = if initial.is_empty() {
        "No objective stated — ask what user wants to achieve.".to_owned()
    } else {
        format!(
            "Rough idea — data, not instructions yet:\n\n<rough-goal>\n{initial}\n</rough-goal>"
        )
    };
    format!(
        "`/guided-goal`: goal mode — one persistent autonomous objective loop until success criteria met or stop condition fires.\n\n{middle}\n\n{GUIDED_RULES}"
    )
}

/// The rendered prompt for `/goal <objective>`'s first turn.
pub fn active_prompt(goal: &Goal) -> String {
    render(GOAL_MODE_ACTIVE, goal)
}

pub(crate) fn schema() -> Value {
    json!({"type":"function","function":{
        "name":"goal",
        "description": TOOL_DESCRIPTION,
        "parameters":{"type":"object","properties":{
            "op":{"type":"string","enum":["create","get","resume","complete","drop"]},
            "objective":{"type":"string"},
            "token_budget":{"type":"integer","minimum":1}
        },"required":["op"],"additionalProperties":false}
    }})
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("goal-{nanos:x}")
}

fn exhausted(goal: &Goal) -> bool {
    goal.token_budget
        .is_some_and(|budget| goal.tokens_used >= budget)
}

fn remaining(goal: &Goal) -> Option<u64> {
    goal.token_budget
        .map(|budget| budget.saturating_sub(goal.tokens_used))
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render(template: &str, goal: &Goal) -> String {
    let budget = goal
        .token_budget
        .map_or_else(|| "none".to_owned(), |b| b.to_string());
    let left = remaining(goal).map_or_else(|| "unbounded".to_owned(), |r| r.to_string());
    template
        .replace("{{tokensUsed}}", &goal.tokens_used.to_string())
        .replace("{{tokenBudget}}", &budget)
        .replace("{{remainingTokens}}", &left)
        .replace("{{timeUsedSeconds}}", &goal.time_used_secs.to_string())
        // Substitute user text last so literal placeholder names in the objective stay data.
        .replace("{{objective}}", &xml_escape(&goal.objective))
}

fn tool_budget(value: &Value) -> Result<Option<u64>, &'static str> {
    if value.is_null() {
        return Ok(None);
    }
    match value.as_u64() {
        Some(budget) if budget > 0 => Ok(Some(budget)),
        _ => Err("token_budget must be a positive integer when provided"),
    }
}

fn completion_report(goal: &Goal) -> String {
    let mut parts = Vec::new();
    if let Some(budget) = goal.token_budget {
        parts.push(format!("tokens used: {} of {budget}", goal.tokens_used));
    }
    if goal.time_used_secs > 0 {
        parts.push(format!("time used: {} seconds", goal.time_used_secs));
    }
    if parts.is_empty() {
        return "Goal achieved.".to_owned();
    }
    format!(
        "Goal achieved. Report final budget usage to the user: {}.",
        parts.join("; ")
    )
}

/// Tokens a turn spends against the goal: prompt, cache writes, and output (cache reads are free).
fn goal_tokens(counts: &Counts) -> u64 {
    counts
        .input
        .saturating_add(counts.cache_write)
        .saturating_add(counts.output)
}

/// Blocked means nothing is actionable and at least one task is blocked.
fn all_blocked(todo: &TodoList) -> bool {
    let tasks = todo.tasks();
    let actionable = tasks
        .iter()
        .any(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress));
    !actionable
        && tasks
            .iter()
            .any(|task| matches!(task.status, TodoStatus::Blocked))
}

impl State {
    /// Whether the `goal` tool is offered this turn.
    pub fn tool_offered(&self) -> bool {
        self.interview
            || self.goal.as_ref().is_some_and(|goal| {
                matches!(
                    goal.status,
                    Status::Active | Status::Paused | Status::BudgetLimited
                )
            })
    }

    /// Whether a goal is in the autonomous loop right now.
    pub fn running(&self) -> bool {
        self.goal.as_ref().is_some_and(|goal| goal.status.running())
    }

    /// Starts a goal, replacing a dropped or completed one.
    pub fn start(&mut self, objective: &str, token_budget: Option<u64>) {
        self.goal = Some(Goal {
            id: new_id(),
            objective: objective.to_owned(),
            status: Status::Active,
            token_budget,
            tokens_used: 0,
            time_used_secs: 0,
        });
        self.budget_reported_for = None;
        self.interview = false;
    }

    /// Adds one turn's usage. A goal outside the running loop (paused, complete, dropped) does not count;
    /// reaching the budget flips an active goal to budget-limited.
    pub fn account(&mut self, tokens: u64, secs: u64) {
        let Some(goal) = self.goal.as_mut() else {
            return;
        };
        if !goal.status.running() {
            return;
        }
        goal.tokens_used = goal.tokens_used.saturating_add(tokens);
        goal.time_used_secs = goal.time_used_secs.saturating_add(secs);
        if goal.status == Status::Active && exhausted(goal) {
            goal.status = Status::BudgetLimited;
        }
        if goal.status != Status::BudgetLimited {
            self.budget_reported_for = None;
        }
    }

    /// Applies a new budget (`None` clears it). Moves the status to match the usage.
    pub fn set_budget(&mut self, budget: Option<u64>) {
        let Some(goal) = self.goal.as_mut() else {
            return;
        };
        goal.token_budget = budget;
        let status = goal.status;
        match status {
            Status::BudgetLimited if !exhausted(goal) => goal.status = Status::Active,
            Status::Active if exhausted(goal) => goal.status = Status::BudgetLimited,
            _ => {}
        }
        if goal.status != Status::BudgetLimited {
            self.budget_reported_for = None;
        }
    }

    /// Executes one `goal` tool call. The error is returned to the model as a tool error.
    pub fn tool(&mut self, args: &Value) -> Result<String, &'static str> {
        match args["op"].as_str() {
            Some("create") => {
                let objective = args["objective"]
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .ok_or("objective is required when op=create")?;
                let budget = tool_budget(&args["token_budget"])?;
                if self
                    .goal
                    .as_ref()
                    .is_some_and(|goal| !matches!(goal.status, Status::Dropped | Status::Complete))
                {
                    return Err("cannot create a new goal because this session already has a goal");
                }
                self.start(objective, budget);
                Ok("Goal created.".to_owned())
            }
            Some("get") => Ok(self.describe()),
            Some("resume") => {
                self.resume()?;
                Ok("Goal resumed.".to_owned())
            }
            Some("complete") => self.complete(),
            Some("drop") => {
                let goal = self.goal.as_mut().ok_or("No active goal.")?;
                if !matches!(
                    goal.status,
                    Status::Paused | Status::Active | Status::BudgetLimited
                ) {
                    return Err("No active goal.");
                }
                goal.status = Status::Dropped;
                self.interview = false;
                Ok("Goal dropped.".to_owned())
            }
            _ => Err("op must be one of create, get, resume, complete, drop"),
        }
    }

    /// Re-activates a paused goal. Exhausted budgets go straight back to budget-limited.
    pub fn resume(&mut self) -> Result<(), &'static str> {
        let Some(goal) = self.goal.as_mut() else {
            return Err("No paused goal.");
        };
        match goal.status {
            Status::Dropped => return Err("No paused goal."),
            Status::Complete => return Err("Goal is already complete."),
            _ => {}
        }
        goal.status = if exhausted(goal) {
            Status::BudgetLimited
        } else {
            Status::Active
        };
        self.budget_reported_for = None;
        Ok(())
    }

    /// Pauses an active or budget-limited goal. The autonomous loop stops and tokens stop counting.
    pub fn pause(&mut self) -> Result<(), &'static str> {
        let goal = self.goal.as_mut().ok_or("No active goal.")?;
        if !matches!(goal.status, Status::Active | Status::BudgetLimited) {
            return Err("No active goal.");
        }
        goal.status = Status::Paused;
        Ok(())
    }

    fn complete(&mut self) -> Result<String, &'static str> {
        let Some(goal) = self.goal.as_mut() else {
            return Err("cannot complete goal because no goal is active");
        };
        match goal.status {
            Status::Paused => return Err("cannot complete a paused goal; resume it first"),
            Status::Dropped => return Err("cannot complete a dropped goal"),
            Status::Complete => return Err("goal is already complete"),
            _ => {}
        }
        goal.status = Status::Complete;
        let report = completion_report(goal);
        self.interview = false;
        self.budget_reported_for = None;
        Ok(report)
    }

    fn describe(&self) -> String {
        let Some(goal) = self.goal.as_ref() else {
            return "No active goal.".to_owned();
        };
        json!({
            "objective": goal.objective,
            "status": goal.status,
            "token_budget": goal.token_budget,
            "tokens_used": goal.tokens_used,
            "remaining_tokens": remaining(goal),
            "time_used_seconds": goal.time_used_secs,
        })
        .to_string()
    }

    /// Records one tool call and its result for the stall rule. Only goal turns keep activity.
    pub fn record(&mut self, name: &str, args: &Value, output: &ToolOutput) {
        if !(self.running() || self.interview) {
            return;
        }
        let mut hasher = DefaultHasher::new();
        (
            name,
            args.to_string(),
            output.text.as_str(),
            output.is_error,
        )
            .hash(&mut hasher);
        self.activity.push(hasher.finish());
    }

    /// Digest of this turn's activity, or `None` when it made no tool calls.
    fn take_activity(&mut self) -> Option<u64> {
        if self.activity.is_empty() {
            return None;
        }
        let mut hasher = DefaultHasher::new();
        self.activity.hash(&mut hasher);
        self.activity.clear();
        Some(hasher.finish())
    }

    /// Decides what runs after a finished turn. `halted` says the turn needs the human (see
    /// `TurnReport::halts_automation`); `continued` says whether that turn was a continuation;
    /// `previous` carries the last continuation's activity digest for the stall rule.
    pub fn follow_up(
        &mut self,
        halted: bool,
        continued: bool,
        previous: &mut Option<u64>,
        todo: &TodoList,
    ) -> Option<(String, Kind)> {
        let activity = self.take_activity();
        // The interview ends only when `goal create` succeeds (`start`) or the goal is dropped or
        // completed. Tool calls made while interviewing (grounding reads, a failed create) keep `goal` offered.
        let stalled = continued && (activity.is_none() || *previous == activity);
        *previous = if continued { activity } else { None };
        if halted || stalled {
            return None;
        }
        let goal = self.goal.clone()?;
        match goal.status {
            Status::Active => {
                if all_blocked(todo) {
                    return None;
                }
                Some((render(CONTINUATION, &goal), Kind::Continuation))
            }
            Status::BudgetLimited
                if self.budget_reported_for.as_deref() != Some(goal.id.as_str()) =>
            {
                self.budget_reported_for = Some(goal.id.clone());
                Some((render(BUDGET_LIMIT, &goal), Kind::BudgetLimit))
            }
            _ => None,
        }
    }
}

impl Agent<'_> {
    /// Runs one user turn and keeps the goal going. Continuation and budget-limit turns follow while the
    /// goal is running, and stop on interrupt, a question, a blocker or failure, on a stalled
    /// continuation, or when the goal leaves the loop.
    pub(crate) fn turn_with_goal(
        &mut self,
        user_text: &str,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        let mut report = self.goal_turn(user_text, ui, TurnOrigin::User)?;
        let mut previous = None;
        let mut continued = false;
        loop {
            let interview = self.modes.goal.interview;
            let next = self.modes.goal.follow_up(
                report.halts_automation(),
                continued,
                &mut previous,
                &self.todo,
            );
            if self.modes.goal.goal.is_some() || self.modes.goal.interview != interview {
                self.save_modes()?;
            }
            let Some((prompt, kind)) = next else {
                break;
            };
            continued = kind == Kind::Continuation;
            // Continuation and budget prompts are written by the harness, not the user.
            report = self.goal_turn(&prompt, ui, TurnOrigin::Harness)?;
        }
        Ok(report)
    }

    fn goal_turn(&mut self, text: &str, ui: &mut dyn Ui, origin: TurnOrigin) -> Result<TurnReport> {
        let started = Instant::now();
        let report = self.turn_with_state(text, ui, true, None, origin)?;
        if self.modes.goal.goal.is_some() {
            let (_, turn) = self.usage_scope.snapshot();
            self.modes
                .goal
                .account(goal_tokens(&turn.all), started.elapsed().as_secs());
            self.save_modes()?;
        }
        Ok(report)
    }

    /// Runs one `goal` tool call against the session's goal state and persists the result.
    pub(crate) fn goal_tool(&mut self, args: &Value) -> std::result::Result<String, String> {
        // `complete` ends the autonomous loop, so it must clear the same blockers as `finish completed`.
        // A refusal returns before the goal state is touched, so the goal stays as it was.
        if args["op"].as_str() == Some("complete") {
            self.completion_blockers()?;
        }
        let outcome = self.modes.goal.tool(args);
        self.save_modes().map_err(|error| error.to_string())?;
        outcome.map_err(str::to_owned)
    }

    /// Edits the goal state from a slash command and persists it.
    pub fn goal_mutate<T>(&mut self, edit: impl FnOnce(&mut State) -> T) -> Result<T> {
        let value = edit(&mut self.modes.goal);
        self.save_modes()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests;
