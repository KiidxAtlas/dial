//! `github` tool: one `op`-selected wrapper over the `gh` CLI.
//!
//! Operations are routed to op modules: `gh_view` (repo_view, file_read), `gh_search`
//! (search_*), `gh_pr` (pr_create, pr_checkout, pr_push), `gh_pr_diff` (pr_diff, behind the
//! `gh_cache` TTL cache), and `gh_run_watch` (run_watch, with progress).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use crate::config::types::Config;
use crate::tools::integrations::ext::{Concurrency, Tool, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, Workspace, tool_schema};

/// Operations that only read state; they run without approval and may run concurrently.
pub(super) const READ_OPS: &[&str] = &[
    "repo_view",
    "file_read",
    "search_issues",
    "search_prs",
    "search_code",
    "search_commits",
    "search_repos",
    "run_watch",
    "pr_diff",
];

/// Every operation the `op` field accepts.
const OPS: &[&str] = &[
    "repo_view",
    "file_read",
    "pr_create",
    "pr_checkout",
    "pr_push",
    "search_issues",
    "search_prs",
    "search_code",
    "search_commits",
    "search_repos",
    "run_watch",
    "pr_diff",
];

const DESCRIPTION: &str = "`gh` op wrapper: repos/files, PRs, search, checkout, push, Actions watch. Read PR diffs with `read pr://<N>`. Use `search_issues` for issues. PR diffs: `pr://<N>/diff` (files); `pr://<N>/diff/<i>` (file slice, 1-indexed); `pr://<N>/diff/all` (full).

Select via `op`.
- `repo`: `owner/repo`; omit to use the current checkout.
- `repo_view`: omit `repo` → current checkout.
- `file_read`: read `path` from `repo`; omit `repo` → current checkout, `branch` → default branch.
- `pr_create`: `head` defaults current branch.
- `pr_checkout`: PR(s) → dedicated git worktrees, never working tree; array `pr` batches multiple in one call.
- `pr_push`: requires prior `op: pr_checkout`.
- `search_issues`/`search_prs`/`search_commits`/`search_repos`: `query` optional with `since`/`until`; omit for date-only filter. `search_code`: `query` required; rejects `since`/`until`.
- `search_*`: `repo` defaults current checkout's `owner/repo`; search elsewhere with `repo:`/`org:`/`user:` in `query`. `search_repos`: ignores `repo`; scope via `org:`/`language:` in `query`.
- Boolean `AND`/`OR`/`NOT` combine text terms, not qualifiers; never place them between qualifiers.
- `since`/`until`: relative `<n>` + `m`/`h`/`d`/`w`/`mo`/`y` (e.g. `3d`, `2w`), ISO date `YYYY-MM-DD`, or ISO datetime. `dateField: \"updated\"`: update time (issues/PRs), push time (repos), never creation.
- `run_watch`: omit `run` → every run for current HEAD; `branch` defaults current. Fast-fails first job failure.
- `pr_diff`: `pr` number or URL; `file` = 1-indexed file slice; `all: true` = full diff; `repo` optional.

Concise summary per op. `run_watch` failures save full logs to a session artifact.

GitHub-hosted repository file: MUST use `file_read`; NEVER `curl`/`wget`.";

/// The `github` tool.
pub struct GitHub;

/// Registry entry point: the `github` tool, gated by name through `config.integrations`.
pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(GitHub)]
}

fn op_of(args: &Value) -> Option<&str> {
    args["op"].as_str()
}

fn is_read_op(args: &Value) -> bool {
    op_of(args).is_some_and(|op| READ_OPS.contains(&op))
}

/// Run one `github` call and return its output, reporting `run_watch` snapshots through `progress`.
/// Errors from op modules become error results.
fn execute(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> ToolOutput {
    let Some(op) = op_of(args) else {
        return ToolOutput::err("github: `op` is required");
    };
    let result = match op {
        "repo_view" | "file_read" => super::gh_view::run(op, ws, args, cancel),
        "search_issues" | "search_prs" | "search_code" | "search_commits" | "search_repos" => {
            super::gh_search::run(op, ws, args, cancel)
        }
        "pr_create" | "pr_checkout" | "pr_push" => {
            let result = super::gh_pr::run(op, ws, args, cancel);
            // Invalidate whatever the outcome: a failed push or checkout may still have moved refs.
            super::gh_cache::invalidate_for_op(op, args);
            result
        }
        "pr_diff" => pr_diff(ws, args, cancel),
        "run_watch" => super::gh_run_watch::run_with_progress(op, ws, args, cancel, progress),
        other => Err(format!(
            "github: unknown op {other:?}; expected one of {}",
            OPS.join(", ")
        )),
    };
    result.unwrap_or_else(ToolOutput::err)
}

/// `pr_diff`, served from the PR-diff cache when a key can be derived from `args`.
fn pr_diff(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> Result<ToolOutput, String> {
    match super::gh_cache::pr_diff_key(ws.root(), args) {
        Some(key) => {
            super::gh_cache::cached(&key, || super::gh_pr_diff::run("pr_diff", ws, args, cancel))
        }
        None => super::gh_pr_diff::run("pr_diff", ws, args, cancel),
    }
}

impl Tool for GitHub {
    fn name(&self) -> &str {
        "github"
    }

    fn schema(&self) -> Value {
        tool_schema(
            "github",
            DESCRIPTION,
            json!({
                "op": {"type": "string", "enum": OPS, "description": "github operation"},
                "repo": {"type": "string", "description": "owner/repo"},
                "branch": {"type": "string", "description": "branch"},
                "path": {"type": "string", "description": "repository-relative file path"},
                "pr": {
                    "anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}],
                    "description": "pr number, url, or branch"
                },
                "force": {"type": "boolean", "description": "reset existing local branch"},
                "forceWithLease": {"type": "boolean", "description": "force-with-lease push"},
                "title": {"type": "string", "description": "pr title"},
                "body": {"type": "string", "description": "pr body markdown"},
                "base": {"type": "string", "description": "pr base branch"},
                "head": {"type": "string", "description": "pr head branch"},
                "draft": {"type": "boolean", "description": "open pr as draft"},
                "fill": {"type": "boolean", "description": "auto-fill pr title/body from commits"},
                "reviewer": {"type": "array", "items": {"type": "string"}, "description": "reviewers"},
                "assignee": {"type": "array", "items": {"type": "string"}, "description": "assignees"},
                "label": {"type": "array", "items": {"type": "string"}, "description": "labels"},
                "query": {"type": "string", "description": "search query"},
                "since": {"type": "string", "description": "lower-bound date filter"},
                "until": {"type": "string", "description": "upper-bound date filter"},
                "dateField": {
                    "type": "string",
                    "enum": ["created", "updated"],
                    "description": "date field"
                },
                "limit": {"type": "number", "description": "max results"},
                "run": {"type": "string", "description": "actions run id or url"},
                "tail": {"type": "number", "description": "log lines per failed job"},
                "file": {"type": "number", "description": "pr_diff: 1-indexed file number for one file slice"},
                "all": {"type": "boolean", "description": "pr_diff: full diff"}
            }),
            &["op"],
        )
    }

    /// Only operations outside `READ_OPS` need approval; a missing or unknown op does too.
    fn needs_approval(&self, args: &Value) -> bool {
        !is_read_op(args)
    }

    /// Only `pr_checkout` changes the local checkout (it creates worktrees).
    fn may_modify_workspace(&self, args: &Value) -> bool {
        op_of(args) == Some("pr_checkout")
    }

    fn concurrency(&self, args: &Value) -> Concurrency {
        if is_read_op(args) {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        }
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_streaming(ctx, args, &|_| {}, &AtomicBool::new(false))
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        // Forward run_watch snapshots as `progress` events while the watch runs.
        let mut progress = |text: &str| {
            on_event(ToolEvent {
                kind: "progress".to_owned(),
                name: "github".to_owned(),
                text: text.to_owned(),
                artifact: None,
            })
        };
        execute(ctx.workspace, args, cancel, &mut progress)
    }
}

#[cfg(test)]
mod tests;
