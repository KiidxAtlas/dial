//! `/btw`: persistent side threads beside the main session. Promotion creates an independent full
//! sibling conversation; no side exchange is injected into the main agent's history.
//! Subcommands: `new <question>`, `threads`, `open <id>`, `copy`, `promote <id>`, `cancel`.

use crate::agent::Agent;
use crate::app::side::SideThreadState;
use crate::app::{App, Shell, Step};
use anyhow::Result;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let args = args.trim();
    let (verb, rest) = match args.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (args, ""),
    };
    if let Err(error) = shell.side.bind(agent.session()) {
        shell.note("BTW", format!("error: {error:#}"));
        return Ok(Step::Done);
    }
    if verb == "promote" {
        if rest.is_empty() {
            shell.note("BTW", "usage: /btw promote <id>");
            return Ok(Step::Done);
        }
        match shell.side.promote(agent, &app.layout, rest) {
            Ok(session) => shell.note(
                "BTW",
                format!("Thread {rest} is a full conversation: /resume {session}. The main session is unchanged."),
            ),
            Err(error) => shell.note("BTW", format!("error: {error:#}")),
        }
        return Ok(Step::Done);
    }
    let asks = SideThreadState::asks(args);
    if asks && !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    let snapshot = if asks {
        match agent.side_snapshot(None) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                shell.note("BTW", format!("error: {error:#}"));
                return Ok(Step::Done);
            }
        }
    } else {
        None
    };
    let text = shell.side.command(args, snapshot.as_ref());
    shell.note("BTW", text);
    Ok(Step::Done)
}
