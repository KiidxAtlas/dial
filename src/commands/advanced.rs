//! Desktop-control and security commands: `/computer on|off|status` and `/security <subcommand>`.
//! `/computer on` and `off` persist the `integrations` list, which gates the `computer` tool, the
//! same way `/mcp enable` persists server state. `/security` forwards to the security command
//! parser in `commands::security`.

use super::CommandSpec;
use super::security;
use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use crate::computer;
use crate::security::command::SUBCOMMANDS as SECURITY_SUBCOMMANDS;
use crate::storage::settings_store::{Change, Scope};
use anyhow::{Result, bail};

pub(super) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "computer",
        description: "control the desktop: screenshots and input",
        args_hint: "on|off|status",
        subcommands: &["on", "off", "status"],
    },
    CommandSpec {
        name: "security",
        description: "security review: plan, scans, import, export, compare, cloud",
        args_hint: "<subcommand>",
        subcommands: SECURITY_SUBCOMMANDS,
    },
];

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    match name {
        "computer" => Some(computer_command(shell, app, agent, args)),
        "security" => Some(security::run(shell, app, agent, args)),
        _ => None,
    }
}

fn computer_command(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    args: &str,
) -> Result<Step> {
    let enabled_now = app
        .config
        .integrations
        .iter()
        .any(|name| name == computer::NAME);
    match args.trim() {
        "" | "status" => {
            let state = if enabled_now { "on" } else { "off" };
            let mut report = format!(
                "Computer use    {state}\nScreenshots     active display, scaled to at most {}×{}",
                computer::MAX_WIDTH,
                computer::MAX_HEIGHT,
            );
            match computer::capabilities() {
                Ok(caps) => report.push_str(&format!(
                    "\nBackend         {}\nScreen capture  {}\nInput control   {}\nAccessibility   {}",
                    capability_field(&caps, "backend"),
                    capability_field(&caps, "capturePermission"),
                    capability_field(&caps, "inputPermission"),
                    capability_field(&caps, "axPermission"),
                )),
                Err(reason) => report.push_str(&format!("\nUnavailable     {reason}")),
            }
            report.push_str(if enabled_now {
                "\n/computer off turns it off."
            } else {
                "\n/computer on lets the agent see and control the screen."
            });
            shell.note("Computer", report);
            Ok(Step::Done)
        }
        "on" => {
            let caps = match computer::capabilities() {
                Ok(caps) => caps,
                Err(reason) => bail!("Cannot enable computer use: {reason}"),
            };
            let step = set_enabled(shell, app, agent, true)?;
            let denied: Vec<&str> = [
                ("capturePermission", "Screen Recording"),
                ("inputPermission", "Accessibility"),
            ]
            .into_iter()
            .filter(|(key, _)| capability_field(&caps, key) == "denied")
            .map(|(_, label)| label)
            .collect();
            if denied.is_empty() {
                shell.note(
                    "Computer",
                    "Enabled computer use. Each run and input action is approved before it runs; read-only calls such as screenshots and window listings run without approval.",
                );
            } else {
                shell.note(
                    "Computer",
                    format!(
                        "Enabled computer use, but {} permission is not granted. Grant it to this terminal app in System Settings > Privacy & Security, then restart dial.",
                        denied.join(" and ")
                    ),
                );
            }
            Ok(step)
        }
        "off" => {
            let step = set_enabled(shell, app, agent, false)?;
            shell.note("Computer", "Disabled computer use.");
            Ok(step)
        }
        _ => bail!("Usage: /computer on|off|status"),
    }
}

fn set_enabled(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, enabled: bool) -> Result<Step> {
    let mut names: Vec<String> = app
        .config
        .integrations
        .iter()
        .filter(|name| name.as_str() != computer::NAME)
        .cloned()
        .collect();
    if enabled {
        names.push(computer::NAME.to_owned());
    }
    let value = toml::Value::Array(names.into_iter().map(toml::Value::String).collect());
    shell.persist(
        app,
        agent,
        &[Change::Set {
            path: "integrations".into(),
            value,
        }],
        Scope::Global,
    )
}

fn capability_field(value: &serde_json::Value, key: &str) -> String {
    match &value[key] {
        serde_json::Value::Null => "unknown".to_owned(),
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
