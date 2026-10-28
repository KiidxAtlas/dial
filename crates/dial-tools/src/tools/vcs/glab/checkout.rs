//! `mr_checkout`: check a merge request out into a new detached worktree, so the primary
//! checkout is never moved. A failed glab checkout removes the worktree it created.

use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use super::runner::{Project, glab_in};
use super::{mr_iid, opt_project, push_repo};
use crate::tools::Workspace;
use crate::tools::vcs::gh_pr::{hash_path, worktrees_root};
use crate::tools::vcs::git;

pub(super) fn run(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> Result<String, String> {
    let project = opt_project(args)?;
    let iid = mr_iid(args)?;
    let base = worktrees_root()?;
    checkout_in(ws.root(), &base, iid, project.as_ref(), cancel)
}

/// Check `iid` out under `base`, keyed by the repository root. Refuses an existing worktree path
/// instead of reusing it, since glab would move the branch under the user's feet.
pub(super) fn checkout_in(
    root: &Path,
    base: &Path,
    iid: u64,
    project: Option<&Project>,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let path = base.join(hash_path(root)).join(format!("mr-{iid}"));
    if path.exists() {
        return Err(format!(
            "{} already exists; remove that worktree to check out merge request !{iid} again",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    let path_text = path.to_string_lossy().into_owned();
    git::run_checked(
        root,
        &["worktree", "add", "--detach", &path_text, "HEAD"],
        true,
    )?;

    let mut argv = vec!["mr".to_owned(), "checkout".to_owned(), iid.to_string()];
    push_repo(&mut argv, project);
    match glab_in(&path, &argv, cancel) {
        Ok(stdout) => {
            let detail = stdout.trim();
            Ok(if detail.is_empty() {
                format!("Checked out merge request !{iid} in {}.", path.display())
            } else {
                format!(
                    "Checked out merge request !{iid} in {}.\n{detail}",
                    path.display()
                )
            })
        }
        Err(error) => {
            // Best effort: the checkout already failed; the worktree is ours to clean up.
            let _ = git::worktree_remove(root, &path, true);
            Err(error)
        }
    }
}
