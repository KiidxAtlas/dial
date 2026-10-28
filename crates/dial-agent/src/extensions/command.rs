//! Slash commands an extension answers over its JSON-RPC protocol. A command replies with text, or
//! with a panel: the host shows it through `Ui::run_panel`, then reports the typed outcome back to
//! the same extension, which may reply with text. A panel is human input and never grants a tool.
use super::panel::{Outcome, outcome_to_json, panel_from_json};
use crate::agent::Ui;
use crate::tools::integrations::plugin::Host;
use anyhow::{Result, bail};
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
    let reply = host.request(
        plugin,
        "command",
        json!({"name": command, "args": args}),
        cancel,
    )?;
    let Some(panel) = reply.get("panel") else {
        return Ok(text_of(&reply));
    };
    let panel = panel_from_json(panel)?;
    let outcome = ui.run_panel(&panel, cancel);
    let closed = host.request(
        plugin,
        "command.panel",
        json!({
            "name": command,
            "generation": panel.generation,
            "outcome": outcome_to_json(&outcome),
        }),
        cancel,
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
