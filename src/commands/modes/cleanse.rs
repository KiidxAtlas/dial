//! `/cleanse`: runs the project's checkers, then repairs their diagnostics with worker subagents
//! until the checkers report clean. Discovery and the repair loop live in `agent::modes::cleanse`.

use crate::agent::Agent;
use crate::agent::Ui;
use crate::agent::modes::cleanse::{LoopStatus, Suite, format_diagnostics};
use crate::app::{App, Bridge, PAINT_EVERY, Shell, Step};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

const CLEANSE_USAGE: &str =
    "Usage: /cleanse [request] [--all] [--tests] [-n <agents>] [-m <model>]";
const DEFAULT_MAX_AGENTS: usize = 32;
const CANCELLED: &str = "Cleanse cancelled.";
const NO_CHECKER: &str = "No supported checker with an available executable was found.";

#[derive(Debug, PartialEq, Eq)]
struct CleanseArgs {
    request: Option<String>,
    all: bool,
    include_tests: bool,
    max_agents: usize,
    model: Option<String>,
}

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let args = match parse_args(args) {
        Ok(args) => args,
        Err(usage) => {
            shell.note("Cleanse", usage);
            return Ok(Step::Done);
        }
    };
    if args.model.is_some() {
        shell.note(
            "Cleanse",
            "`-m` is not supported: Dial's subagent runner selects the model from the role config.",
        );
        return Ok(Step::Done);
    }
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    shell.composer.set_busy(true);
    let interrupt = app.interrupt.clone();
    let result = {
        let mut bridge = Bridge {
            app,
            shell: &mut *shell,
            interrupt: interrupt.clone(),
            started: Instant::now(),
            last_paint: Instant::now() - PAINT_EVERY,
            failed: false,
            pending: Vec::new(),
            next_id: 0,
        };
        bridge.repaint(true);
        cleanse(agent, &mut bridge, &interrupt, &args)
    };
    shell.composer.set_busy(false);
    match result {
        Ok(message) => shell.note("Cleanse", message),
        Err(error) => shell.transcript.error(&format!("{error:#}")),
    }
    Ok(Step::Done)
}

/// Resolves the checker suite (request, picker, or `--all`), then runs the repair loop.
fn cleanse(
    agent: &mut Agent<'_>,
    ui: &mut dyn Ui,
    interrupt: &Arc<AtomicBool>,
    args: &CleanseArgs,
) -> Result<String> {
    let suite = match &args.request {
        Some(request) => match agent.cleanse_request(request, ui, interrupt)? {
            Some(suite) if suite.plans().is_empty() => {
                return Ok("Checker discovery produced no runnable command.".into());
            }
            Some(suite) => suite,
            None => return Ok(CANCELLED.into()),
        },
        None => {
            let discovered = agent.cleanse_discover(args.include_tests);
            if args.all || discovered.plans().is_empty() {
                discovered
            } else {
                match pick(ui, discovered) {
                    Pick::Suite(suite) => suite,
                    Pick::Request(request) => {
                        match agent.cleanse_request(&request, ui, interrupt)? {
                            Some(suite) if suite.plans().is_empty() => {
                                return Ok("Checker discovery produced no runnable command.".into());
                            }
                            Some(suite) => suite,
                            None => return Ok(CANCELLED.into()),
                        }
                    }
                    Pick::Cancel => return Ok(CANCELLED.into()),
                }
            }
        }
    };
    if suite.plans().is_empty() {
        return Ok(no_checker_message(&suite));
    }
    let outcome = agent.cleanse_repair(&suite, args.max_agents, interrupt, ui)?;
    Ok(match outcome.status {
        LoopStatus::Cancelled => CANCELLED.into(),
        LoopStatus::Clean if outcome.workers == 0 => {
            format!("Clean: {} checker(s) passed.", suite.plans().len())
        }
        LoopStatus::Clean => "Clean: all detected diagnostics are resolved.".into(),
        LoopStatus::Stalled => format!(
            "Stalled after {} worker(s); {} diagnostic(s) remain:\n{}",
            outcome.workers,
            outcome.unresolved.len(),
            format_diagnostics(&outcome.unresolved),
        ),
    })
}

fn no_checker_message(suite: &Suite) -> String {
    let mut lines = vec![NO_CHECKER.to_string()];
    lines.extend(
        suite
            .skipped()
            .iter()
            .map(|skipped| format!("- {}: {}", skipped.label, skipped.reason)),
    );
    lines.join("\n")
}

enum Pick {
    Suite(Suite),
    Request(String),
    Cancel,
}

/// Shows the checker picker. Typed text that is not an option becomes a free-form request.
fn pick(ui: &mut dyn Ui, mut suite: Suite) -> Pick {
    let all_label = format!("Run all {} discovered checker(s)", suite.plans().len());
    let mut options = vec![all_label.clone()];
    options.extend(
        suite
            .plans()
            .iter()
            .map(|plan| format!("{} — {}", plan.label, plan.display)),
    );
    let Some(answer) = ui.ask_user(
        "Select what to cleanse",
        "Choose a checker to run, or type a request.",
        &options,
    ) else {
        return Pick::Cancel;
    };
    if answer == all_label {
        return Pick::Suite(suite);
    }
    match options.iter().position(|option| *option == answer) {
        Some(index) if index > 0 => {
            let id = suite.plans()[index - 1].id.clone();
            suite.retain(|plan| plan.id == id);
            Pick::Suite(suite)
        }
        _ => Pick::Request(answer),
    }
}

/// Parses `/cleanse` arguments: `-n`/`-m` take the next token,
/// unknown flags are rejected, and remaining words form the request.
fn parse_args(args: &str) -> Result<CleanseArgs, &'static str> {
    let mut parsed = CleanseArgs {
        request: None,
        all: false,
        include_tests: false,
        max_agents: DEFAULT_MAX_AGENTS,
        model: None,
    };
    let mut words = Vec::new();
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        match token {
            "--all" | "-a" => parsed.all = true,
            "--tests" | "-t" => parsed.include_tests = true,
            "--agents" | "-n" => {
                parsed.max_agents = tokens
                    .next()
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|count| *count > 0)
                    .ok_or(CLEANSE_USAGE)?;
            }
            "--model" | "-m" => {
                parsed.model = Some(tokens.next().ok_or(CLEANSE_USAGE)?.to_string());
            }
            flag if is_flag(flag) => return Err(CLEANSE_USAGE),
            word => words.push(word),
        }
    }
    if !words.is_empty() {
        parsed.request = Some(words.join(" "));
    }
    Ok(parsed)
}

/// `-x` style flags. `-5` is a negative number and stays part of the request.
fn is_flag(token: &str) -> bool {
    token.len() > 1
        && token.starts_with('-')
        && !token[1..].starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests;
