//! Workspace roots and git worktrees: `/add-dir`, `/remove-dir`, `/dirs`, `/wt` and `/git`.
//!
//! Added directories extend the agent's `Workspace` in place, so the file tools accept paths under
//! them from the next call on. `/wt` manages git worktrees of the primary repository: `create` makes
//! one carrying the uncommitted changes and moves the session into it; `switch` moves
//! the session into an existing one.

use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use crate::tools::git::{self, Worktree};
use anyhow::{Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const COMMANDS: &[super::CommandSpec] = &[
    super::CommandSpec {
        name: "add-dir",
        description: "Add a directory to the workspace so file tools can read and write under it",
        args_hint: "<dir>",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "remove-dir",
        description: "Remove an added directory from the workspace",
        args_hint: "<dir>",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "dirs",
        description: "List the workspace directories",
        args_hint: "",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "wt",
        description: "manage git worktrees for this session",
        args_hint: "[list|create [branch]|switch <path|branch>|remove <path|branch> [--force]]",
        subcommands: &["create", "list", "switch", "remove"],
    },
    super::CommandSpec {
        name: "git",
        description: "Show git status and recent commits, or the summary of one revision",
        args_hint: "[revision]",
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
    let args = args.trim();
    let result = match name {
        "add-dir" => add_dir(shell, agent, args),
        "remove-dir" => remove_dir(shell, agent, args),
        "dirs" => dirs(shell, agent),
        "wt" => worktree_command(shell, app, agent, args),
        "git" => git_command(shell, app, agent, args),
        _ => return None,
    };
    Some(result)
}

/// Resolve a user-typed directory: `~` and `~/…` against the home directory, absolute paths as
/// given, relative paths against the primary root.
fn resolve_dir(root: &Path, arg: &str) -> PathBuf {
    if arg == "~" || arg.starts_with("~/") {
        return crate::storage::archive::expand_home(arg);
    }
    let path = PathBuf::from(arg);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn add_dir(shell: &mut Shell, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    if args.is_empty() {
        shell.note("Add dir", "Usage: /add-dir <dir>");
        return Ok(Step::Done);
    }
    let dir = resolve_dir(agent.workspace().root(), args);
    let added = agent.workspace_mut().add_root(&dir)?;
    agent.persist_workspace_roots()?;
    let text = if added {
        format!("Added {}.", canonical(&dir).display())
    } else {
        format!("Already in the workspace: {}", canonical(&dir).display())
    };
    shell.note("Add dir", text);
    Ok(Step::Done)
}

fn remove_dir(shell: &mut Shell, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    if args.is_empty() {
        shell.note("Remove dir", "Usage: /remove-dir <dir>");
        return Ok(Step::Done);
    }
    let dir = resolve_dir(agent.workspace().root(), args);
    let removed = agent.workspace_mut().remove_root(&dir)?;
    agent.persist_workspace_roots()?;
    let text = if removed {
        format!("Removed {}.", canonical(&dir).display())
    } else {
        format!("Not a workspace directory: {}", dir.display())
    };
    shell.note("Remove dir", text);
    Ok(Step::Done)
}

fn dirs(shell: &mut Shell, agent: &Agent<'_>) -> Result<Step> {
    let ws = agent.workspace();
    let mut lines = vec![format!("{} (working directory)", ws.root().display())];
    lines.extend(
        ws.additional_roots()
            .iter()
            .map(|dir| dir.display().to_string()),
    );
    shell.note("Workspace directories", lines.join("\n"));
    Ok(Step::Done)
}

fn worktree_command(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    args: &str,
) -> Result<Step> {
    let (sub, rest) = split_word(args);
    match sub {
        // Bare `/wt` only looks; creating a worktree moves the session, so it needs `create`.
        "" => list(shell, agent),
        "create" => create(shell, app, agent, rest),
        "list" => list(shell, agent),
        "switch" => switch(shell, app, agent, rest),
        "remove" => remove(shell, agent, rest),
        other => {
            shell.note(
                "Worktree",
                format!("Unknown subcommand {other:?}. Usage: /wt [list|create [branch]|switch <path|branch>|remove <path|branch> [--force]]"),
            );
            Ok(Step::Done)
        }
    }
}

/// Create a worktree on a new branch under `~/.dial/wt` carrying the uncommitted changes, then move
/// this session into it.
fn create(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, branch: &str) -> Result<Step> {
    if agent.session().path().is_none() {
        bail!("no session file to move (in-memory session)");
    }
    let branch = if branch.is_empty() {
        default_branch()
    } else {
        branch.to_owned()
    };
    check_branch_name(&branch)?;
    let root = agent.workspace().root().to_path_buf();
    let reference = format!("refs/heads/{branch}");
    if git::run_checked(
        &root,
        &["show-ref", "--verify", "--quiet", &reference],
        false,
    )
    .is_ok()
    {
        bail!("Branch '{branch}' already exists; pick another name.");
    }
    let path = available_path(&git::worktree_path_for(&root, &branch).map_err(|e| anyhow!(e))?);
    let carried = git::worktree_add_keep_changes(&root, &path, &branch).map_err(|e| anyhow!(e))?;
    let path = canonical(&path);
    relocate(agent, &path).map_err(|e| {
        anyhow!(
            "Created {} but the session did not move: {e:#}",
            path.display()
        )
    })?;
    shell.stop_loop_for_session_change();
    let cleaned = if app.config.worktree.clean_source {
        match git::clean_source_checkout(&root) {
            Ok(()) => true,
            Err(error) => {
                let message = format!(
                    "Warning: Worktree created, but cleaning source checkout failed: {error}"
                );
                shell.note("Worktree", message);
                false
            }
        }
    } else {
        false
    };
    let how = match (carried, cleaned) {
        (true, true) => "uncommitted changes moved, source checkout cleaned",
        (true, false) => "uncommitted changes carried over",
        (false, _) => "started from clean HEAD",
    };
    shell.note(
        "Worktree",
        format!("Created {} on {branch} ({how}).", path.display()),
    );
    Ok(Step::Done)
}

/// Move this session into `dir` through `/move`, so its journal and the live tools follow it.
/// Fails unless the session actually moved, so `/wt` never reports a move that did not happen.
fn relocate(agent: &mut Agent<'_>, dir: &Path) -> Result<()> {
    match super::session::move_session(agent, &dir.to_string_lossy())? {
        super::session::MoveOutcome::Moved(_) => Ok(()),
        super::session::MoveOutcome::Refused(reason) => bail!("{reason}"),
    }
}

/// `base`, or the first `base-N` (N ≥ 2) that does not exist yet.
fn available_path(base: &Path) -> PathBuf {
    let name = base
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut path = base.to_path_buf();
    let mut n = 2;
    while path.exists() {
        path = base.with_file_name(format!("{name}-{n}"));
        n += 1;
    }
    path
}

/// Reject branch names git refuses.
fn check_branch_name(branch: &str) -> Result<()> {
    let bad_char = branch
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'));
    if branch.is_empty()
        || bad_char
        || branch.starts_with('-')
        || branch.ends_with('/')
        || branch.contains("..")
    {
        bail!("Invalid branch name: {branch}");
    }
    Ok(())
}

fn list(shell: &mut Shell, agent: &Agent<'_>) -> Result<Step> {
    let root = agent.workspace().root().to_path_buf();
    let trees = git::worktree_list(&root).map_err(|e| anyhow!(e))?;
    let attached: Vec<PathBuf> = agent.workspace().roots().map(Path::to_path_buf).collect();
    let lines: Vec<String> = trees
        .iter()
        .enumerate()
        .map(|(index, tree)| {
            let mut line = format!("{}  {}", tree.path.display(), label(tree));
            if index == 0 {
                line.push_str(" (primary)");
            }
            if tree.locked {
                line.push_str(" (locked)");
            }
            if attached.iter().any(|dir| same_path(dir, &tree.path)) {
                line.push_str(" [in workspace]");
            }
            line
        })
        .collect();
    let mut text = lines.join("\n");
    text.push_str(
        "\n/wt create [branch] makes a worktree and moves this session there · /wt switch <branch> · /wt remove <branch>",
    );
    shell.note("Worktrees", text);
    Ok(Step::Done)
}

/// Move this session into an existing worktree (matched by branch or path).
fn switch(shell: &mut Shell, _app: &App, agent: &mut Agent<'_>, target: &str) -> Result<Step> {
    if target.is_empty() {
        shell.note("Worktree", "Usage: /wt switch <path|branch>");
        return Ok(Step::Done);
    }
    let root = agent.workspace().root().to_path_buf();
    let trees = git::worktree_list(&root).map_err(|e| anyhow!(e))?;
    let Some(tree) = find_target(&trees, target, &root) else {
        bail!("no worktree matches {target:?}");
    };
    let path = tree.path.clone();
    if same_path(&path, &root) {
        shell.note(
            "Worktree",
            format!("The session already runs in {}.", path.display()),
        );
        return Ok(Step::Done);
    }
    relocate(agent, &path)?;
    shell.stop_loop_for_session_change();
    shell.note(
        "Worktree",
        format!("Moved to {}. Tools now operate there.", path.display()),
    );
    Ok(Step::Done)
}

/// Remove a linked worktree (never the primary one) and detach it from the workspace.
fn remove(shell: &mut Shell, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let force = args.split_whitespace().any(|word| word == "--force");
    let target = args
        .split_whitespace()
        .find(|word| *word != "--force")
        .unwrap_or("");
    if target.is_empty() {
        shell.note("Worktree", "Usage: /wt remove <path|branch> [--force]");
        return Ok(Step::Done);
    }
    let root = agent.workspace().root().to_path_buf();
    let trees = git::worktree_list(&root).map_err(|e| anyhow!(e))?;
    let Some(tree) = find_target(&trees, target, &root) else {
        bail!("no worktree matches {target:?}");
    };
    if trees
        .first()
        .is_some_and(|primary| same_path(&primary.path, &tree.path))
    {
        bail!("cannot remove the primary worktree {}", tree.path.display());
    }
    if same_path(&tree.path, &root) {
        bail!(
            "the session runs in {}; move it elsewhere with /move or /wt switch first",
            tree.path.display()
        );
    }
    git::worktree_remove(&root, &tree.path, force).map_err(|e| anyhow!(e))?;
    agent.workspace_mut().remove_root(&tree.path)?;
    agent.persist_workspace_roots()?;
    shell.note("Worktree", format!("Removed {}.", tree.path.display()));
    Ok(Step::Done)
}

/// `/git [revision]`: the git panel on the working tree, or read-only on a pinned revision.
fn git_command(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    revision: &str,
) -> Result<Step> {
    if revision.starts_with('-') {
        bail!("revision must not start with '-': {revision}");
    }
    shell.open_git(app, agent, revision)
}

fn find_target<'a>(trees: &'a [Worktree], target: &str, root: &Path) -> Option<&'a Worktree> {
    if let Some(tree) = trees
        .iter()
        .find(|tree| tree.branch.as_deref() == Some(target))
    {
        return Some(tree);
    }
    let path = resolve_dir(root, target);
    trees.iter().find(|tree| same_path(&tree.path, &path))
}

fn label(tree: &Worktree) -> String {
    if tree.bare {
        "bare".to_owned()
    } else if let Some(branch) = &tree.branch {
        branch.clone()
    } else {
        format!("detached at {}", tree.head.get(..7).unwrap_or(&tree.head))
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    canonical(a) == canonical(b)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn default_branch() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("wt-{secs}")
}

/// Split off the first whitespace-delimited word.
fn split_word(args: &str) -> (&str, &str) {
    match args.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (args, ""),
    }
}

#[cfg(test)]
mod tests;
