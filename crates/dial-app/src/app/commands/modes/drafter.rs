//! Turn-drafter preference, persisted through the shared settings/adoption path.
use super::{Toggle, parse_toggle};
use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use crate::config::settings_store::{Change, Scope};
use anyhow::Result;

fn next_state(current: bool, args: &str) -> std::result::Result<Option<bool>, &'static str> {
    Ok(
        match parse_toggle(args, "Usage: /drafter [on|off|status]")? {
            Toggle::On => Some(true),
            Toggle::Off => Some(false),
            Toggle::Flip => Some(!current),
            Toggle::Status => None,
        },
    )
}

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: Option<&mut Agent<'_>>,
    args: &str,
) -> Result<Step> {
    let current = shell.settings_config(app).experimental.turn_optimizer;
    let next = match next_state(current, args) {
        Ok(next) => next,
        Err(usage) => {
            shell.note("Turn drafter", usage);
            return Ok(Step::Done);
        }
    };
    let Some(enabled) = next else {
        shell.note("Turn drafter", if current {
            "On: each drafted instruction is shown for acceptance before sending. /drafter off sends your original messages directly."
        } else {
            "Off: your original messages go directly to the agent. /drafter on enables drafting and review."
        });
        return Ok(Step::Done);
    };
    let deferred = agent.is_none();
    match shell.save(
        app,
        agent,
        &[Change::Set {
            path: "experimental.turn_optimizer".into(),
            value: toml::Value::Boolean(enabled),
        }],
        Scope::Project,
    ) {
        Ok(step) => {
            let behavior = if enabled {
                "On: review each draft, then choose Accept and send or Cancel."
            } else {
                "Off: original messages go directly to the agent without drafting."
            };
            shell.note(
                "Turn drafter",
                if deferred {
                    format!("{behavior} Applies after the current turn.")
                } else {
                    behavior.into()
                },
            );
            Ok(step)
        }
        Err(error) => {
            shell.note("Turn drafter", format!("Not changed: {error:#}"));
            Ok(Step::Done)
        }
    }
}

#[cfg(test)]
mod tests;
