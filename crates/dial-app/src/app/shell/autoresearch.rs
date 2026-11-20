//! `/autoresearch auto`: the self-driving experiment loop.
//!
//! The host owns every decision. Each iteration asks the agent for one focused change, then the
//! engine (`tools::knowledge::autoresearch::experiment::evaluate`) commits it, benchmarks it, and
//! keeps it only when it beats the best run; anything else is reset to the iteration's start. The
//! loop stops at the iteration cap, after `patience` iterations without improvement, when a turn
//! ends waiting for the human (question, failure, interrupt), or when evaluation itself fails.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::drive::Step;
use crate::app::shell::Shell;
use crate::tools::knowledge::autoresearch::experiment::{self, Policy, Verdict};
use anyhow::Result;
use std::path::Path;

const TITLE: &str = "Autoresearch";

impl Shell {
    pub(super) fn autoresearch_auto(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        state: &Path,
        args: &str,
    ) -> Result<Step> {
        let mut policy = match Policy::parse(args) {
            Ok(policy) => policy,
            Err(error) => {
                self.note(
                    TITLE,
                    format!("{error}. Usage: /autoresearch auto [iterations] [--patience N]"),
                );
                return Ok(Step::Done);
            }
        };
        let ws = agent.workspace().clone();
        let started = match experiment::current(state) {
            Ok(Some(e)) => e,
            Ok(None) => {
                self.note(TITLE, "No experiment. Start one with /autoresearch init <name> <metric> <max|min> <benchmark command>.");
                return Ok(Step::Done);
            }
            Err(error) => {
                self.note(TITLE, format!("{error:#}"));
                return Ok(Step::Done);
            }
        };
        match experiment::ensure_baseline(&ws, state) {
            Ok(Some(run)) => self.note(
                TITLE,
                format!(
                    "Baseline {} = {} at {}.",
                    started.metric,
                    run.metric,
                    short(&run.commit)
                ),
            ),
            Ok(None) => {}
            Err(error) => {
                self.note(TITLE, format!("Not started: {error:#}"));
                return Ok(Step::Done);
            }
        }
        self.note(
            TITLE,
            format!(
                "Self-driving {} for up to {} iteration(s), stopping after {} without improvement. Esc stops after the current step.",
                started.name, policy.max_iterations, policy.patience
            ),
        );
        let stop = loop {
            let number = policy.next();
            let start = match experiment::head(ws.root()) {
                Ok(start) => start,
                Err(error) => break format!("{error:#}"),
            };
            let prompt = match experiment::iteration_prompt(state, number, policy.max_iterations) {
                Ok(prompt) => prompt,
                Err(error) => break format!("{error:#}"),
            };
            if let Step::Quit | Step::Rebuild(_) = self.run_turn(app, agent, &prompt)? {
                break "the session ended".to_owned();
            }
            // Evaluate even a halted turn: its change is committed or reverted, never left behind.
            let halted = self.last_turn_halted;
            let iteration = match experiment::evaluate(&ws, state, &start, number) {
                Ok(iteration) => iteration,
                Err(error) => break format!("evaluation failed: {error:#}"),
            };
            self.note(TITLE, describe(number, &iteration));
            let reason = policy.record(iteration.verdict);
            if halted {
                break "the turn ended waiting for you".to_owned();
            }
            if let Some(reason) = reason {
                break reason;
            }
        };
        let best = experiment::best(state)
            .ok()
            .flatten()
            .map_or_else(|| "none".to_owned(), |v| v.to_string());
        self.note(
            TITLE,
            format!(
                "Stopped: {stop}. Best {} = {best}; /autoresearch log lists the runs.",
                started.metric
            ),
        );
        Ok(Step::Done)
    }
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

fn describe(number: u32, iteration: &experiment::Iteration) -> String {
    let metric = iteration
        .metric
        .map_or_else(String::new, |m| format!(" = {m}"));
    let commit = iteration
        .commit
        .as_deref()
        .map_or_else(String::new, |c| format!(" ({})", short(c)));
    match iteration.verdict {
        Verdict::Kept => format!("Iteration {number}: kept{metric}{commit}, new best."),
        Verdict::Reverted => format!("Iteration {number}: reverted{metric}{commit}, not better."),
        Verdict::NoChange => format!("Iteration {number}: no change proposed."),
        Verdict::Crashed => format!(
            "Iteration {number}: reverted{commit}, benchmark failed: {}",
            iteration.detail
        ),
    }
}
