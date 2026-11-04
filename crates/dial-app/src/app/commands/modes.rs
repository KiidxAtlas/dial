//! Mode and model slash commands, such as `/effort`.
//! Reuses the existing provider overlay and reasoning settings path (`reasoning_changes` + `persist`).

use crate::agent::Agent;
use crate::app::{App, Shell, Step, reasoning_changes, reasoning_profile, thinking_label};
use crate::config::settings_store::Scope;
use crate::config::types::Config;
use anyhow::Result;
mod btw;
mod cleanse;
mod compaction;
mod contract;
mod debug;
mod drafter;
mod fresh;
mod goal;
mod handoff;
mod loop_cmd;
mod plan_review;
mod prewalk;
mod ratchet;
mod rule;
mod shake;
mod switch;
mod tan;

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    match name {
        "vibe" => Some(vibe(shell, app, agent, args)),
        "skillful" => Some(skillful(shell, app, agent, args)),
        "fresh" => Some(fresh::run(shell, app, agent)),
        "handoff" => Some(handoff::run(shell, app, agent, args)),
        "shake" => Some(shake::run(shell, app, agent, args)),
        "btw" => Some(btw::run(shell, app, agent, args)),
        "tan" => Some(tan::run(shell, app, agent, args)),
        "debug" => Some(debug::run(shell, app, agent, args)),
        "rule" => Some(rule::run(shell, app, agent, args)),
        "prewalk" => Some(prewalk::run(shell, app, agent, args)),
        "switch" => Some(switch::run(shell, app, agent, args)),
        "plan-review" => Some(plan_review::run(shell, app, agent)),
        "ratchet" => Some(ratchet::run(shell, app, agent, args)),
        "loop" => Some(loop_cmd::run(shell, app, agent, args)),
        "goal" => Some(goal::run(shell, app, agent, args)),
        "guided-goal" => Some(goal::guided(shell, app, agent, args)),
        "contract" => Some(contract::run(shell, app, agent, args)),
        "cleanse" => Some(cleanse::run(shell, app, agent, args)),
        _ => None,
    }
}

/// `/effort`; `agent` is `None` during a turn (see `Shell::save`).
pub(super) fn run_settings(
    shell: &mut Shell,
    app: &App,
    agent: Option<&mut Agent<'_>>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    match name {
        "effort" => Some(effort(shell, app, agent, args)),
        "drafter" => Some(drafter::run(shell, app, agent, args)),
        _ => None,
    }
}

pub(super) fn completions(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(Some(profile)) = reasoning_profile(config) {
        out.extend(
            profile
                .capabilities
                .reasoning_efforts
                .iter()
                .map(|effort| format!("/effort {}", effort.as_str())),
        );
    }
    out
}

fn effort(shell: &mut Shell, app: &App, agent: Option<&mut Agent<'_>>, args: &str) -> Result<Step> {
    let args = args.trim();
    if let Some((role, level)) = args.split_once(char::is_whitespace) {
        let (role, level) = (role.trim(), level.trim());
        match crate::app::selection::role_thinking_changes(
            &shell.settings_config(app).clone(),
            role,
            level,
        )
        .and_then(|changes| shell.save(app, agent, &changes, Scope::Project))
        {
            Ok(step) => {
                shell.note(
                    "Thinking",
                    format!("Thinking for role {role} set to {level}."),
                );
                return Ok(step);
            }
            Err(error) => {
                shell.note("Thinking", format!("not changed: {error:#}"));
                return Ok(Step::Done);
            }
        }
    }
    let config = shell.settings_config(app).clone();
    if args.is_empty() {
        if config.thinking_auto {
            let judged = match agent.as_ref().map(|agent| agent.last_auto_thinking()) {
                Some(Some(choice)) => format!("last judged: {choice}"),
                Some(None) => "no prompt judged yet this session".to_owned(),
                None => "judging the running turn".to_owned(),
            };
            shell.note(
                "Thinking",
                format!("Thinking: auto ({judged})\nSet with /effort <level> or /effort auto"),
            );
            return Ok(Step::Done);
        }
        match reasoning_profile(&config) {
            Ok(Some(profile)) => {
                let levels = profile
                    .capabilities
                    .reasoning_efforts
                    .iter()
                    .map(|effort| effort.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ");
                shell.note(
                    "Thinking",
                    format!(
                        "Thinking: {}\nAvailable: {levels}\nSet with /effort <level>",
                        thinking_label(&profile)
                    ),
                );
            }
            Ok(None) => shell.note(
                "Thinking",
                "Dial does not know this model's reasoning levels yet.\nOpen /models and press F5 to rescan, or pick a /profile that sets them.",
            ),
            Err(error) => shell.note("Thinking", format!("{error:#}")),
        }
        return Ok(Step::Done);
    }
    match reasoning_changes(&config, args).and_then(|(changes, effort)| {
        shell
            .save(app, agent, &changes, Scope::Project)
            .map(|step| (step, effort))
    }) {
        Ok((step, effort)) => {
            shell.note("Thinking", format!("Thinking set to {effort}."));
            Ok(step)
        }
        Err(error) => {
            shell.note("Thinking", format!("not changed: {error:#}"));
            Ok(Step::Done)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Toggle {
    On,
    Off,
    Status,
    Flip,
}

fn parse_toggle(args: &str, usage: &'static str) -> std::result::Result<Toggle, &'static str> {
    match args.trim() {
        "" => Ok(Toggle::Flip),
        "on" => Ok(Toggle::On),
        "off" => Ok(Toggle::Off),
        "status" => Ok(Toggle::Status),
        _ => Err(usage),
    }
}

fn vibe(shell: &mut Shell, _app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    const USAGE: &str = "Usage: /vibe [on|off|status]";
    let current = agent.modes().vibe;
    let next = match parse_toggle(args, USAGE) {
        Ok(Toggle::Status) => {
            shell.note(
                "Vibe",
                if current {
                    "Vibe mode: on (read-only tools)"
                } else {
                    "Vibe mode: off"
                },
            );
            return Ok(Step::Done);
        }
        Ok(Toggle::On) => true,
        Ok(Toggle::Off) => false,
        Ok(Toggle::Flip) => !current,
        Err(usage) => {
            shell.note("Vibe", usage);
            return Ok(Step::Done);
        }
    };
    match agent.set_vibe(next) {
        Ok(()) => shell.note(
            "Vibe",
            if next {
                "Vibe mode on: only read-only tools are offered."
            } else {
                "Vibe mode off."
            },
        ),
        Err(error) => shell.note("Vibe", format!("not changed: {error:#}")),
    }
    Ok(Step::Done)
}

fn skillful(shell: &mut Shell, _app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    const USAGE: &str = "Usage: /skillful [on|off|status]";
    let current = agent.modes().skillful != Some(false);
    let next = match parse_toggle(args, USAGE) {
        Ok(Toggle::Status) => {
            shell.note(
                "Skillful",
                if current {
                    "Skills: offered"
                } else {
                    "Skills: hidden for this session"
                },
            );
            return Ok(Step::Done);
        }
        Ok(Toggle::On) => true,
        Ok(Toggle::Off) => false,
        Ok(Toggle::Flip) => !current,
        Err(usage) => {
            shell.note("Skillful", usage);
            return Ok(Step::Done);
        }
    };
    match agent.set_skillful(if next { Some(true) } else { Some(false) }) {
        Ok(()) => shell.note(
            "Skillful",
            if next {
                "Skills will be offered from the next turn."
            } else {
                "Skills hidden from the next turn."
            },
        ),
        Err(error) => shell.note("Skillful", format!("not changed: {error:#}")),
    }
    Ok(Step::Done)
}
