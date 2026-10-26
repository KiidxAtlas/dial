//! Split-commit execution. The staged state is captured once when the commit run starts
//! ([`StagedSnapshot`]). Every group is built from those captured identities: whole-file groups
//! take the captured blob and mode, and hunk or line groups take the captured staged diff. All
//! group trees are prepared in a private index before the repository index is touched, so a
//! failed preparation leaves git unchanged. Each commit then points the real index at its prepared
//! tree; when a commit fails, the captured staging is restored and the report says what was
//! committed.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use super::state::{
    CommitProposal, ConventionalAnalysis, FileChange, SplitCommitGroup, SplitCommitPlan,
};
use crate::commit::CommitError;
use crate::commit::commit_types::format_conventional_commit;
use crate::commit::diff::parse_file_diffs;
use crate::commit::execute::{commit_with_message, git_output, git_output_indexed};
use crate::tools::vcs::git_patch::{file_header_and_hunks, line_patch};

fn text(error: CommitError) -> String {
    match error {
        CommitError::Model(text) | CommitError::Git(text) | CommitError::Validation(text) => text,
    }
}

/// `git <args>` with an optional private index, errors as text.
fn git_in(root: &Path, index: Option<&Path>, args: &[&str]) -> Result<String, String> {
    git_output_indexed(root, index, args, "Git command failed").map_err(text)
}

/// Run `git <args>` in `root` with `stdin` piped in and `index` as its index. A non-zero exit
/// becomes the error text.
fn git_with_stdin(root: &Path, index: &Path, args: &[&str], stdin: &str) -> Result<(), String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_INDEX_FILE", index)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run git: {error}"))?;
    let written = match child.stdin.take() {
        Some(mut input) => input.write_all(stdin.as_bytes()),
        None => Ok(()),
    };
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed to wait for git: {error}"))?;
    written.map_err(|error| format!("failed to write patch to git: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// The commit `HEAD` names, or `None` on an unborn branch.
fn head_commit(root: &Path) -> Result<Option<String>, CommitError> {
    Ok(git_output(
        root,
        &["rev-parse", "--verify", "-q", "HEAD"],
        "Failed to read HEAD",
    )
    .ok()
    .map(|head| head.trim().to_owned())
    .filter(|head| !head.is_empty()))
}

/// Ensure a patch fragment ends with a newline.
fn with_newline(fragment: &str) -> String {
    if fragment.ends_with('\n') {
        fragment.to_owned()
    } else {
        format!("{fragment}\n")
    }
}

/// Assemble a single-file patch from a header and hunk texts.
fn assemble(header: &str, hunks: &[String]) -> String {
    let mut patch = with_newline(header);
    for hunk in hunks {
        patch.push_str(&with_newline(hunk));
    }
    patch
}

/// The new-side start line of a `@@ -a,b +c,d @@` header.
fn new_start(hunk_header: &str) -> Option<u64> {
    hunk_header
        .split_whitespace()
        .find(|token| token.starts_with('+'))?
        .trim_start_matches('+')
        .split(',')
        .next()?
        .parse()
        .ok()
}

/// Build a patch that stages only the added lines of `file_text` whose new-side line number is in `start..=end`.
fn lines_patch(file_text: &str, path: &str, start: u64, end: u64) -> Result<String, String> {
    let (header, hunks) =
        file_header_and_hunks(file_text).ok_or_else(|| format!("no hunks for {path}"))?;
    let mut selected_hunks: Vec<String> = Vec::new();
    let mut header_out: Option<String> = None;
    for (hunk_index, hunk_text) in hunks.iter().enumerate() {
        let mut body = hunk_text.lines();
        let hunk_header = body.next().unwrap_or("");
        let mut new_line =
            new_start(hunk_header).ok_or_else(|| format!("malformed hunk for {path}"))?;
        let mut selected: Vec<usize> = Vec::new();
        let mut body_index = 0usize;
        for line in body {
            // `line_patch` does not number `\ No newline` markers; numbering must match it.
            if line.starts_with('\\') {
                continue;
            }
            if line.starts_with('+') {
                if (start..=end).contains(&new_line) {
                    selected.push(body_index);
                }
                new_line += 1;
            } else if line.starts_with(' ') {
                new_line += 1;
            }
            body_index += 1;
        }
        if selected.is_empty() {
            continue;
        }
        let patch = line_patch(file_text, hunk_index, &selected, false)
            .ok_or_else(|| format!("could not build a patch for {path}"))?;
        let (patch_header, patch_hunks) = file_header_and_hunks(&patch)
            .ok_or_else(|| format!("could not build a patch for {path}"))?;
        header_out.get_or_insert(patch_header);
        selected_hunks.extend(patch_hunks);
    }
    if selected_hunks.is_empty() {
        return Err(format!("no added lines in {start}-{end} for {path}"));
    }
    Ok(assemble(&header_out.unwrap_or(header), &selected_hunks))
}

/// Build the patch for one hunk or line selection, taken from the captured staged diff. Binary
/// files have no hunks, so a partial selection of one is refused; they are committed whole.
fn change_patch(diff_text: &str, change: &FileChange) -> Result<String, String> {
    let path = change.path();
    let file = parse_file_diffs(diff_text)
        .into_iter()
        .find(|file| file.filename == path)
        .ok_or_else(|| format!("no staged diff for {path}"))?;
    match change {
        FileChange::All { .. } => Err(format!("{path} is staged as a whole file")),
        FileChange::Indices { indices, .. } => {
            let (header, hunks) = file_header_and_hunks(&file.content)
                .ok_or_else(|| format!("no hunks for {path}; commit it whole"))?;
            let mut selected = Vec::new();
            for index in indices {
                let hunk = hunks
                    .get((*index as usize).wrapping_sub(1))
                    .ok_or_else(|| format!("invalid hunk indices for {path}"))?;
                selected.push(hunk.clone());
            }
            Ok(assemble(&header, &selected))
        }
        FileChange::Lines { start, end, .. } => lines_patch(&file.content, path, *start, *end),
    }
}

/// One staged entry of the captured index: file mode and blob id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StagedEntry {
    mode: String,
    blob: String,
}

/// The staged state captured when a commit run starts. Groups are built only from these identities.
/// `index_tree` lets execution detect a change made after the capture.
pub(crate) struct StagedSnapshot {
    head: Option<String>,
    index_tree: String,
    full_diff: String,
    /// Captured staged entries keyed by path; `None` marks a staged deletion.
    entries: BTreeMap<String, Option<StagedEntry>>,
}

/// Parses `git diff --cached --raw -z --no-renames --no-abbrev` into captured entries. Each record
/// is `:<old mode> <new mode> <old blob> <new blob> <status>` followed by the path.
fn parse_raw(raw: &str) -> BTreeMap<String, Option<StagedEntry>> {
    let mut entries = BTreeMap::new();
    let mut fields = raw.split('\0');
    while let Some(meta) = fields.next() {
        let Some(meta) = meta.strip_prefix(':') else {
            continue;
        };
        let Some(path) = fields.next() else {
            break;
        };
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() != 5 || path.is_empty() {
            continue;
        }
        let entry = if parts[4].starts_with('D') {
            None
        } else {
            Some(StagedEntry {
                mode: parts[1].to_owned(),
                blob: parts[3].to_owned(),
            })
        };
        entries.insert(path.to_owned(), entry);
    }
    entries
}

impl StagedSnapshot {
    /// Captures HEAD, the index tree, the staged diff and the staged entries. `index` is the
    /// private preview index for a dry run, or `None` for the repository's own index.
    pub(crate) fn capture(
        root: &Path,
        index: Option<&Path>,
        full_diff: &str,
    ) -> Result<Self, CommitError> {
        let head = head_commit(root)?;
        let index_tree =
            git_output_indexed(root, index, &["write-tree"], "Failed to read the index")?
                .trim()
                .to_owned();
        let raw = git_output_indexed(
            root,
            index,
            &[
                "diff",
                "--cached",
                "--raw",
                "-z",
                "--no-renames",
                "--no-abbrev",
            ],
            "Failed to read staged entries",
        )?;
        Ok(Self {
            head,
            index_tree,
            full_diff: full_diff.to_owned(),
            entries: parse_raw(&raw),
        })
    }

    /// Records a file that Dial wrote during this run (a changelog update), so the plan commits
    /// exactly the bytes written. The mode follows any captured entry, otherwise a regular file.
    pub(crate) fn add_written_file(&mut self, root: &Path, path: &str) -> Result<(), CommitError> {
        let path_arg = format!("--path={path}");
        let blob = git_output(
            root,
            &["hash-object", "-w", path_arg.as_str(), "--", path],
            "Failed to record changelog update",
        )?
        .trim()
        .to_owned();
        let mode = match self.entries.get(path) {
            Some(Some(entry)) => entry.mode.clone(),
            _ => "100644".to_owned(),
        };
        self.entries
            .insert(path.to_owned(), Some(StagedEntry { mode, blob }));
        Ok(())
    }

    /// The captured entry for a whole-file change: `Some` to stage the blob, `None` for a deletion.
    fn entry(&self, path: &str) -> Result<Option<&StagedEntry>, String> {
        match self.entries.get(path) {
            Some(entry) => Ok(entry.as_ref()),
            None => Err(format!("{path} is not staged")),
        }
    }
}

/// Stages one group's changes into `index`, which already holds the groups before it.
fn stage_group(
    root: &Path,
    index: &Path,
    snapshot: &StagedSnapshot,
    group: &SplitCommitGroup,
) -> Result<(), String> {
    for change in &group.changes {
        match change {
            FileChange::All { path } => match snapshot.entry(path)? {
                Some(entry) => {
                    git_in(
                        root,
                        Some(index),
                        &[
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            entry.mode.as_str(),
                            entry.blob.as_str(),
                            path.as_str(),
                        ],
                    )?;
                }
                None => {
                    git_in(
                        root,
                        Some(index),
                        &["update-index", "--force-remove", "--", path.as_str()],
                    )?;
                }
            },
            other => {
                let patch = change_patch(&snapshot.full_diff, other)?;
                git_with_stdin(
                    root,
                    index,
                    &["apply", "--cached", "--recount", "-"],
                    &patch,
                )?;
            }
        }
    }
    Ok(())
}

/// Cumulative tree after each group, in `order`, built in a private index under `scratch`. Nothing
/// in the repository index or history changes. Group `k`'s tree holds HEAD plus groups `0..=k`.
fn prepare_trees(
    root: &Path,
    snapshot: &StagedSnapshot,
    plan: &SplitCommitPlan,
    order: &[usize],
    scratch: &Path,
) -> Result<Vec<String>, String> {
    let index = scratch.join("index");
    let base = match &snapshot.head {
        Some(head) => vec!["read-tree", head.as_str()],
        None => vec!["read-tree", "--empty"],
    };
    git_in(root, Some(index.as_path()), &base)?;
    let mut trees = Vec::with_capacity(order.len());
    for group_index in order {
        stage_group(root, &index, snapshot, &plan.commits[*group_index])?;
        let tree = git_in(root, Some(index.as_path()), &["write-tree"])?;
        trees.push(tree.trim().to_owned());
    }
    Ok(trees)
}

/// Verifies that HEAD and the staged content still match the captured state.
fn verify_unchanged(root: &Path, snapshot: &StagedSnapshot) -> Result<(), CommitError> {
    let head = head_commit(root)?;
    let index_tree = git_output(root, &["write-tree"], "Failed to read the index")?
        .trim()
        .to_owned();
    if head != snapshot.head || index_tree != snapshot.index_tree {
        return Err(CommitError::Git(
            "HEAD or the staged changes changed after the split plan was made; nothing was committed. Run the commit again."
                .to_owned(),
        ));
    }
    Ok(())
}

/// Commit message of one group, as the commit will record it.
fn group_message(group: &SplitCommitGroup) -> crate::commit::commit_types::CommitMessage {
    CommitProposal {
        analysis: ConventionalAnalysis {
            commit_type: group.commit_type.clone(),
            scope: group.scope.clone(),
            details: group.details.clone(),
            issue_refs: group.issue_refs.clone(),
        },
        summary: group.summary.clone(),
        warnings: Vec::new(),
    }
    .to_message()
}

/// Files of `order[from..]`, counted once per path.
fn remaining_files(plan: &SplitCommitPlan, order: &[usize], from: usize) -> usize {
    let mut paths: Vec<&str> = order[from..]
        .iter()
        .flat_map(|index| plan.commits[*index].changes.iter().map(FileChange::path))
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths.len()
}

/// Subject lines of `groups`, one per line, for the failure report.
fn subjects(plan: &SplitCommitPlan, groups: &[usize]) -> String {
    groups
        .iter()
        .map(|index| {
            let message = format_conventional_commit(&group_message(&plan.commits[*index]));
            format!("  - {}", message.lines().next().unwrap_or_default())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns the commit messages a split would create, without touching git.
pub(crate) fn preview_split(plan: &SplitCommitPlan, order: &[usize]) -> Vec<String> {
    order
        .iter()
        .map(|index| format_conventional_commit(&group_message(&plan.commits[*index])))
        .collect()
}

/// Execute a validated split plan in dependency order against the captured staged state.
///
/// Every group tree is prepared first, so a bad selection fails before the first commit. Each
/// commit then sets the real index to its prepared tree and commits it. If a group fails, the
/// captured staging is restored, so the remaining groups stay staged exactly as captured, and the
/// error reports which groups were committed and which remain.
pub(crate) fn execute_split(
    root: &Path,
    plan: &SplitCommitPlan,
    order: &[usize],
    snapshot: &StagedSnapshot,
    bypass_hooks: bool,
) -> Result<Vec<String>, CommitError> {
    let total = order.len();
    verify_unchanged(root, snapshot)?;
    let scratch = tempfile::tempdir()
        .map_err(|error| CommitError::Git(format!("Failed to prepare the split: {error}")))?;
    let trees =
        prepare_trees(root, snapshot, plan, order, scratch.path()).map_err(CommitError::Git)?;

    let mut messages = Vec::with_capacity(total);
    for (position, group_index) in order.iter().enumerate() {
        let group = &plan.commits[*group_index];
        let result = git_in(root, None, &["read-tree", trees[position].as_str()]).and_then(|_| {
            commit_with_message(
                root,
                &format_conventional_commit(&group_message(group)),
                bypass_hooks,
            )
            .map_err(text)
        });
        if let Err(error) = result {
            let restored = git_in(root, None, &["read-tree", snapshot.index_tree.as_str()]);
            let staging = match restored {
                Ok(_) => format!(
                    "The captured staging of the {} file(s) in the remaining groups was restored; unstaged changes were not modified.",
                    remaining_files(plan, order, position)
                ),
                Err(restore_error) => format!(
                    "Restoring the captured staging failed ({restore_error}). Recover it with `git read-tree {}`.",
                    snapshot.index_tree
                ),
            };
            let completed = &order[..position];
            let pending = &order[position..];
            let committed = if completed.is_empty() {
                "No commits were created.".to_owned()
            } else {
                format!("Created commits:\n{}", subjects(plan, completed))
            };
            return Err(CommitError::Git(format!(
                "Commit {} of {total} failed: {error}\n{position} of {total} commits created. {committed}\nNot committed:\n{}\n{staging}",
                position + 1,
                subjects(plan, pending),
            )));
        }
        messages.push(format_conventional_commit(&group_message(group)));
    }
    Ok(messages)
}

#[cfg(test)]
mod tests;
