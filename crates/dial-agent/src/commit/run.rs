//! Commit command runner: the agentic flow or the deterministic legacy flow.

use std::path::Path;

use super::agentic::{AgenticOutcome, run_agentic_commit};
use super::backend::CommitBackend;
use super::changelog::{ChangelogFlowArgs, run_changelog_flow};
use super::commit_types::CommitError;
use super::config::GenerationConfig;
use super::execute::{
    PreviewIndex, commit_with_message, git_output, git_output_indexed, push_or_abort, repo_top,
};
use super::model_selection::resolve_commit_config;
use super::repo_context::generation_context;
use super::types::CommitCommandArgs;
use super::{GenerationInput, format_conventional_commit, generate_with_backend};
use crate::config::types::Config;

/// Result of a commit run. Each variant states what happened; callers print it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitRunOutcome {
    /// No changes were staged. `pushed` reports whether existing commits were pushed (`--push`).
    NothingToCommit { pushed: bool },
    /// `--dry-run`: the generated message, plus a validation warning when it needs correction.
    DryRun {
        message: String,
        validation_error: Option<String>,
    },
    /// The message was generated but not committed (the user declined, or the agent did not commit).
    NotCommitted { message: String },
    /// A commit was created. `used_fallback` marks a mechanical fallback commit, a degraded outcome.
    Committed {
        message: String,
        pushed: bool,
        used_fallback: bool,
    },
    /// `--dry-run --push` with nothing staged: the existing commits that `--push` would publish
    /// were not pushed.
    WouldPush,
}

impl CommitRunOutcome {
    /// Process exit code: a fallback commit is degraded and exits 1; everything else exits 0.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Committed {
                used_fallback: true,
                ..
            } => 1,
            _ => 0,
        }
    }
}

impl CommitRunOutcome {
    /// Human-readable report of the run, shown in the TUI transcript and printed by `dial commit`.
    pub fn summary(&self) -> String {
        match self {
            Self::NothingToCommit { pushed: true } => {
                "No changes to commit; pushed existing commits.".to_owned()
            }
            Self::NothingToCommit { pushed: false } => "No changes to commit.".to_owned(),
            Self::WouldPush => "Dry run. No changes to commit; nothing was pushed.".to_owned(),
            Self::DryRun {
                message,
                validation_error: None,
            } => format!("Dry run. Message:\n{message}"),
            Self::DryRun {
                message,
                validation_error: Some(error),
            } => format!("Dry run. Message:\n{message}\n\nWarning: {error}"),
            Self::NotCommitted { message } => format!("Commit cancelled. Message was:\n{message}"),
            Self::Committed {
                message,
                pushed,
                used_fallback,
            } => {
                let mut text = format!("Commit created.\n{message}");
                if *pushed {
                    text = format!("{text}\n\nPushed to remote.");
                }
                if *used_fallback {
                    text = format!(
                        "{text}\n\nWarning: the agent did not finish; a fallback commit was used."
                    );
                }
                text
            }
        }
    }
}

/// Run the commit command. `confirm` receives the generated message and returns whether to commit.
/// The legacy flow consults it before committing; non-interactive callers pass `&mut |_| true`.
/// The agentic flow commits from inside its own tool loop, so `confirm` does not apply to it.
pub fn run_commit_command(
    args: &CommitCommandArgs,
    cwd: &Path,
    config: &Config,
    backend: &dyn CommitBackend,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> Result<CommitRunOutcome, CommitError> {
    // Paths from git are top-level relative, but pathspecs resolve from the working directory.
    let top = repo_top(cwd)?;
    let cwd = top.as_path();
    // A dry run stages and diffs through a private copy of the index, so the repository's index is
    // never written: a concurrent `git add` is never overwritten, and nothing needs restoring.
    let preview = args
        .dry_run
        .then(|| PreviewIndex::capture(cwd))
        .transpose()?;
    let index = preview.as_ref().map(PreviewIndex::path);
    run_commit_steps(args, cwd, index, config, backend, confirm)
}

fn run_commit_steps(
    args: &CommitCommandArgs,
    cwd: &Path,
    index: Option<&Path>,
    config: &Config,
    backend: &dyn CommitBackend,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> Result<CommitRunOutcome, CommitError> {
    if args.legacy {
        return run_legacy_commit(args, cwd, index, config, backend, confirm);
    }
    let cfg = resolve_commit_config(config, args.model.as_deref());
    let AgenticOutcome {
        used_fallback,
        committed,
        message,
    } = run_agentic_commit(args, cwd, index, &cfg, backend, confirm)?;
    if !committed {
        return Ok(if args.dry_run {
            CommitRunOutcome::DryRun {
                message,
                validation_error: None,
            }
        } else {
            CommitRunOutcome::NotCommitted { message }
        });
    }
    // The single dry-run effect policy: a dry run never publishes.
    let pushed = args.push && !args.dry_run;
    if pushed {
        push_or_abort(cwd, config.git.bypass_hooks)?;
    }
    Ok(CommitRunOutcome::Committed {
        message,
        pushed,
        used_fallback,
    })
}

/// Deterministic legacy pipeline: stage everything if nothing is staged, generate one conventional
/// message from the staged diff, then (after confirmation) update the changelog, commit, and push.
/// `index` is the private preview index for a dry run, or `None` for the repository's own index.
fn run_legacy_commit(
    args: &CommitCommandArgs,
    cwd: &Path,
    index: Option<&Path>,
    config: &Config,
    backend: &dyn CommitBackend,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> Result<CommitRunOutcome, CommitError> {
    const CONTEXT: &str = "Commit generation failed";
    let cfg = resolve_commit_config(config, args.model.as_deref());
    // The single dry-run effect policy: nothing below publishes unless `publish` is set, and a
    // dry run reports what it would push instead.
    let publish = args.push && !args.dry_run;

    if git_output_indexed(cwd, index, &["diff", "--cached", "--name-only"], CONTEXT)?
        .trim()
        .is_empty()
    {
        git_output_indexed(cwd, index, &["add", "-A"], CONTEXT)?;
    }
    let diff = git_output_indexed(
        cwd,
        index,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
        ],
        CONTEXT,
    )?;
    if diff.trim().is_empty() {
        if args.dry_run && args.push {
            return Ok(CommitRunOutcome::WouldPush);
        }
        if publish {
            push_or_abort(cwd, config.git.bypass_hooks)?;
            return Ok(CommitRunOutcome::NothingToCommit { pushed: true });
        }
        return Ok(CommitRunOutcome::NothingToCommit { pushed: false });
    }
    let stat = git_output_indexed(
        cwd,
        index,
        &["diff", "--cached", "--no-color", "--stat", "--no-renames"],
        CONTEXT,
    )?;
    let numstat = git_output_indexed(
        cwd,
        index,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--numstat",
            "--no-renames",
        ],
        CONTEXT,
    )?;

    let mut context = generation_context(cwd);
    context.user_context = args.context.clone();
    let input = GenerationInput {
        diff: &diff,
        stat: &stat,
        numstat: &numstat,
        context: &context,
    };
    let result = generate_with_backend(
        backend,
        &cfg,
        &GenerationConfig::from_settings(&cfg.commit),
        input,
    )?;
    let message = format_conventional_commit(&result.commit);

    if args.dry_run {
        return Ok(CommitRunOutcome::DryRun {
            message,
            validation_error: result.validation_error,
        });
    }
    if let Some(error) = result.validation_error {
        return Err(CommitError::Validation(format!(
            "Generated commit message failed validation: {error}"
        )));
    }
    if !confirm(&message) {
        return Ok(CommitRunOutcome::NotCommitted { message });
    }
    if !args.no_changelog {
        update_changelog(cwd, &cfg, backend)?;
    }
    commit_with_message(cwd, &message, config.git.bypass_hooks)?;
    if publish {
        push_or_abort(cwd, config.git.bypass_hooks)?;
    }
    Ok(CommitRunOutcome::Committed {
        message,
        pushed: publish,
        used_fallback: false,
    })
}

/// Changelog step of the legacy flow: update changelogs for the staged files, then stage the files
/// the flow wrote so they land in the same commit.
fn update_changelog(
    cwd: &Path,
    cfg: &Config,
    backend: &dyn CommitBackend,
) -> Result<(), CommitError> {
    const CONTEXT: &str = "Changelog update failed";
    let staged = git_output(cwd, &["diff", "--cached", "--name-only", "-z"], CONTEXT)?;
    let staged_files: Vec<String> = staged
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    let outcome = run_changelog_flow(&ChangelogFlowArgs {
        cwd,
        config: cfg,
        backend,
        staged_files: &staged_files,
        dry_run: false,
    })?;
    if outcome.changed_files.is_empty() {
        return Ok(());
    }
    let mut add = vec!["add", "--"];
    add.extend(outcome.changed_files.iter().map(String::as_str));
    git_output(cwd, &add, CONTEXT)?;
    Ok(())
}

#[cfg(test)]
mod tests;
