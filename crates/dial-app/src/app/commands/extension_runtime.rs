//! Operator entry points for registered runtime contributions. These route through the owning
//! agent controller; UI interaction and declarative registration never authorize tool execution.
use super::CommandSpec;
use crate::agent::Agent;
use crate::app::{App, Bridge, PAINT_EVERY, Shell, Step};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Instant;

pub(super) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "extension",
        description: "run extension workflows, profiles and context sources; inspect or restart workers",
        args_hint: "[list|diagnostics|workflow|profile|context|stop|restart] [owner/id] [arguments]",
        subcommands: &[
            "list",
            "diagnostics",
            "workflow",
            "profile",
            "context",
            "stop",
            "restart",
        ],
    },
    CommandSpec {
        name: "extension-view",
        description: "list, show, hide or focus a persistent extension workspace",
        args_hint: "[list | owner/id [show|hide|focus]]",
        subcommands: &["list"],
    },
    CommandSpec {
        name: "extension-keybindings",
        description: "inspect extension shortcuts and override conflicts",
        args_hint: "[global|workspace] [list]",
        subcommands: &["global", "workspace", "list"],
    },
    CommandSpec {
        name: "extension-settings",
        description: "edit an extension's typed settings",
        args_hint: "owner [global|workspace]",
        subcommands: &[],
    },
];

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if !COMMANDS.iter().any(|spec| spec.name == name) {
        return None;
    }
    Some(run_inner(shell, app, agent, name, args.trim()))
}

fn run_inner(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Result<Step> {
    if name == "extension-view" {
        let text = shell.extension_view_command(args)?;
        if !text.is_empty() {
            shell.note("Extension views", text);
        }
        return Ok(Step::Done);
    }
    if name == "extension-keybindings" {
        let text = shell.extension_keybindings_command(app, agent, args)?;
        if !text.is_empty() {
            shell.note("Extension keybindings", text);
        }
        return Ok(Step::Done);
    }
    if name == "extension-settings" {
        let text = shell.extension_settings_command(app, agent, args)?;
        if !text.is_empty() {
            shell.note("Extension settings", text);
        }
        return Ok(Step::Done);
    }
    let host = agent
        .extension_host()
        .context("no extension host is active")?;
    let (operation, rest) = split_word(args);
    let operation = if operation.is_empty() {
        "list"
    } else {
        operation
    };
    if matches!(operation, "list" | "diagnostics") {
        ensure!(rest.is_empty(), "usage: /extension {operation}");
        let entries: Vec<Value> = host.loaded_names().into_iter().map(|owner| {
            let registration = host.registration(&owner).map(|(id, protocol)| json!({"generation":id,"protocol":protocol.version,"capabilities":protocol.capabilities}));
            json!({"owner":owner,"registration":registration,"contributions":host.contributions(&owner)})
        }).collect();
        shell.note(
            "Runtime extensions",
            serde_json::to_string_pretty(&entries)?,
        );
        return Ok(Step::Done);
    }
    let (target, payload) = split_word(rest);
    ensure!(
        !target.is_empty(),
        "an extension owner or owner/id is required"
    );
    if matches!(operation, "stop" | "restart") {
        ensure!(payload.is_empty(), "usage: /extension {operation} owner");
        if operation == "stop" {
            ensure!(host.unregister(target), "extension {target} is not loaded");
        } else {
            host.restart(target, &app.interrupt)?;
        }
        shell.note(
            "Runtime extension",
            format!("{target}: {operation} complete"),
        );
        return Ok(Step::Done);
    }
    let (owner, id) = target.split_once('/').context("expected owner/id")?;
    ensure!(
        !owner.is_empty() && !id.is_empty() && !id.contains('/'),
        "expected owner/id"
    );
    let mut bridge = Bridge {
        app,
        shell,
        interrupt: app.interrupt.clone(),
        started: Instant::now(),
        last_paint: Instant::now() - PAINT_EVERY,
        failed: false,
        pending: Vec::new(),
        next_id: 0,
    };
    let result = match operation {
        "workflow" => {
            let arguments = if payload.is_empty() {
                json!({})
            } else {
                serde_json::from_str(payload).context("workflow arguments must be JSON")?
            };
            ensure!(
                arguments.is_object(),
                "workflow arguments must be an object"
            );
            agent.run_extension_workflow(&mut bridge, &host, owner, id, arguments)?
        }
        "profile" => {
            ensure!(!payload.is_empty(), "profile requires a task description");
            agent.run_extension_profile(&mut bridge, &host, owner, id, payload)?
        }
        "context" => {
            json!({"context":agent.resolve_extension_context(&mut bridge, &host, owner, id, payload)?})
        }
        _ => bail!(
            "unknown operation {operation}; use list, diagnostics, workflow, profile, context, stop or restart"
        ),
    };
    bridge
        .shell
        .note("Extension", serde_json::to_string_pretty(&result)?);
    Ok(Step::Done)
}

fn split_word(text: &str) -> (&str, &str) {
    text.find(char::is_whitespace)
        .map_or((text, ""), |at| (&text[..at], text[at..].trim_start()))
}
