//! `dial rules`: inspect the stream rules this workspace loads, without a model.
//!
//! `list` shows every loaded rule with its triggers, interrupt mode and scopes, plus the files that
//! were refused while loading. `test` runs one sample (assistant text, reasoning, or a tool call)
//! through every stream rule exactly as a live inference would and reports which rules fire and
//! whether each would interrupt or defer. Judged `question` rules are listed, never asked.

use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::agent::safety::rule_inspect::{self, Probe, Sample};
use crate::config::paths::Layout;
use crate::config::types::Config;
use crate::llm::ToolCall;

#[derive(Args, Debug, Clone)]
pub struct RulesArgs {
    #[command(subcommand)]
    pub action: Option<RulesAction>,
    /// Emit JSON.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum RulesAction {
    /// List loaded rules and refused rule files (default).
    List,
    /// Report which stream rules a sample output would fire. Exits 1 when any rule fires.
    Test {
        /// Assistant text to test; `-` reads standard input.
        #[arg(conflicts_with_all = ["reasoning", "tool"])]
        text: Option<String>,
        /// Test this text as model reasoning instead of answer text (`-` reads standard input).
        #[arg(long, conflicts_with = "tool")]
        reasoning: Option<String>,
        /// Test a tool call with this tool name; `--args` holds its JSON arguments.
        #[arg(long, requires = "args")]
        tool: Option<String>,
        /// JSON arguments of the `--tool` call.
        #[arg(long, requires = "tool")]
        args: Option<String>,
        /// Agent name the rules' `agents` filter sees.
        #[arg(long, default_value = "main")]
        agent: String,
    },
}

/// Runs `dial rules`. Returns the process exit code.
pub fn run(layout: &Layout, workspace: &Path, config: &Config, args: &RulesArgs) -> Result<i32> {
    match args.action.clone().unwrap_or(RulesAction::List) {
        RulesAction::List => {
            let listing = rule_inspect::list(layout, workspace, config);
            if args.json {
                println!("{}", serde_json::to_string_pretty(&listing)?);
            } else {
                print!("{}", render_listing(&listing));
            }
            Ok(0)
        }
        RulesAction::Test {
            text,
            reasoning,
            tool,
            args: arguments,
            agent,
        } => {
            let sample = sample(text, reasoning, tool, arguments)?;
            let probe = rule_inspect::test(layout, workspace, config, &agent, &sample)
                .map_err(anyhow::Error::msg)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&probe)?);
            } else {
                print!("{}", render_probe(&probe));
            }
            Ok(i32::from(
                !probe.matches.is_empty() || !probe.judged.is_empty(),
            ))
        }
    }
}

fn sample(
    text: Option<String>,
    reasoning: Option<String>,
    tool: Option<String>,
    arguments: Option<String>,
) -> Result<Sample> {
    if let Some(name) = tool {
        let raw = arguments.unwrap_or_default();
        let arguments: serde_json::Value =
            serde_json::from_str(&raw).context("--args must be a JSON value")?;
        return Ok(Sample::Tool(ToolCall {
            id: "dial-rules-test".into(),
            name,
            arguments,
            malformed_arguments: false,
        }));
    }
    if let Some(reasoning) = reasoning {
        return Ok(Sample::Thinking(read_text(reasoning)?));
    }
    match text {
        Some(text) => Ok(Sample::Text(read_text(text)?)),
        None => bail!("rules test needs sample text, --reasoning TEXT, or --tool NAME --args JSON"),
    }
}

fn read_text(value: String) -> Result<String> {
    if value != "-" {
        return Ok(value);
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
        .context("reading the sample from standard input")?;
    Ok(text)
}

fn render_listing(listing: &rule_inspect::Listing) -> String {
    let mut out = String::new();
    if !listing.enabled {
        out.push_str("Portable rules are disabled (portable_rules.enabled = false).\n");
    }
    if listing.rules.is_empty() {
        out.push_str("No rules loaded.\n");
    }
    for rule in &listing.rules {
        out.push_str(&format!("{}\n", rule.source));
        out.push_str(&format!("  triggers: {}", rule.triggers.join(", ")));
        if rule
            .triggers
            .iter()
            .any(|t| t == "condition" || t == "astCondition")
        {
            out.push_str(&format!("; interrupt: {}", rule.interrupt_mode));
        }
        out.push('\n');
        if let Some(question) = &rule.question {
            out.push_str(&format!("  question: {question}\n"));
        }
        if rule
            .triggers
            .iter()
            .any(|t| t != "globs" && t != "alwaysApply")
        {
            out.push_str(&format!("  scopes: {}\n", rule.scopes.join(", ")));
        }
        if !rule.globs.is_empty() {
            out.push_str(&format!("  globs: {}\n", rule.globs.join(", ")));
        }
    }
    for warning in &listing.warnings {
        out.push_str(&format!("refused: {warning}\n"));
    }
    out
}

fn render_probe(probe: &Probe) -> String {
    let mut out = String::new();
    if probe.matches.is_empty() && probe.judged.is_empty() {
        out.push_str("No stream rule fires on this sample.\n");
    }
    for hit in &probe.matches {
        let effect = if hit.trigger == "interrupt" {
            "interrupts the inference"
        } else {
            "defers its reminder to the next request"
        };
        out.push_str(&format!("{}: {effect}\n", hit.source));
    }
    for source in &probe.judged {
        out.push_str(&format!("{source}: would be asked of the judge role\n"));
    }
    out
}

#[cfg(test)]
mod tests;
