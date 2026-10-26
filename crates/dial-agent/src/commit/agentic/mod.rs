//! Agentic commit flow: the model proposes one commit or a split plan through
//! tools, changelog entries are applied, and the commit is created. A deterministic fallback covers failures.

pub(crate) mod agent;
pub(crate) mod fallback;
pub(crate) mod lock_files;
pub(crate) mod stage;
pub(crate) mod state;
pub(crate) mod template;
pub(crate) mod tools;
pub(crate) mod topo_sort;
pub(crate) mod trivial;
pub(crate) mod validation;

use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use agent::{ExistingEntries, run_agent};
use fallback::generate_fallback_proposal;
use lock_files::assign_lock_files_to_plan;
use stage::{StagedSnapshot, execute_split, preview_split};
use state::{
    ChangelogProposal, CommitAgentState, CommitProposal, ConventionalAnalysis, FileChange,
    SplitCommitPlan,
};
use tools::{ToolEnv, parse_numstat};
use topo_sort::compute_dependency_order;
use trivial::{TrivialChange, detect_trivial_change};

use crate::commit::CommitError;
use crate::commit::backend::CommitBackend;
use crate::commit::changelog::detect::detect_targets;
use crate::commit::changelog::{apply_changelog_proposals, read_unreleased};
use crate::commit::commit_types::format_conventional_commit;
use crate::commit::execute::{commit_with_message, git_output_indexed};
use crate::commit::types::CommitCommandArgs;
use crate::config::types::Config;

/// Result of an agentic commit run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgenticOutcome {
    pub used_fallback: bool,
    pub committed: bool,
    pub message: String,
}

/// Which commit plan is in hand.
enum Plan {
    Single(CommitProposal),
    Split(SplitCommitPlan),
}

/// Test and operator switches read from the environment.
#[derive(Debug, Clone, Copy, Default)]
struct Mode {
    /// `DIAL_COMMIT_TEST_FALLBACK`: skip the model and use the deterministic fallback.
    force_fallback: bool,
    /// `DIAL_COMMIT_NO_FALLBACK`: an agent failure is an error instead of a fallback.
    no_fallback: bool,
}

fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("true") | Ok("1"))
}

/// Run the agentic commit flow for the staged changes in `cwd`. Dry runs create no commit and write no changelog.
pub(crate) fn run_agentic_commit(
    args: &CommitCommandArgs,
    cwd: &Path,
    index: Option<&Path>,
    config: &Config,
    backend: &dyn CommitBackend,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> Result<AgenticOutcome, CommitError> {
    run_with(
        args,
        cwd,
        index,
        config,
        backend,
        confirm,
        Mode {
            force_fallback: env_flag("DIAL_COMMIT_TEST_FALLBACK"),
            no_fallback: env_flag("DIAL_COMMIT_NO_FALLBACK"),
        },
    )
}

/// Ask the user to confirm a split plan. Non-interactive runs accept without asking.
fn split_confirmed(commits: usize, confirm: &mut dyn FnMut(&str) -> bool) -> bool {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return true;
    }
    confirm(&format!(
        "Split commit plan has {commits} commits. Proceed? (y/N): "
    ))
}

fn staged_files(cwd: &Path, index: Option<&Path>) -> Result<Vec<String>, CommitError> {
    // `-z` keeps paths unquoted; without it git quotes non-ASCII names and `git add` cannot find them.
    let output = git_output_indexed(
        cwd,
        index,
        &["diff", "--cached", "--name-only", "--no-renames", "-z"],
        "Failed to list staged files",
    )?;
    Ok(output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

fn trivial_proposal(trivial: &TrivialChange) -> CommitProposal {
    CommitProposal {
        analysis: ConventionalAnalysis {
            commit_type: trivial.commit_type.to_owned(),
            scope: None,
            details: Vec::new(),
            issue_refs: Vec::new(),
        },
        summary: trivial.summary.to_owned(),
        warnings: Vec::new(),
    }
}

/// Extract the plan from a finished agent run. `None` when neither commit proposal exists.
fn agent_plan(mut state: CommitAgentState) -> Option<(Plan, Option<ChangelogProposal>)> {
    let changelog = state.changelog_proposal.take();
    if let Some(proposal) = state.proposal.take() {
        return Some((Plan::Single(proposal), changelog));
    }
    state
        .split_proposal
        .take()
        .map(|plan| (Plan::Split(plan), changelog))
}

/// Apply changelog proposals and return the changed paths. Warnings go to stderr.
fn apply_changelog(
    cwd: &Path,
    changelog: Option<&ChangelogProposal>,
    dry_run: bool,
) -> Result<Vec<String>, CommitError> {
    let Some(changelog) = changelog else {
        return Ok(Vec::new());
    };
    let outcome = apply_changelog_proposals(cwd, &changelog.entries, dry_run)?;
    for warning in &outcome.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(outcome.changed)
}

fn run_with(
    args: &CommitCommandArgs,
    cwd: &Path,
    index: Option<&Path>,
    config: &Config,
    backend: &dyn CommitBackend,
    confirm: &mut dyn FnMut(&str) -> bool,
    mode: Mode,
) -> Result<AgenticOutcome, CommitError> {
    // Staging, diffs and tool reads all go through `index`: the repository's own index, or a
    // private copy for a dry run.
    let mut staged = staged_files(cwd, index)?;
    if staged.is_empty() {
        git_output_indexed(cwd, index, &["add", "-A"], "Failed to stage changes")?;
        staged = staged_files(cwd, index)?;
    }
    if staged.is_empty() {
        return Err(CommitError::Validation("No changes to commit".to_owned()));
    }
    let full_diff = git_output_indexed(
        cwd,
        index,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
        ],
        "Failed to read staged diff",
    )?;
    // The staged state is captured before the model runs; a split is executed against it only.
    let mut snapshot = StagedSnapshot::capture(cwd, index, &full_diff)?;
    let numstat_text = git_output_indexed(
        cwd,
        index,
        &["diff", "--cached", "--numstat", "--no-renames", "-z"],
        "Failed to read numstat",
    )?;
    let numstat = parse_numstat(&numstat_text);

    let targets: Vec<String> = if args.no_changelog {
        Vec::new()
    } else {
        detect_targets(cwd, &staged)
            .into_iter()
            .map(|target| target.path)
            .collect()
    };

    let (plan, changelog, used_fallback) = if let Some(trivial) = detect_trivial_change(&full_diff)
    {
        (Plan::Single(trivial_proposal(&trivial)), None, false)
    } else if mode.force_fallback {
        (
            Plan::Single(generate_fallback_proposal(&numstat)),
            None,
            true,
        )
    } else {
        let existing: Vec<ExistingEntries> = targets
            .iter()
            .filter_map(|path| read_unreleased(cwd, path).map(|sections| (path.clone(), sections)))
            .collect();
        // No interrupt reaches the commit flow yet; the flag is the seam `analyze_files` checks
        // before admitting each model call.
        let cancel = AtomicBool::new(false);
        let mut env = ToolEnv {
            root: cwd,
            config,
            backend,
            staged: staged.clone(),
            numstat: numstat.clone(),
            index: index.map(Path::to_path_buf),
            cancel: &cancel,
            changelog_targets: targets,
            state: CommitAgentState::default(),
        };
        let agent_result = run_agent(&mut env, args.context.as_deref(), &existing).and_then(|()| {
            agent_plan(std::mem::take(&mut env.state))
                .ok_or_else(|| "agent finished without a commit proposal".to_owned())
        });
        match agent_result {
            Ok((plan, changelog)) => (plan, changelog, false),
            Err(error) if mode.no_fallback => return Err(CommitError::Model(error)),
            Err(error) => {
                eprintln!("warning: commit agent failed, using fallback: {error}");
                (
                    Plan::Single(generate_fallback_proposal(&numstat)),
                    None,
                    true,
                )
            }
        }
    };

    match plan {
        Plan::Single(proposal) => {
            let changed = apply_changelog(cwd, changelog.as_ref(), args.dry_run)?;
            if !args.dry_run && !changed.is_empty() {
                let mut add: Vec<&str> = vec!["add", "--"];
                add.extend(changed.iter().map(String::as_str));
                git_output_indexed(cwd, index, &add, "Failed to stage changelog updates")?;
            }
            let message = format_conventional_commit(&proposal.to_message());
            if !args.dry_run {
                commit_with_message(cwd, &message, config.git.bypass_hooks)?;
            }
            Ok(AgenticOutcome {
                used_fallback,
                committed: !args.dry_run,
                message,
            })
        }
        Plan::Split(mut plan) => {
            assign_lock_files_to_plan(&mut plan, &staged);
            let order = compute_dependency_order(&plan.commits).map_err(CommitError::Validation)?;
            if !args.dry_run && !split_confirmed(plan.commits.len(), confirm) {
                println!("Split commit aborted by user.");
                return Ok(AgenticOutcome {
                    used_fallback,
                    committed: false,
                    message: String::new(),
                });
            }
            let last = *order
                .last()
                .ok_or_else(|| CommitError::Validation("split plan has no commits".to_owned()))?;
            let changed = apply_changelog(cwd, changelog.as_ref(), args.dry_run)?;
            for path in changed {
                if !args.dry_run {
                    // The changelog bytes Dial just wrote are the content the split commits.
                    snapshot.add_written_file(cwd, &path)?;
                }
                let planned = plan
                    .commits
                    .iter()
                    .any(|group| group.changes.iter().any(|change| change.path() == path));
                if !planned {
                    plan.commits[last].changes.push(FileChange::All { path });
                }
            }
            let messages = if args.dry_run {
                preview_split(&plan, &order)
            } else {
                execute_split(cwd, &plan, &order, &snapshot, config.git.bypass_hooks)?
            };
            Ok(AgenticOutcome {
                used_fallback,
                committed: !args.dry_run,
                message: messages.join("\n\n---\n\n"),
            })
        }
    }
}

#[cfg(test)]
mod tests;
