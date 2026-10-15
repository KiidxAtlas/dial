//! Split-commit execution: unstage, then stage and commit each group in order.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use super::state::{
    CommitProposal, ConventionalAnalysis, FileChange, SplitCommitGroup, SplitCommitPlan,
};
use crate::commit::commit_types::format_conventional_commit;
use crate::commit::diff::parse_file_diffs;
use crate::commit::execute::{commit_with_message, git_output};
use crate::commit::run::CommitError;
use crate::tools::git_patch::{file_header_and_hunks, line_patch};

fn text(error: CommitError) -> String {
    match error {
        CommitError::Model(text) | CommitError::Git(text) | CommitError::Validation(text) => text,
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    git_output(root, args, "Git command failed").map_err(text)
}

/// Run `git <args>` in `root` with `stdin` piped in. A non-zero exit becomes the error text.
fn git_with_stdin(root: &Path, args: &[&str], stdin: &str) -> Result<(), String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
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

/// Build the patch for one change of a group. `All` is handled by the caller with `git add`.
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
                .ok_or_else(|| format!("no hunks for {path}"))?;
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

/// Stage and commit one group. Changes are staged in plan order; the commit message comes from the group.
fn commit_group(
    root: &Path,
    group: &SplitCommitGroup,
    diff_text: &str,
    bypass_hooks: bool,
) -> Result<(), String> {
    for change in &group.changes {
        match change {
            FileChange::All { path } => {
                git(root, &["add", "-A", "--", path])?;
            }
            other => {
                let patch = change_patch(diff_text, other)?;
                git_with_stdin(root, &["apply", "--cached", "--recount", "-"], &patch)?;
            }
        }
    }
    let message = group_message(group);
    commit_with_message(root, &format_conventional_commit(&message), bypass_hooks).map_err(text)
}

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

/// Files still staged from `order[from..]`, counted once per path.
fn remaining_files(plan: &SplitCommitPlan, order: &[usize], from: usize) -> usize {
    let mut paths: Vec<&str> = order[from..]
        .iter()
        .flat_map(|index| plan.commits[*index].changes.iter().map(FileChange::path))
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths.len()
}

/// Execute a validated split plan in dependency order.
///
/// `full_diff` is the staged diff captured before any unstaging; hunk and line selections are read from it.
/// Dry runs return the commit messages without touching git. A failed group is reported with how many
/// commits were already created; no staged content is discarded.
pub(crate) fn execute_split(
    root: &Path,
    plan: &SplitCommitPlan,
    order: &[usize],
    full_diff: &str,
    dry_run: bool,
    bypass_hooks: bool,
) -> Result<Vec<String>, CommitError> {
    let total = order.len();
    if dry_run {
        return Ok(order
            .iter()
            .map(|index| format_conventional_commit(&group_message(&plan.commits[*index])))
            .collect());
    }

    let has_head = git_output(
        root,
        &["rev-parse", "--verify", "-q", "HEAD"],
        "Git command failed",
    )
    .is_ok();
    let unstage = if has_head {
        git(root, &["reset", "-q"])
    } else {
        git(
            root,
            &["rm", "-r", "--cached", "-q", "--ignore-unmatch", "."],
        )
    };
    unstage.map_err(CommitError::Git)?;

    let mut messages = Vec::with_capacity(total);
    for (position, index) in order.iter().enumerate() {
        let group = &plan.commits[*index];
        if let Err(error) = commit_group(root, group, full_diff, bypass_hooks) {
            let done = position;
            let remaining = remaining_files(plan, order, position);
            return Err(CommitError::Git(format!(
                "Commit {} of {total} failed: {error}\n{done} of {total} commits created; {remaining} file(s) remain staged. No changes were lost.",
                position + 1
            )));
        }
        messages.push(format_conventional_commit(&group_message(group)));
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::agentic::state::{
        ConventionalDetail, FileChange, SplitCommitGroup, SplitCommitPlan,
    };
    use std::fs;

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn group(path: &str, summary: &str) -> SplitCommitGroup {
        SplitCommitGroup {
            changes: vec![FileChange::All {
                path: path.to_owned(),
            }],
            commit_type: "feat".to_owned(),
            scope: None,
            summary: summary.to_owned(),
            details: Vec::<ConventionalDetail>::new(),
            issue_refs: Vec::new(),
            rationale: None,
            dependencies: Vec::new(),
        }
    }

    #[test]
    fn lines_patch_keeps_only_selected_added_lines() {
        let diff = "diff --git a/f.txt b/f.txt\nindex 1..2 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,4 @@\n a\n+b\n+c\n d\n";
        let patch = lines_patch(diff, "f.txt", 2, 2).expect("patch");
        assert!(patch.contains("+b\n"));
        assert!(!patch.contains("+c"));
    }

    #[test]
    fn dry_run_returns_messages_without_git() {
        let plan = SplitCommitPlan {
            commits: vec![group("a.txt", "add a")],
            warnings: Vec::new(),
        };
        let messages = execute_split(
            Path::new("/nonexistent-dial-root"),
            &plan,
            &[0],
            "",
            true,
            false,
        )
        .expect("dry run");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with("feat: add a"));
    }

    #[test]
    fn split_creates_one_commit_per_group_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.email", "test@example.com"]);
        run(root, &["config", "user.name", "Test"]);
        run(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("base.txt"), "base\n").expect("write base");
        run(root, &["add", "base.txt"]);
        run(root, &["commit", "-q", "-m", "init"]);
        fs::write(root.join("a.txt"), "a\n").expect("write a");
        fs::write(root.join("b.txt"), "b\n").expect("write b");
        run(root, &["add", "a.txt", "b.txt"]);
        let full_diff = run(
            root,
            &[
                "diff",
                "--cached",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
            ],
        );

        let plan = SplitCommitPlan {
            commits: vec![group("a.txt", "add a"), group("b.txt", "add b")],
            warnings: Vec::new(),
        };
        execute_split(root, &plan, &[0, 1], &full_diff, false, false).expect("split");

        let log = run(root, &["log", "--format=%s"]);
        let subjects: Vec<&str> = log.lines().collect();
        assert_eq!(subjects, vec!["feat: add b", "feat: add a", "init"]);
    }
}
