//! `dial review`: the TUI `/review` workflow as a non-interactive print-mode command.
//!
//! The target is frozen by the same `Agent::review_target` the TUI calls, and the guidance is the
//! `review` command template (a project or user `commands/review.md` overrides it, as in the TUI).
//! The run is read-only: the print path forces the approval policy to `readonly`, and reviewer
//! subagents have no write tools.

use crate::agent::window::context::{discover_commands, expand_command};
use crate::config::paths::Layout;
use crate::repl::OutputFormat;
use anyhow::{Result, bail, ensure};
use clap::Args as ClapArgs;
use std::path::PathBuf;

/// Review target flags. `--uncommitted` is the default when none is given; the four target kinds
/// are mutually exclusive.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Review staged, unstaged and untracked changes (the default).
    #[arg(long, conflicts_with_all = ["base", "commit", "prompt"])]
    pub uncommitted: bool,
    /// Review the changes between this branch or ref and HEAD.
    #[arg(long, value_name = "BRANCH", conflicts_with_all = ["commit", "prompt"])]
    pub base: Option<String>,
    /// Review the changes introduced by this commit.
    #[arg(long, value_name = "SHA", conflicts_with = "prompt")]
    pub commit: Option<String>,
    /// Review the uncommitted changes with this focus (free-form instructions).
    #[arg(value_name = "PROMPT")]
    pub prompt: Vec<String>,
    /// Stdout format: text, one JSON result, or JSON events (overrides the top-level flag).
    #[arg(long, value_enum)]
    pub output_format: Option<OutputFormat>,
    /// Write the review report to FILE as well as stdout (atomic; see `docs/rpc-and-acp.md`).
    #[arg(short = 'o', long = "output-last-message", value_name = "FILE")]
    pub output_last_message: Option<PathBuf>,
}

/// A resolved review target: the `/review` target words and any extra focus instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub target: String,
    pub instructions: String,
}

/// What the print path runs: the frozen target words and the expanded guidance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub target: String,
    pub guidance: String,
}

impl Args {
    /// Resolve the flags into a target. Refs must be single words so the target survives the
    /// `/review` word split unchanged.
    pub fn request(&self) -> Result<Request> {
        let targets = usize::from(self.uncommitted)
            + usize::from(self.base.is_some())
            + usize::from(self.commit.is_some())
            + usize::from(!self.prompt.is_empty());
        ensure!(
            targets <= 1,
            "choose one review target: --uncommitted, --base, --commit or a prompt"
        );
        if let Some(base) = &self.base {
            return Ok(Request {
                target: format!("base {}", single_word("--base", base)?),
                instructions: String::new(),
            });
        }
        if let Some(commit) = &self.commit {
            return Ok(Request {
                target: format!("commit {}", single_word("--commit", commit)?),
                instructions: String::new(),
            });
        }
        let instructions = self.prompt.join(" ").trim().to_owned();
        if !self.prompt.is_empty() && instructions.is_empty() {
            bail!("the review prompt is empty");
        }
        Ok(Request {
            target: "uncommitted".into(),
            instructions,
        })
    }

    /// The run for this project: the target plus the `review` guidance.
    pub fn run(&self, layout: &Layout) -> Result<Run> {
        let request = self.request()?;
        Ok(Run {
            guidance: guidance(layout, &request),
            target: request.target,
        })
    }
}

fn single_word(flag: &str, value: &str) -> Result<String> {
    let value = value.trim();
    ensure!(
        !value.is_empty() && !value.starts_with('-') && !value.chars().any(char::is_whitespace),
        "{flag} takes one branch or commit name"
    );
    Ok(value.to_owned())
}

/// The review guidance: the `review` command template expanded with the target, plus any focus.
pub fn guidance(layout: &Layout, request: &Request) -> String {
    let template = discover_commands(layout)
        .into_iter()
        .find(|def| def.name == "review")
        .map(|def| expand_command(&def, &request.target))
        .unwrap_or_default();
    if request.instructions.is_empty() {
        template
    } else {
        format!(
            "{template}\nAdditional review focus: {}",
            request.instructions
        )
    }
}

#[cfg(test)]
mod tests;
