//! `/security` slash command. The grammar and all
//! operations live in `crate::security`; this file only runs them against the app and shows output.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use serde_json::Value;

use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use crate::security::Reviewer;
use crate::security::command;

const TITLE: &str = "Security";

/// Runs reviewer tasks through the active agent runtime and observes `/security` cancellation.
struct AgentReviewer<'a, 'b> {
    interrupt: Arc<AtomicBool>,
    agent: &'a mut Agent<'b>,
}

impl Reviewer for AgentReviewer<'_, '_> {
    fn run_reviewer(&mut self, prompt: &str, output_schema: &Value) -> Result<String> {
        self.agent.run_reviewer_task(prompt, output_schema)
    }

    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }
}

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let mut reviewer = AgentReviewer {
        interrupt: agent.interrupt_flag(),
        agent,
    };
    report(
        shell,
        command::execute(&app.config, args, Some(&mut reviewer)),
    )
}

/// Every operation except `scan` and `validate`, which run reviewer tasks on the agent. Bad input
/// and usage requests also answer at once.
pub(super) fn run_live(shell: &mut Shell, app: &App, args: &str) -> Option<Result<Step>> {
    let reviewer = matches!(
        command::parse(args),
        Ok(Some(invocation)) if matches!(invocation.op, "scan" | "validate")
    );
    (!reviewer).then(|| report(shell, command::execute(&app.config, args, None)))
}

fn report(shell: &mut Shell, result: Result<String>) -> Result<Step> {
    match result {
        Ok(text) => shell.note(TITLE, text),
        Err(error) => shell.note(TITLE, format!("Security: {error:#}")),
    }
    Ok(Step::Done)
}
