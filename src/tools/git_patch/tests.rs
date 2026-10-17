use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const FILE_HEADER: &str =
    "diff --git a/f.txt b/f.txt\nindex 1111111..2222222 100644\n--- a/f.txt\n+++ b/f.txt\n";

fn patch(body: &str) -> String {
    format!("{FILE_HEADER}{body}")
}

/// Body indices of the changed lines in hunk 0, numbered the same way `line_patch` numbers them.
fn change_indices(file_patch: &str) -> Vec<usize> {
    let (_, hunks) = parse(file_patch).expect("patch parses");
    hunks[0]
        .lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.kind != Kind::Context)
        .map(|(index, _)| index)
        .collect()
}

/// Scratch directory removed on drop, so a failing test still cleans up.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "dial-git-patch-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args([
            "-c",
            "user.name=dial",
            "-c",
            "user.email=dial@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git must be installed to run this test");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("git output is UTF-8")
}

fn assert_applies(repo: &Path, scratch: &Path, patch_text: &str) {
    let patch_file = scratch.join("p.patch");
    fs::write(&patch_file, patch_text).expect("write patch file");
    let out = Command::new("git")
        .current_dir(repo)
        .args(["apply", "--cached", "--check"])
        .arg(&patch_file)
        .output()
        .expect("run git apply");
    assert!(
        out.status.success(),
        "git apply --check rejected:\n{patch_text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn single_added_line_selected() {
    let file = patch("@@ -1,2 +1,4 @@\n a\n+b\n+c\n d\n");
    assert_eq!(
        line_patch(&file, 0, &[1], false).unwrap(),
        patch("@@ -1,2 +1,3 @@\n a\n+b\n d\n")
    );
}

#[test]
fn single_removed_line_selected() {
    let file = patch("@@ -1,3 +1,2 @@\n a\n-b\n c\n");
    assert_eq!(
        line_patch(&file, 0, &[1], false).unwrap(),
        patch("@@ -1,3 +1,2 @@\n a\n-b\n c\n")
    );
}

#[test]
fn unselected_removal_becomes_context() {
    let file = patch("@@ -1,4 +1,2 @@\n a\n-b\n-c\n d\n");
    assert_eq!(
        line_patch(&file, 0, &[2], false).unwrap(),
        patch("@@ -1,4 +1,3 @@\n a\n b\n-c\n d\n")
    );
}

#[test]
fn reverse_mode_swaps_roles() {
    let file = patch("@@ -1,3 +1,3 @@\n a\n-old\n+new\n c\n");
    // Reverse-selecting the removal re-adds `old`; the unselected addition `new` stays as context.
    assert_eq!(
        line_patch(&file, 0, &[1], true).unwrap(),
        patch("@@ -1,3 +1,4 @@\n a\n+old\n new\n c\n")
    );
    // Reverse-selecting the addition removes `new`; the unselected removal `old` is dropped.
    assert_eq!(
        line_patch(&file, 0, &[2], true).unwrap(),
        patch("@@ -1,3 +1,2 @@\n a\n-new\n c\n")
    );
}

#[test]
fn header_recount_with_multiple_hunks() {
    let file = patch("@@ -10,2 +10,3 @@\n a\n+x\n b\n@@ -40,4 +41,2 @@\n c\n-d\n e\n-f\n");
    assert_eq!(
        line_patch(&file, 1, &[1], false).unwrap(),
        patch("@@ -40,4 +41,3 @@\n c\n-d\n e\n f\n")
    );
}

#[test]
fn empty_selection_returns_none() {
    let file = patch("@@ -1,3 +1,2 @@\n a\n-b\n c\n");
    assert_eq!(line_patch(&file, 0, &[], false), None);
    assert_eq!(line_patch(&file, 0, &[9], false), None);
    // Context-only selection touches no change.
    assert_eq!(line_patch(&file, 0, &[0, 2], false), None);
    assert!(line_patch(&file, 0, &[1], true).is_some());
    assert_eq!(line_patch(&file, 3, &[1], false), None);
}

#[test]
fn no_newline_marker_preserved() {
    let body = "@@ -1,2 +1,2 @@\n a\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
    let file = patch(body);
    assert_eq!(line_patch(&file, 0, &[1, 2], false).unwrap(), file);
    // Only the removal is selected: its marker stays with it, the dropped addition's marker goes.
    assert_eq!(
        line_patch(&file, 0, &[1], false).unwrap(),
        patch("@@ -1,2 +1,1 @@\n a\n-old\n\\ No newline at end of file\n")
    );
}

#[test]
fn header_and_hunks_split_verbatim() {
    let file = patch("@@ -1,2 +1,3 @@ fn a()\n a\n+b\n c\n@@ -40,4 +41,2 @@\n c\n-d\n e\n-f\n");
    let (header, hunks) = file_header_and_hunks(&file).expect("hunks present");
    assert_eq!(header, FILE_HEADER);
    assert_eq!(
        hunks,
        vec![
            "@@ -1,2 +1,3 @@ fn a()\n a\n+b\n c\n".to_owned(),
            "@@ -40,4 +41,2 @@\n c\n-d\n e\n-f\n".to_owned(),
        ]
    );
}

#[test]
fn selected_lines_pass_git_apply_check() {
    let scratch = Scratch::new("apply-check");
    let repo = scratch.0.join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("f.txt"), "1\n2\n3\n4\n5\n").expect("write base");
    git(&repo, &["add", "f.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    fs::write(repo.join("f.txt"), "1\nTWO\n3\n5\nsix\n").expect("write edit");

    // Stage: the diff is against the index, which still equals HEAD.
    let unstaged = git(&repo, &["diff", "--no-color", "--", "f.txt"]);
    let changes = change_indices(&unstaged);
    assert_eq!(changes.len(), 4);
    let mut selections = vec![changes.clone()];
    selections.extend(changes.iter().map(|&index| vec![index]));
    for selected in &selections {
        let patch_text = line_patch(&unstaged, 0, selected, false).expect("selection has changes");
        assert_applies(&repo, &scratch.0, &patch_text);
    }

    // Unstage: stage everything, then revert selected lines against the index.
    git(&repo, &["add", "f.txt"]);
    let staged = git(&repo, &["diff", "--cached", "--no-color", "--", "f.txt"]);
    let staged_changes = change_indices(&staged);
    assert_eq!(staged_changes.len(), 4);
    let mut reverse_selections = vec![staged_changes.clone()];
    reverse_selections.extend(staged_changes.iter().map(|&index| vec![index]));
    for selected in &reverse_selections {
        let patch_text = line_patch(&staged, 0, selected, true).expect("selection has changes");
        assert_applies(&repo, &scratch.0, &patch_text);
    }
}
