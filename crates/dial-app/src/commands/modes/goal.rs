//! `/goal` and `/guided-goal`: the session's persistent objective. Engine: `agent::modes::goal`.

use crate::agent::Agent;
use crate::agent::modes::goal::{self, Status};
use crate::app::{App, Shell, Step};
use anyhow::Result;
use serde_json::json;

const USAGE: &str =
    "Usage: /goal [set <objective>|<objective>|show|pause|resume|drop|budget <tokens|off>]";

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let trimmed = args.trim();
    let (word, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (trimmed, ""),
    };
    match word.to_ascii_lowercase().as_str() {
        "" | "show" => {
            let shown = agent.goal_mutate(|state| state.tool(&json!({"op": "get"})))?;
            report(shell, shown);
            Ok(Step::Done)
        }
        "set" => set(shell, app, agent, rest),
        "pause" => {
            let outcome =
                agent.goal_mutate(|state| state.pause().map(|()| "Goal paused.".to_owned()))?;
            report(shell, outcome);
            Ok(Step::Done)
        }
        "resume" => {
            let outcome = agent.goal_mutate(|state| {
                state.resume()?;
                let goal = state.goal.as_ref().ok_or("No paused goal.")?;
                Ok::<_, &'static str>((goal::active_prompt(goal), "Goal resumed.".to_owned()))
            })?;
            match outcome {
                Ok((prompt, note)) => {
                    shell.note("Goal", note);
                    shell.run_turn(app, agent, &prompt)
                }
                Err(error) => {
                    shell.note("Goal", error);
                    Ok(Step::Done)
                }
            }
        }
        "drop" => {
            let outcome = agent.goal_mutate(|state| state.tool(&json!({"op": "drop"})))?;
            report(shell, outcome);
            Ok(Step::Done)
        }
        "budget" => budget(shell, app, agent, rest),
        _ => set(shell, app, agent, trimmed),
    }
}

/// `/guided-goal <idea>`: the agent interviews the user in chat before any goal exists.
pub(super) fn guided(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    args: &str,
) -> Result<Step> {
    let idea = args.trim();
    if idea.is_empty() {
        shell.note("Guided goal", "Usage: /guided-goal <idea>");
        return Ok(Step::Done);
    }
    let live = agent.goal_mutate(|state| {
        state.goal.as_ref().is_some_and(|goal| {
            matches!(
                goal.status,
                Status::Active | Status::Paused | Status::BudgetLimited
            )
        })
    })?;
    if live {
        shell.note(
            "Guided goal",
            "A goal already exists; `/goal drop` it first.",
        );
        return Ok(Step::Done);
    }
    agent.goal_mutate(|state| state.interview = true)?;
    shell.run_turn(app, agent, &goal::guided_kickoff(idea))
}

fn set(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, objective: &str) -> Result<Step> {
    if objective.is_empty() {
        shell.note("Goal", USAGE);
        return Ok(Step::Done);
    }
    let prompt = agent.goal_mutate(|state| {
        state.start(objective, None);
        state
            .goal
            .as_ref()
            .map_or_else(|| objective.to_owned(), goal::active_prompt)
    })?;
    shell.note("Goal", format!("Goal set: {objective}"));
    shell.run_turn(app, agent, &prompt)
}

fn budget(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, rest: &str) -> Result<Step> {
    if rest.is_empty() {
        shell.note("Goal", USAGE);
        return Ok(Step::Done);
    }
    let parsed = if rest.eq_ignore_ascii_case("off") {
        None
    } else {
        match rest.parse::<u64>() {
            Ok(tokens) if tokens > 0 => Some(tokens),
            _ => {
                shell.note("Goal", "Goal budget must be a positive integer or `off`.");
                return Ok(Step::Done);
            }
        }
    };
    let outcome = agent.goal_mutate(|state| {
        match state.goal.as_ref().map(|goal| goal.status) {
            None | Some(Status::Dropped) => return Err("No active goal."),
            Some(Status::Complete) => return Err("Goal is already complete."),
            Some(_) => {}
        }
        state.set_budget(parsed);
        let note = match parsed {
            None => "Goal budget cleared.".to_owned(),
            Some(tokens) => format!("Goal budget set to {tokens}."),
        };
        let resumed = state
            .running()
            .then(|| state.goal.as_ref().map(goal::active_prompt))
            .flatten();
        Ok((note, resumed))
    })?;
    match outcome {
        Ok((note, resumed)) => {
            shell.note("Goal", note);
            match resumed {
                Some(prompt) => shell.run_turn(app, agent, &prompt),
                None => Ok(Step::Done),
            }
        }
        Err(error) => {
            shell.note("Goal", error);
            Ok(Step::Done)
        }
    }
}

fn report(shell: &mut Shell, outcome: std::result::Result<String, &'static str>) {
    match outcome {
        Ok(text) => shell.note("Goal", text),
        Err(error) => shell.note("Goal", error),
    }
}
