//! Goal progress file (`goal.progress_file`): a workspace-visible Markdown mirror of the goal, its
//! todos and the working state, rewritten after each goal-mode turn so a long task's progress can
//! be read, shared or committed between sessions.
//!
//! The file is a user deliverable, not Dial's private state (that lives outside the workspace; see
//! `config::paths`). It is a harness write that respects confinement: the configured relative path
//! resolves through the workspace's roots and symlink checks, protected Dial storage is refused,
//! and the checkpoint observer records the previous content, as for any native edit. The user opts
//! in by naming the path, so no per-write approval is asked. Plan mode writes nothing.

use super::{Goal, remaining};
use crate::agent::workflow::todo::TodoList;
use crate::tools::Workspace;
use serde_json::Value;
use std::io::Write;

/// The progress file's Markdown. Model-recorded todos and working state are shown as written;
/// they are claims, not verification.
pub(super) fn render(goal: &Goal, todo: &TodoList, working_state: &Value) -> String {
    let status = serde_json::to_value(goal.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let budget = match (goal.token_budget, remaining(goal)) {
        (Some(budget), Some(left)) => format!(
            "{} of {budget} tokens used ({left} remaining)",
            goal.tokens_used
        ),
        _ => format!("{} tokens used (no budget)", goal.tokens_used),
    };
    let mut out = format!(
        "# Goal progress\n\n<!-- Written by Dial after each goal-mode turn (goal.progress_file); edits here are overwritten. Todos and working state are the agent's own records, not verification. -->\n\nStatus: {status}. {budget}; {} seconds.\n\n## Objective\n\n{}\n",
        goal.time_used_secs,
        goal.objective.trim()
    );
    let todos = todo.render();
    if !todos.is_empty() {
        out.push_str(&format!("\n## Todos\n\n```text\n{todos}\n```\n"));
    }
    let state = crate::agent::workflow::working_state::render_markdown(working_state);
    if !state.is_empty() {
        out.push_str(&format!("\n## Working state\n\n{state}\n"));
    }
    out
}

/// Write `text` to the workspace-relative `path`. Returns `false` when the file already holds it,
/// so an unchanged state adds no checkpoint entry.
pub(super) fn write(workspace: &Workspace, path: &str, text: &str) -> Result<bool, String> {
    let target = workspace.resolve(path)?;
    if std::fs::read(&target).is_ok_and(|current| current == text.as_bytes()) {
        return Ok(false);
    }
    workspace.observe_write(&target)?;
    let parent = target
        .parent()
        .ok_or_else(|| format!("{path} has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("creating {path}'s directory: {error}"))?;
    // Re-resolve after creating directories, so a component that appeared meanwhile is checked.
    if workspace.resolve(path)? != target {
        return Err(format!(
            "{path} changed location while it was being written"
        ));
    }
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    file.write_all(text.as_bytes())
        .map_err(|error| error.to_string())?;
    file.persist(&target)
        .map_err(|error| error.error.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests;
