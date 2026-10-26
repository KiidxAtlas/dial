use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;

use super::*;
use crate::commit::backend::Completion;
use crate::config::types::Config;
use crate::llm::{Chat, Reply};

const REPLY: &str = "# feat(demo): added greeting\n\n- Added the greeting helper.";

/// Canned model: every completion returns `REPLY` and is counted.
struct FakeBackend {
    calls: AtomicUsize,
}

impl FakeBackend {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CommitBackend for FakeBackend {
    fn complete(&self, _config: &Config, _request: &Chat) -> Result<Completion, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Completion {
            reply: Reply {
                text: REPLY.to_owned(),
                ..Reply::default()
            },
            cost_usd: None,
        })
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Dial Test",
            "-c",
            "user.email=test@example.com",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository with one commit and no remote.
fn repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q"]);
    // Dial's own commit runs plain `git commit`; CI runners have no global identity.
    git(dir.path(), &["config", "user.name", "Dial Test"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("README.md"), "demo\n").expect("write readme");
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

/// Config whose user directory (commit cache) lives outside the repository.
fn test_config(user_dir: &Path) -> Config {
    Config {
        user_dir: user_dir.to_path_buf(),
        ..Default::default()
    }
}

fn legacy_args() -> CommitCommandArgs {
    CommitCommandArgs {
        legacy: true,
        no_changelog: true,
        ..CommitCommandArgs::default()
    }
}

fn write_change(dir: &Path) {
    std::fs::write(dir.join("greeting.txt"), "hello\n").expect("write change");
}

fn run_legacy(
    args: &CommitCommandArgs,
    repo: &Path,
    user: &TempDir,
    backend: &FakeBackend,
) -> Result<CommitRunOutcome, CommitError> {
    run_commit_command(
        args,
        repo,
        &test_config(user.path()),
        backend,
        &mut |_: &str| true,
    )
}

#[test]
fn legacy_dry_run_returns_message_and_creates_no_commit() {
    let repo = repo();
    let user = TempDir::new().expect("tempdir");
    write_change(repo.path());
    let backend = FakeBackend::new();
    let args = CommitCommandArgs {
        dry_run: true,
        ..legacy_args()
    };
    let outcome = run_legacy(&args, repo.path(), &user, &backend).expect("dry run succeeds");
    let CommitRunOutcome::DryRun { message, .. } = outcome else {
        panic!("expected DryRun, got {outcome:?}");
    };
    assert!(message.contains("feat(demo): added greeting"), "{message}");
    assert_eq!(
        git(repo.path(), &["rev-list", "--count", "HEAD"]).trim(),
        "1"
    );
}

#[test]
fn legacy_commit_creates_commit_with_generated_message() {
    let repo = repo();
    let user = TempDir::new().expect("tempdir");
    write_change(repo.path());
    let backend = FakeBackend::new();
    let outcome =
        run_legacy(&legacy_args(), repo.path(), &user, &backend).expect("commit succeeds");
    let CommitRunOutcome::Committed {
        message,
        pushed,
        used_fallback,
    } = outcome
    else {
        panic!("expected Committed, got {outcome:?}");
    };
    assert!(!pushed);
    assert!(!used_fallback);
    assert!(message.contains("feat(demo): added greeting"), "{message}");
    assert_eq!(
        git(repo.path(), &["log", "-1", "--format=%s"]).trim(),
        "feat(demo): added greeting"
    );
}

#[test]
fn push_without_remote_is_a_git_error() {
    let repo = repo();
    let user = TempDir::new().expect("tempdir");
    write_change(repo.path());
    let backend = FakeBackend::new();
    let args = CommitCommandArgs {
        push: true,
        ..legacy_args()
    };
    let error = run_legacy(&args, repo.path(), &user, &backend)
        .expect_err("push must fail without a remote");
    assert!(matches!(error, CommitError::Git(_)), "{error:?}");
}

#[test]
fn clean_repo_is_nothing_to_commit() {
    let repo = repo();
    let user = TempDir::new().expect("tempdir");
    let backend = FakeBackend::new();
    let outcome =
        run_legacy(&legacy_args(), repo.path(), &user, &backend).expect("clean run succeeds");
    assert_eq!(outcome, CommitRunOutcome::NothingToCommit { pushed: false });
    assert_eq!(backend.calls(), 0);
}

#[test]
fn fallback_commit_exits_with_code_one() {
    let fallback = CommitRunOutcome::Committed {
        message: String::new(),
        pushed: false,
        used_fallback: true,
    };
    assert_eq!(fallback.exit_code(), 1);
    let clean = CommitRunOutcome::Committed {
        message: String::new(),
        pushed: false,
        used_fallback: false,
    };
    assert_eq!(clean.exit_code(), 0);
    assert_eq!(
        CommitRunOutcome::NotCommitted {
            message: String::new()
        }
        .exit_code(),
        0
    );
}

/// A bare remote that receives `repo`'s current branch.
fn with_remote(repo: &Path) -> TempDir {
    let remote = TempDir::new().expect("tempdir");
    git(remote.path(), &["init", "-q", "--bare"]);
    git(
        repo,
        &[
            "remote",
            "add",
            "origin",
            remote.path().to_str().expect("utf-8 path"),
        ],
    );
    git(repo, &["push", "-q", "-u", "origin", "HEAD"]);
    remote
}

#[test]
fn dry_run_leaves_the_repository_index_untouched() {
    let repo = repo();
    let user = TempDir::new().expect("tempdir");
    write_change(repo.path());
    let index_before = std::fs::read(repo.path().join(".git/index")).expect("read index");
    let backend = FakeBackend::new();
    let args = CommitCommandArgs {
        dry_run: true,
        ..legacy_args()
    };
    run_legacy(&args, repo.path(), &user, &backend).expect("dry run succeeds");
    assert_eq!(
        std::fs::read(repo.path().join(".git/index")).expect("read index"),
        index_before
    );
    assert!(git(repo.path(), &["diff", "--cached", "--name-only"]).is_empty());
}

#[test]
fn dry_run_push_never_publishes_existing_commits() {
    let repo = repo();
    let user = TempDir::new().expect("tempdir");
    let remote = with_remote(repo.path());
    git(
        repo.path(),
        &["commit", "-q", "--allow-empty", "-m", "local only"],
    );
    let before = git(remote.path(), &["rev-parse", "HEAD"]);
    let backend = FakeBackend::new();

    let args = CommitCommandArgs {
        dry_run: true,
        push: true,
        ..legacy_args()
    };
    let outcome = run_legacy(&args, repo.path(), &user, &backend).expect("dry run succeeds");
    assert_eq!(outcome, CommitRunOutcome::WouldPush);
    assert_eq!(git(remote.path(), &["rev-parse", "HEAD"]), before);
    assert_eq!(backend.calls(), 0);

    // A real push of the same clean repository still publishes the outgoing commit.
    let args = CommitCommandArgs {
        push: true,
        ..legacy_args()
    };
    let outcome = run_legacy(&args, repo.path(), &user, &backend).expect("push succeeds");
    assert_eq!(outcome, CommitRunOutcome::NothingToCommit { pushed: true });
    assert_ne!(git(remote.path(), &["rev-parse", "HEAD"]), before);
}
