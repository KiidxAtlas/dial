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

fn init_repo(root: &Path) {
    run(root, &["init", "-q"]);
    run(root, &["config", "user.email", "test@example.com"]);
    run(root, &["config", "user.name", "Test"]);
    run(root, &["config", "commit.gpgsign", "false"]);
    fs::write(root.join("base.txt"), "base\n").expect("write base");
    run(root, &["add", "base.txt"]);
    run(root, &["commit", "-q", "-m", "init"]);
}

fn cached_diff(root: &Path) -> String {
    run(
        root,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
        ],
    )
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
fn preview_returns_messages_without_git() {
    let plan = SplitCommitPlan {
        commits: vec![group("a.txt", "add a")],
        warnings: Vec::new(),
    };
    let messages = preview_split(&plan, &[0]);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].starts_with("feat: add a"));
}

#[test]
fn split_commits_captured_content_and_keeps_unstaged_edits_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    // Staged bytes differ from the working tree; the binary file is committed whole.
    fs::write(root.join("a.txt"), "staged\n").expect("write a");
    run(root, &["add", "a.txt"]);
    fs::write(root.join("a.txt"), "unfinished scratch\n").expect("edit a");
    fs::write(root.join("b.bin"), b"\0\x01binary\0").expect("write b");
    run(root, &["add", "b.bin"]);
    let full_diff = cached_diff(root);
    let snapshot = StagedSnapshot::capture(root, None, &full_diff).expect("capture");

    let plan = SplitCommitPlan {
        commits: vec![group("a.txt", "add a"), group("b.bin", "add b")],
        warnings: Vec::new(),
    };
    execute_split(root, &plan, &[0, 1], &snapshot, false).expect("split");

    let log = run(root, &["log", "--format=%s"]);
    let subjects: Vec<&str> = log.lines().collect();
    assert_eq!(subjects, vec!["feat: add b", "feat: add a", "init"]);
    assert_eq!(run(root, &["show", "HEAD~1:a.txt"]), "staged\n");
    assert_eq!(
        fs::read_to_string(root.join("a.txt")).unwrap(),
        "unfinished scratch\n"
    );
    assert_eq!(run(root, &["status", "--porcelain"]), " M a.txt\n");
    assert!(run(root, &["diff", "--cached", "--name-only"]).is_empty());
}

#[test]
fn split_refuses_to_run_when_staging_changed_after_capture() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    fs::write(root.join("a.txt"), "a\n").expect("write a");
    run(root, &["add", "a.txt"]);
    let snapshot = StagedSnapshot::capture(root, None, &cached_diff(root)).expect("capture");
    fs::write(root.join("b.txt"), "b\n").expect("write b");
    run(root, &["add", "b.txt"]);
    let plan = SplitCommitPlan {
        commits: vec![group("a.txt", "add a")],
        warnings: Vec::new(),
    };
    assert!(execute_split(root, &plan, &[0], &snapshot, false).is_err());
    assert_eq!(run(root, &["log", "--format=%s"]).lines().count(), 1);
}

#[cfg(unix)]
#[test]
fn failed_group_restores_remaining_staging_and_reports_progress() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    init_repo(root);
    let hook = root.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        "#!/bin/sh\ngit diff --cached | grep -q REJECT && exit 1\nexit 0\n",
    )
    .expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");
    fs::write(root.join("a.txt"), "ok\n").expect("write a");
    fs::write(root.join("b.txt"), "REJECT\n").expect("write b");
    run(root, &["add", "a.txt", "b.txt"]);
    let full_diff = cached_diff(root);
    let snapshot = StagedSnapshot::capture(root, None, &full_diff).expect("capture");
    let plan = SplitCommitPlan {
        commits: vec![group("a.txt", "add a"), group("b.txt", "add b")],
        warnings: Vec::new(),
    };

    let error = execute_split(root, &plan, &[0, 1], &snapshot, false)
        .expect_err("second commit is rejected by the hook");
    let text = error.to_string();
    assert!(text.contains("1 of 2 commits created"), "{text}");
    assert!(text.contains("add b"), "{text}");
    assert_eq!(run(root, &["log", "--format=%s"]).lines().count(), 2);
    assert_eq!(run(root, &["diff", "--cached", "--name-only"]), "b.txt\n");
    assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "REJECT\n");
}
