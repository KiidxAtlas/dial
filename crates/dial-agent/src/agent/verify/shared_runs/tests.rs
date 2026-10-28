use super::*;

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn fingerprint_tracks_head_uncommitted_content_command_and_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    // The test tempdir may sit inside another repository; a fresh repository without commits
    // has no HEAD, so nothing is shareable yet.
    git(root, &["init", "-q"]);
    assert!(
        tree_fingerprint(root, "cargo test", "").is_none(),
        "no sharing without a HEAD commit"
    );
    std::fs::write(root.join("lib.rs"), "fn a() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
    let clean = tree_fingerprint(root, "cargo test", "").unwrap();
    assert_eq!(tree_fingerprint(root, "cargo test", "").unwrap(), clean);
    assert_ne!(tree_fingerprint(root, "cargo check", "").unwrap(), clean);
    assert_ne!(tree_fingerprint(root, "cargo test", "sub").unwrap(), clean);

    std::fs::write(root.join("lib.rs"), "fn a() { }\n").unwrap();
    let edited = tree_fingerprint(root, "cargo test", "").unwrap();
    assert_ne!(edited, clean, "an uncommitted edit changes the tree");
    std::fs::write(root.join("new.rs"), "fn b() {}\n").unwrap();
    let untracked = tree_fingerprint(root, "cargo test", "").unwrap();
    assert_ne!(untracked, edited, "an untracked file changes the tree");
    std::fs::write(root.join("new.rs"), "fn b() { }\n").unwrap();
    assert_ne!(tree_fingerprint(root, "cargo test", "").unwrap(), untracked);

    // Ignored build output never affects the identity.
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    let ignored = tree_fingerprint(root, "cargo test", "").unwrap();
    std::fs::create_dir(root.join("target")).unwrap();
    std::fs::write(root.join("target/out"), "artifact").unwrap();
    assert_eq!(tree_fingerprint(root, "cargo test", "").unwrap(), ignored);
}

#[test]
fn completion_labels_yield_exit_codes_only_for_normal_exits() {
    assert_eq!(completion_exit_code("exited (exit code 0)"), Some(0));
    assert_eq!(
        completion_exit_code("exited (exit code 101); boom"),
        Some(101)
    );
    assert_eq!(completion_exit_code("exited (killed by signal 9)"), None);
    assert_eq!(completion_exit_code("running"), None);
}

#[test]
fn shared_text_labels_the_result_as_a_peer_observation() {
    let entry = RunEntry {
        owner: "0".repeat(32),
        session_id: "abc".into(),
        command: "cargo test".into(),
        fingerprint: "f".into(),
        status: RunStatus::Failed,
        exit_code: Some(101),
        summary: "error[E0433]".into(),
        artifact: Some("artifact://log".into()),
        started_unix: 0,
        finished_unix: Some(now()),
    };
    let text = shared_result_text(&entry);
    for needle in [
        "not run here",
        "abc",
        "failed (exit code 101)",
        "artifact://log",
        "error[E0433]",
        "Run the same command again",
    ] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
}
