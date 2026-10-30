//! Slash commands an extension answers over its JSON-RPC protocol. A command replies with text, or
//! with a panel: the host shows it through `Ui::run_panel`, then reports the typed outcome back to
//! the same extension, which may reply with text. A panel is human input and never grants a tool.
use super::panel::{Outcome, outcome_to_json, panel_from_json};
use crate::agent::Ui;
use crate::tools::integrations::plugin::Host;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

/// Runs `command` through the extension `plugin` and returns the text to show, possibly empty.
/// Every exchange goes through `host.request`, so unloading the extension fails the open or the
/// close instead of reaching a removed process.
pub fn run(
    ui: &mut dyn Ui,
    host: &Host,
    plugin: &str,
    command: &str,
    args: &str,
    cancel: &AtomicBool,
) -> Result<String> {
    run_with_host(
        ui,
        host,
        plugin,
        command,
        args,
        cancel,
        &mut |_, method, _| bail!("host API {method} requires an agent controller"),
    )
}

/// Runs a command with an execution-owner callback. Panels remain human input, not authorization.
pub fn run_with_host(
    ui: &mut dyn Ui,
    host: &Host,
    plugin: &str,
    command: &str,
    args: &str,
    cancel: &AtomicBool,
    callback: &mut dyn FnMut(&mut dyn Ui, &str, &Value) -> Result<Value>,
) -> Result<String> {
    let mut request = json!({"name":command,"args":args});
    if let Some(definition) = host
        .contributions(plugin)
        .and_then(|c| c.commands.into_iter().find(|c| c.name == command))
    {
        // The default open object retains legacy free-form command arguments. An explicitly
        // constrained schema opts into structured JSON without removing the original args string.
        if definition.parameters != json!({"type":"object"}) {
            let value: Value =
                serde_json::from_str(if args.trim().is_empty() { "{}" } else { args })
                    .context("command arguments must be JSON matching its registered schema")?;
            crate::runtime::schema::validate(&definition.parameters, &value)
                .map_err(anyhow::Error::msg)?;
            request["arguments"] = value;
        }
    }
    let reply =
        host.request_with_host(plugin, "command", request, cancel, &mut |method, params| {
            callback(ui, method, params)
        })?;
    let Some(panel) = reply.get("panel") else {
        return Ok(text_of(&reply));
    };
    let panel = panel_from_json(panel)?;
    let outcome = ui.run_panel(&panel, cancel);
    let closed = host.request_with_host(
        plugin,
        "command.panel",
        json!({
            "name": command,
            "generation": panel.generation,
            "outcome": outcome_to_json(&outcome),
        }),
        cancel,
        &mut |method, params| callback(ui, method, params),
    )?;
    match outcome {
        Outcome::Unsupported(reason) => bail!("{command}: {reason}"),
        Outcome::Submitted { .. } | Outcome::Cancelled => Ok(text_of(&closed)),
    }
}

fn text_of(reply: &Value) -> String {
    reply
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests;
