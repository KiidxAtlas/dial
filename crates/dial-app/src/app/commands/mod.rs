//! Slash-command groups. Each group owns its command names, descriptions, argument hints,
//! subcommand completions and handler. `Shell::command` consults [`run`] before its own match,
//! so a group can add commands or extend existing ones without touching the dispatcher.
//!
//! A group module exposes:
//! - `COMMANDS`: specs for commands it adds (never names already listed in app.rs `BUILTIN_COMMANDS`).
//! - `run`: returns `None` when the name is not handled by the group, otherwise the handler result.
//! - `run_live` (optional): the commands and subcommands that need only the shell and app, never
//!   the agent. They run at once even while a turn runs; `run` tries them first.
//! - `run_settings` (optional): commands whose only use of the agent is adopting saved settings.
//!   During a turn they get `None`: the write lands at once and the agent adopts it afterwards.
//! - `completions`: extra composer argument candidates such as `/mcp login <server>`.

use super::{App, Shell, Step};
use crate::agent::Agent;
use crate::config::types::Config;
use anyhow::Result;

mod advanced;
pub(super) mod branch;
mod collab;
pub(crate) mod commit;
mod config_cmds;
pub(crate) use config_cmds::UsageStatus;
mod extensions;
pub(crate) mod extensions_browser;
mod graphify;
pub(crate) mod maintenance;
mod modes;
mod peers;
mod security;
mod session;
mod setup;
pub(super) mod smithery;
mod workspace;

/// Name, one-line description, argument hint (empty for none) and static subcommands.
#[derive(Debug, Clone, Copy)]
pub(super) struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub args_hint: &'static str,
    pub subcommands: &'static [&'static str],
}

/// Every group's commands in display order.
pub(super) fn specs() -> impl Iterator<Item = &'static CommandSpec> {
    collab::COMMANDS
        .iter()
        .chain(advanced::COMMANDS)
        .chain(extensions::COMMANDS)
        .chain(graphify::COMMANDS)
        .chain(config_cmds::COMMANDS)
        .chain(setup::COMMANDS)
        .chain(commit::COMMANDS)
        .chain(workspace::COMMANDS)
        .chain(peers::COMMANDS)
        .chain(session::COMMANDS)
        .chain(modes::COMMANDS)
        .chain(maintenance::COMMANDS)
}

/// Routes `name` to the first group that handles it.
pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if let Some(result) = run_live(shell, app, name, args) {
        return Some(result);
    }
    if let Some(result) = advanced::run(shell, app, agent, name, args) {
        return Some(result);
    }
    if let Some(result) = maintenance::run(shell, app, agent, name, args) {
        return Some(result);
    }
    if let Some(result) = collab::run(shell, app, agent, name, args) {
        return Some(result);
    }
    if let Some(result) = smithery::run(shell, app, agent, name, args) {
        return Some(result);
    }
    if let Some(result) = run_settings(shell, app, Some(&mut *agent), name, args) {
        return Some(result);
    }
    if let Some(result) = extensions::run(shell, app, agent, name) {
        return Some(result);
    }
    if let Some(result) = commit::run(shell, app, agent, name, args) {
        return Some(result);
    }
    if let Some(result) = peers::run(shell, agent, name, args) {
        return Some(result);
    }
    if let Some(result) = workspace::run(shell, app, agent, name, args) {
        return Some(result);
    }
    if let Some(result) = session::run(shell, app, agent, name, args) {
        return Some(result);
    }
    if let Some(result) = setup::run_command(shell, app, agent, name, args) {
        return Some(result);
    }
    modes::run(shell, app, agent, name, args)
}

/// Group commands that need only the shell and app. The central `command_registry::during_turn`
/// policy permits live dispatch; the idle dispatcher reaches the same handlers through [`run`].
/// Add scheduling rules in that registry when adding handlers here (default: after the turn).
pub(super) fn run_live(
    shell: &mut Shell,
    app: &App,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    let args = args.trim();
    advanced::run_live(shell, app, name, args)
        .or_else(|| maintenance::run_live(shell, app, name, args))
        .or_else(|| collab::run_live(shell, app, name, args))
        .or_else(|| smithery::run_live(shell, app, name, args))
        .or_else(|| extensions::run_live(shell, app, name, args))
        .or_else(|| graphify::run_live(shell, app, name, args))
        .or_else(|| config_cmds::run_live(shell, app, name, args))
        .or_else(|| session::run_live(shell, app, name, args))
        .or_else(|| setup::run_live(shell, app, name, args))
}

/// Group commands that save settings (see [`Shell::save`]). `agent` is `None` during a turn; the
/// idle dispatcher passes the agent so the change applies at once.
pub(super) fn run_settings(
    shell: &mut Shell,
    app: &App,
    mut agent: Option<&mut Agent<'_>>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    let args = args.trim();
    if let Some(result) = advanced::run_settings(shell, app, agent.as_deref_mut(), name, args) {
        return Some(result);
    }
    if let Some(result) = extensions::run_settings(shell, app, agent.as_deref_mut(), name, args) {
        return Some(result);
    }
    if let Some(result) = config_cmds::run_settings(shell, app, agent.as_deref_mut(), name, args) {
        return Some(result);
    }
    modes::run_settings(shell, app, agent, name, args)
}

/// Composer argument candidates: `/<name> <subcommand>` for every static subcommand, plus each
/// group's dynamic candidates (sessions, servers, plugins).
pub(super) fn completions(config: &Config) -> Vec<String> {
    let mut candidates: Vec<String> = specs()
        .flat_map(|spec| {
            spec.subcommands
                .iter()
                .map(move |sub| format!("/{} {sub}", spec.name))
        })
        .collect();
    candidates.extend(collab::completions(config));
    candidates.extend(extensions::completions(config));
    candidates.extend(session::completions(config));
    candidates.extend(modes::completions(config));
    candidates
}
