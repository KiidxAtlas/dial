//! Argument parsing and help text for the commit command.

use super::types::CommitCommandArgs;

/// Parse the arguments that follow `commit` (`dial commit <args...>` or `/commit <args...>`).
///
/// Returns `Ok(None)` when `--help`/`-h` is requested, so the caller prints help and exits.
/// Errors carry the messages the command prints before exiting.
pub(crate) fn parse_commit_args(args: &[String]) -> Result<Option<CommitCommandArgs>, String> {
    let mut result = CommitCommandArgs::default();
    let mut help = false;
    let mut i = 0;
    while i < args.len() {
        let raw = args[i].as_str();
        let flag = match raw {
            "-c" => "--context",
            "-m" => "--model",
            other => other,
        };
        match flag {
            "--push" => result.push = true,
            "--dry-run" => result.dry_run = true,
            "--no-changelog" => result.no_changelog = true,
            "--legacy" => result.legacy = true,
            "--context" => {
                result.context = Some(take_value(args, &mut i, "--context")?);
            }
            "--model" => {
                result.model = Some(take_value(args, &mut i, "--model")?);
            }
            "--help" | "-h" => help = true,
            other if other.starts_with('-') => {
                return Err(format!("Error: Unknown flag {other}"));
            }
            _ => {}
        }
        i += 1;
    }
    if help {
        return Ok(None);
    }
    Ok(Some(result))
}

/// Consume the value following a flag; the value must exist and must not look like a flag.
fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    match args.get(*i + 1) {
        Some(value) if !value.is_empty() && !value.starts_with('-') => {
            *i += 1;
            Ok(value.clone())
        }
        _ => Err(format!("Error: {flag} requires a value")),
    }
}

/// Help text for the commit command.
pub(crate) fn commit_help() -> &'static str {
    "Usage:\n  dial commit [options]\n\nOptions:\n  --push           Push after committing\n  --dry-run        Preview without committing\n  --no-changelog   Skip changelog updates\n  --legacy         Use legacy deterministic pipeline\n  --context, -c    Additional context for the model\n  --model, -m      Override model selection\n  --help, -h       Show this help message\n"
}

#[cfg(test)]
mod tests;
