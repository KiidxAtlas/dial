use super::*;
use crate::commit::run::Completion;
use crate::llm::Chat;
use std::process::Command;

/// A backend that must not be reached in these tests.
struct NoModel;

impl CommitBackend for NoModel {
    fn complete(&self, _config: &Config, _request: &Chat) -> Result<Completion, String> {
        Err("model not expected in this test".to_owned())
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn repo_with_staged_file() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("base.txt"), "base\n").expect("write base");
    git(root, &["add", "base.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);
    std::fs::write(root.join("a.txt"), "a\n").expect("write a");
    git(root, &["add", "a.txt"]);
    dir
}

fn args(dry_run: bool) -> CommitCommandArgs {
    CommitCommandArgs {
        push: false,
        dry_run,
        no_changelog: true,
        legacy: false,
        context: None,
        model: None,
    }
}

#[test]
fn forced_fallback_commits_staged_changes() {
    let dir = repo_with_staged_file();
    let outcome = run_with(
        &args(false),
        dir.path(),
        None,
        &Config::default(),
        &NoModel,
        &mut |_| true,
        Mode {
            force_fallback: true,
            no_fallback: false,
        },
    )
    .expect("fallback commit");
    assert!(outcome.used_fallback);
    assert!(outcome.committed);
    assert!(!outcome.message.is_empty());
    let log = git(dir.path(), &["log", "--format=%s"]);
    assert_eq!(log.lines().count(), 2);
}

#[test]
fn dry_run_creates_no_commit() {
    let dir = repo_with_staged_file();
    let outcome = run_with(
        &args(true),
        dir.path(),
        None,
        &Config::default(),
        &NoModel,
        &mut |_| true,
        Mode {
            force_fallback: true,
            no_fallback: false,
        },
    )
    .expect("dry run");
    assert!(!outcome.committed);
    let log = git(dir.path(), &["log", "--format=%s"]);
    assert_eq!(log.lines().count(), 1);
}

#[test]
fn no_fallback_turns_agent_failure_into_error() {
    let dir = repo_with_staged_file();
    let result = run_with(
        &args(true),
        dir.path(),
        None,
        &Config::default(),
        &NoModel,
        &mut |_| true,
        Mode {
            force_fallback: false,
            no_fallback: true,
        },
    );
    assert!(matches!(result, Err(CommitError::Model(_))));
}
