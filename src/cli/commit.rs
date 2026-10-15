//! `dial commit`: generate and create a commit for the working tree without the TUI.
//! Changes are committed without confirmation.

use std::path::Path;

use anyhow::Result;

use crate::commit::cli::{commit_help, parse_commit_args};
use crate::commit::run::{ProviderBackend, run_commit_command};
use crate::config::types::Config;

/// Run `dial commit` with the raw arguments after the subcommand. `global_model` is the top-level
/// `--model`, which clap captures before the subcommand sees it; it applies unless `-m` was given.
/// Returns the process exit code: 1 for a fallback commit or a bad argument, 0 otherwise.
pub fn run(
    config: &Config,
    cwd: &Path,
    args: &[String],
    global_model: Option<&str>,
) -> Result<i32> {
    let mut parsed = match parse_commit_args(args) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            println!("{}", commit_help());
            return Ok(0);
        }
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    if parsed.model.is_none() {
        parsed.model = global_model.map(str::to_owned);
    }
    let outcome = run_commit_command(&parsed, cwd, config, &ProviderBackend, &mut |_: &str| true)?;
    println!("{}", outcome.summary());
    Ok(outcome.exit_code())
}
