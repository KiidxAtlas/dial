use super::*;

fn run(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// A repository with one commit: tracked `kept.txt`, `.env` and `sub/tracked.txt`.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run(root, &["init", "--quiet"]);
    run(root, &["config", "user.email", "dial@example.invalid"]);
    run(root, &["config", "user.name", "dial"]);
    fs::write(root.join("kept.txt"), "original").unwrap();
    fs::write(root.join("removed.txt"), "gone soon").unwrap();
    fs::write(root.join(".env"), "TOKEN=secret").unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/tracked.txt"), "tracked").unwrap();
    run(root, &["add", "."]);
    run(root, &["commit", "--quiet", "-m", "initial"]);
    dir
}

#[test]
fn probe_rejects_a_repository_without_commits_and_a_subdirectory() {
    let empty = tempfile::tempdir().unwrap();
    run(empty.path(), &["init", "--quiet"]);
    assert!(probe(empty.path()).unwrap_err().contains("no commits"));

    let repo = repository();
    let sub = repo.path().join("sub");
    assert!(
        probe(&sub)
            .unwrap_err()
            .contains("top of its git repository")
    );
    assert!(probe(repo.path()).is_ok());
}

#[test]
fn populate_matches_the_project_and_excludes_secrets() {
    let repo = repository();
    let project = repo.path();
    fs::write(project.join("kept.txt"), "edited uncommitted").unwrap();
    fs::remove_file(project.join("removed.txt")).unwrap();
    fs::write(project.join("new.txt"), "untracked").unwrap();
    fs::write(project.join("ignored.log"), "ignored but not excluded").unwrap();
    fs::create_dir(project.join("cache")).unwrap();
    fs::write(project.join("cache/entry.bin"), "cached").unwrap();
    fs::write(project.join(".gitignore"), "*.log\ncache/\n").unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let dest = scratch.path().join("work");
    let mut link = populate(project, scratch.path(), &dest, u64::MAX).unwrap();

    assert_eq!(
        fs::read_to_string(dest.join("kept.txt")).unwrap(),
        "edited uncommitted"
    );
    assert!(
        !dest.join("removed.txt").exists(),
        "deleted files are removed"
    );
    assert_eq!(
        fs::read_to_string(dest.join("new.txt")).unwrap(),
        "untracked"
    );
    assert_eq!(
        fs::read_to_string(dest.join("ignored.log")).unwrap(),
        "ignored but not excluded"
    );
    assert_eq!(
        fs::read_to_string(dest.join("cache/entry.bin")).unwrap(),
        "cached"
    );
    assert!(
        !dest.join(".env").exists(),
        "excluded secrets are not visible"
    );
    assert_eq!(
        fs::read_to_string(dest.join("sub/tracked.txt")).unwrap(),
        "tracked"
    );
    assert!(
        !dest.join(".git").is_dir(),
        "the parent's git data is not visible"
    );
    assert!(!dest.join(".git").exists(), "the worktree file is detached");

    let admin = project.join(".git/worktrees");
    assert!(admin.is_dir(), "git registered the worktree");
    link.remove().unwrap();
    assert!(!admin.join("work").exists(), "administrative entry removed");
    assert!(link.remove().is_ok(), "remove is idempotent");
}

#[test]
fn populate_stops_at_the_dependency_budget() {
    let repo = repository();
    let project = repo.path();
    fs::write(project.join("new.txt"), "0123456789").unwrap();
    let scratch = tempfile::tempdir().unwrap();

    let error = populate(project, scratch.path(), &scratch.path().join("small"), 4)
        .err()
        .expect("a copy over the budget must fail");
    assert!(
        error.contains("isolated dependency budget exceeded"),
        "unexpected error: {error}"
    );
    assert!(populate(project, scratch.path(), &scratch.path().join("large"), 10).is_ok());
}
