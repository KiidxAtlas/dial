use super::*;
use std::{fs, path::Path, process::Command};

fn git_in(root: &Path) -> impl Fn(&[&str]) -> Option<String> + '_ {
    move |args| {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).ok())?
    }
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    {
        let git = git_in(dir.path());
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Test"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(args).unwrap();
        }
    }
    fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
    commit_all(dir.path());
    dir
}

fn commit_all(root: &Path) {
    let git = git_in(root);
    git(&["add", "-A"]).unwrap();
    git(&["commit", "-q", "-m", "work"]).unwrap();
}

#[test]
fn committing_the_turns_exact_result_counts() {
    let dir = repo();
    let git = git_in(dir.path());
    let base = head(&git).unwrap();
    fs::write(dir.path().join("a.txt"), "two\n").unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/new.rs"), "fn main() {}\n").unwrap();
    fs::remove_file(dir.path().join("gone.txt")).unwrap();
    let changed = ["a.txt", "src/new.rs", "gone.txt"].map(String::from);
    let change = capture(&git, dir.path(), 7, base, &changed).unwrap();
    assert_eq!(committed(&git, &change), Some(false), "not committed yet");
    commit_all(dir.path());
    assert_eq!(committed(&git, &change), Some(true));
    assert_eq!(change.trajectory, 7);
}

#[test]
fn edits_before_the_commit_do_not_count() {
    let dir = repo();
    let git = git_in(dir.path());
    let base = head(&git).unwrap();
    fs::write(dir.path().join("a.txt"), "two\n").unwrap();
    let change = capture(&git, dir.path(), 1, base, &["a.txt".into()]).unwrap();
    fs::write(dir.path().join("a.txt"), "two, fixed by hand\n").unwrap();
    commit_all(dir.path());
    assert_eq!(committed(&git, &change), Some(false));
}

#[test]
fn a_turn_that_leaves_files_as_committed_is_not_tracked() {
    let dir = repo();
    let git = git_in(dir.path());
    let base = head(&git).unwrap();
    fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let changed = ["a.txt".to_owned(), "/elsewhere/b.txt".to_owned()];
    assert!(capture(&git, dir.path(), 1, base.clone(), &changed).is_none());
    assert!(capture(&git, dir.path(), 1, base, &[]).is_none());
}

#[test]
fn outside_a_repository_nothing_is_tracked() {
    let dir = tempfile::tempdir().unwrap();
    // Temp directories can live inside another checkout, and git's ceiling does not stop the
    // lookup at the directory itself. An explicit GIT_DIR naming no repository makes this
    // probe fail without discovery, leaving the parent checkout out of the picture.
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .env("GIT_DIR", dir.path().join(".git"))
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).ok())?
    };
    assert!(head(&git).is_none());
}
