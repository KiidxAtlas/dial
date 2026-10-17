//! `/prewalk [restart|off]`: arm, restart, or cancel the todo-gated model handoff.

use crate::agent::Agent;
use crate::agent::modes::prewalk::RestartOutcome;
use crate::app::{App, Shell, Step};
use anyhow::Result;

const USAGE: &str = "Usage: /prewalk [restart|off]";

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let arg = args.trim().to_ascii_lowercase();
    if !matches!(arg.as_str(), "" | "restart" | "off") {
        shell.note("Prewalk", USAGE);
        return Ok(Step::Done);
    }
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    match arg.as_str() {
        "off" => {
            let was_armed = agent.prewalk_is_armed();
            match agent.prewalk_disarm() {
                Ok(notices) => show(shell, notices),
                Err(error) => shell.note("Prewalk", format!("{error:#}")),
            }
            shell.note(
                "Prewalk",
                if was_armed {
                    "Prewalk off: canceled this session's pending handoff; keeping the active model."
                } else {
                    "Prewalk already off for this session; keeping the active model."
                },
            );
        }
        "restart" => match agent.prewalk_restart("@small") {
            Ok(report) => {
                show(shell, report.notices);
                let message = match report.outcome {
                    RestartOutcome::Rejected => None,
                    RestartOutcome::Armed => Some(format!(
                        "Prewalk restarted: using @main ({}) for planning, then switching to @small ({}) at the next edit/write (todo-gated).",
                        report.source, report.target
                    )),
                    RestartOutcome::Reset => Some(format!(
                        "Prewalk reset: using @main ({}); @small resolves to the same model, so no handoff was armed.",
                        report.source
                    )),
                };
                if let Some(message) = message {
                    shell.note("Prewalk", message);
                }
            }
            Err(error) => shell.note("Prewalk", format!("{error:#}")),
        },
        _ => {
            let target = match agent.prewalk_resolve("@small") {
                Ok(target) => target,
                Err(error) => {
                    shell.note("Prewalk", format!("{error:#}"));
                    return Ok(Step::Done);
                }
            };
            let label = format!("{}/{}", target.provider, target.model);
            match agent.prewalk_arm(target) {
                Ok((armed, notices)) => {
                    show(shell, notices);
                    if armed {
                        shell.note(
                            "Prewalk",
                            format!(
                                "Prewalk on: switching to {label} at the next edit/write (todo-gated)."
                            ),
                        );
                    }
                }
                Err(error) => shell.note("Prewalk", format!("{error:#}")),
            }
        }
    }
    Ok(Step::Done)
}

fn show(shell: &mut Shell, notices: Vec<String>) {
    for notice in notices {
        shell.note("Prewalk", notice);
    }
}
