//! `/setup` and `/doctor`: list optional capabilities, preview one set-up, and run it.
//!
//! `/setup <id>` shows the exact commands, the install destination and any prerequisites.
//! Nothing runs until `/setup <id> yes`. That runs as a background job; settings are written
//! only after the install and its health check succeed.

use super::extensions::spawn_job_streaming;
use crate::agent::Agent;
use crate::app::{App, Shell, Step, Which};
use crate::setup::deps::{Env, plan, recipe, run};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const TITLE: &str = "Setup";

const USAGE: &str = "/setup                 list capabilities and their status\n/setup <id>            preview what would run and where it installs\n/setup <id> yes        set it up (prerequisites first), then save its settings";

pub(super) const COMMANDS: &[super::CommandSpec] = &[
    super::CommandSpec {
        name: "setup",
        description: "Check optional capabilities, set one up, or sign in to providers (/setup providers)",
        args_hint: "[<id> [yes]]",
        subcommands: &["providers"],
    },
    super::CommandSpec {
        name: "doctor",
        description: "Alias of /setup: list capability status",
        args_hint: "",
        subcommands: &[],
    },
];

pub(super) fn run_command(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if name != "setup" && name != "doctor" {
        return None;
    }
    let parts: Vec<&str> = args.split_whitespace().collect();
    let env = Env::current(&app.layout.user_dir);
    match parts.as_slice() {
        [] => shell.note(TITLE, run::list_text(&env)),
        ["help"] => shell.note(TITLE, USAGE),
        ["providers"] => return Some(shell.open(app, agent, Which::Providers)),
        [id] => shell.note(TITLE, run::preview_text(id, &env)),
        [id, "yes"] => start(shell, app, id),
        _ => shell.note(TITLE, USAGE),
    }
    Some(Ok(Step::Done))
}

fn start(shell: &mut Shell, app: &App, id: &str) {
    let env = Env::current(&app.layout.user_dir);
    let Some(target) = recipe(id) else {
        shell.note(TITLE, format!("Unknown capability `{id}`."));
        return;
    };
    if let Err(blocked) = plan::plan(target, &env) {
        shell.note(TITLE, blocked.to_string());
        return;
    }
    let layout = app.layout.clone();
    let id = id.to_owned();
    spawn_job_streaming(shell, "Setup", move |cancel: Arc<AtomicBool>, progress| {
        let done = run::install_and_save(&layout, &id, &cancel, progress)?;
        Ok(format!("Set up and saved:\n{}", done.join("\n")))
    });
}
