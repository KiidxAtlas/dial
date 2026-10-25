//! `/peers` is a text-only adapter over the agent's existing coordination API.
use super::CommandSpec;
use crate::agent::Agent;
use crate::app::{Shell, Step};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

const USAGE: &str = "Usage: /peers [send <id> <text>|claim <paths...>|clear]";

pub(super) const COMMANDS: &[CommandSpec] = &[CommandSpec {
    name: "peers",
    description: "list same-checkout sessions, send a message or declare advisory path claims",
    args_hint: "[send <id> <text>|claim <paths...>|clear]",
    subcommands: &["send", "claim", "clear"],
}];

pub(super) fn run(
    shell: &mut Shell,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if name != "peers" {
        return None;
    }
    let text = match arguments(args) {
        Ok(args) => agent.peer_coordination(&args).text,
        Err(error) => error.to_string(),
    };
    shell.note("Project peers", text);
    Some(Ok(Step::Done))
}

fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(char::is_whitespace) {
        Some(index) => (&text[..index], text[index..].trim_start()),
        None => (text, ""),
    }
}

fn arguments(args: &str) -> Result<Value> {
    let (action, rest) = split_word(args);
    match action {
        "" => Ok(json!({"action": "list"})),
        "clear" if rest.is_empty() => Ok(json!({"action": "clear"})),
        "send" => {
            let (to, text) = split_word(rest);
            if to.is_empty() || text.is_empty() {
                bail!(USAGE);
            }
            Ok(json!({"action": "send", "to": to, "text": text}))
        }
        "claim" => {
            let scopes = shlex::split(rest)
                .ok_or_else(|| anyhow!("Unclosed quote in /peers claim paths. {USAGE}"))?;
            if scopes.is_empty() || scopes.iter().any(String::is_empty) {
                bail!(USAGE);
            }
            Ok(json!({"action": "claim", "scopes": scopes}))
        }
        _ => bail!(USAGE),
    }
}

#[cfg(test)]
mod tests;
