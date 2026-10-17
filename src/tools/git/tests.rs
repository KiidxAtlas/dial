use super::*;

fn repo() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let sh = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
                .status
                .success(),
            "{args:?}"
        )
    };
    sh(&["init", "-q", "-b", "main"]);
    sh(&["config", "user.name", "t"]);
    sh(&["config", "user.email", "t@t"]);
    sh(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    sh(&["add", "a.txt"]);
    sh(&["commit", "-q", "-m", "init", "--no-verify"]);
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

fn call(ws: &Workspace, args: Value) -> ToolOutput {
    dispatch(ws, &args, false, &NEVER_CANCELLED).unwrap_or_else(ToolOutput::err)
}

#[test]
fn read_and_write_flow() {
    let (dir, ws) = repo();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "new\n").unwrap();
    assert!(call(&ws, json!({"action":"status"})).text.contains("a.txt"));
    assert!(
        call(&ws, json!({"action":"diff","path":"a.txt"}))
            .text
            .contains("+two")
    );
    assert!(!call(&ws, json!({"action":"add","paths":["a.txt","b.txt"]})).is_error);
    assert!(
        call(&ws, json!({"action":"diff","staged":true}))
            .text
            .contains("+new")
    );
    assert!(!call(&ws, json!({"action":"unstage","paths":["b.txt"]})).is_error);
    let commit = call(&ws, json!({"action":"commit","message":"second"}));
    assert!(!commit.is_error, "{}", commit.text);
    assert!(
        call(&ws, json!({"action":"log","limit":5}))
            .text
            .contains("second")
    );
    assert!(
        call(&ws, json!({"action":"show","rev":"HEAD"}))
            .text
            .contains("second")
    );
    assert!(
        call(&ws, json!({"action":"status"})).text.contains("b.txt"),
        "unstaged file stays out of the commit"
    );
    assert!(!call(&ws, json!({"action":"branch_create","name":"feature/x"})).is_error);
    assert!(
        call(&ws, json!({"action":"branches"}))
            .text
            .contains("feature/x")
    );
    assert!(!call(&ws, json!({"action":"switch","name":"main"})).is_error);
}

#[test]
fn hostile_arguments_are_refused() {
    let (_dir, ws) = repo();
    for rev in ["--output=/tmp/x", "-p", "a..b", "x y", ""] {
        assert!(
            call(&ws, json!({"action":"show","rev":rev})).is_error,
            "{rev}"
        );
    }
    assert!(call(&ws, json!({"action":"add","paths":["../outside"]})).is_error);
    assert!(
        call(&ws, json!({"action":"add","paths":["--all"]})).is_error,
        "an option-looking path is treated as a path, which does not exist"
    );
    assert!(call(&ws, json!({"action":"push","remote":"--upload-pack=x"})).is_error);
    assert!(call(&ws, json!({"action":"reset"})).is_error);
    assert!(call(&ws, json!({"action":"commit","message":"  "})).is_error);
}

#[test]
fn hooks_run_by_default_and_bypass_is_reported() {
    let (dir, ws) = repo();
    let hook = dir.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\nexit 1\n").unwrap();
    crate::platform::fs::set_mode(&hook, 0o755).unwrap();
    std::fs::write(dir.path().join("c.txt"), "c\n").unwrap();
    assert!(!call(&ws, json!({"action":"add","paths":["c.txt"]})).is_error);
    let blocked = call(&ws, json!({"action":"commit","message":"x"}));
    assert!(
        blocked.is_error,
        "the failing pre-commit hook must block: {}",
        blocked.text
    );
    assert!(
        dir.path().join("hook-ran").exists(),
        "repository hook should run by default"
    );
    std::fs::remove_file(dir.path().join("hook-ran")).unwrap();
    let bypassed = dispatch(
        &ws,
        &json!({"action":"commit","message":"x"}),
        true,
        &NEVER_CANCELLED,
    )
    .unwrap();
    assert!(!bypassed.is_error, "{}", bypassed.text);
    assert!(
        bypassed.text.contains("git.bypass_hooks"),
        "{}",
        bypassed.text
    );
    assert!(
        !dir.path().join("hook-ran").exists(),
        "bypass must skip the hook"
    );
    // Reads never run hooks.
    assert!(!call(&ws, json!({"action":"status"})).is_error);
    assert!(!dir.path().join("hook-ran").exists());
    for (action, asks) in [
        ("status", false),
        ("diff", false),
        ("blame", false),
        ("pr_view", false),
        ("commit", true),
        ("push", true),
        ("add", true),
        ("pr_create", true),
    ] {
        assert_eq!(
            Git::default().needs_approval(&json!({"action":action})),
            asks,
            "{action}"
        );
    }
}

#[test]
fn large_output_drains_fully_and_spills_to_an_artifact() {
    let (dir, ws) = repo();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(crate::tools::artifact::ArtifactStore::new(state.path()).unwrap());
    // Far beyond the pipe buffer and the old 4×cap read limit that caused SIGPIPE failures.
    let big: String = (0..60_000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("big.txt"), &big).unwrap();
    assert!(!call(&ws, json!({"action":"add","paths":["big.txt"]})).is_error);
    let commit = dispatch(
        &ws,
        &json!({"action":"commit","message":"big"}),
        true,
        &NEVER_CANCELLED,
    )
    .unwrap();
    assert!(!commit.is_error, "{}", commit.text);
    let show = call(&ws, json!({"action":"show","rev":"HEAD"}));
    assert!(
        !show.is_error,
        "large output must not fail: {}",
        &show.text[show.text.len().saturating_sub(300)..]
    );
    assert!(show.text.len() < MAX_OUTPUT + 1024);
    let uris = crate::tools::artifact::uris(&show.text);
    assert_eq!(
        uris.len(),
        1,
        "{}",
        &show.text[show.text.len().saturating_sub(300)..]
    );
    let probe: Value = serde_json::from_str(
        &ws.artifacts()
            .unwrap()
            .retrieve_bytes(uris[0], 0, 1)
            .unwrap(),
    )
    .unwrap();
    let total = probe["total_bytes"].as_u64().unwrap();
    assert!(
        total > big.len() as u64 + 60_000,
        "artifact must hold the complete output, got {total} bytes"
    );
}

#[test]
fn blame_reports_line_authors() {
    let (_dir, ws) = repo();
    let blame = call(
        &ws,
        json!({"action":"blame","path":"a.txt","start":1,"end":1}),
    );
    assert!(!blame.is_error, "{}", blame.text);
    assert!(blame.text.contains("one"), "{}", blame.text);
    assert!(
        call(
            &ws,
            json!({"action":"blame","path":"a.txt","start":3,"end":1})
        )
        .is_error
    );
}

#[test]
fn parses_porcelain_worktree_blocks() {
    let text = "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\nworktree /r-wt\nHEAD bbb\ndetached\nlocked reason\n\nworktree /bare\nbare\n";
    let trees = parse_worktrees(text);
    assert_eq!(trees.len(), 3);
    assert_eq!(trees[0].path, PathBuf::from("/r"));
    assert_eq!(trees[0].branch.as_deref(), Some("main"));
    assert_eq!(trees[0].head, "aaa");
    assert!(trees[1].detached && trees[1].locked);
    assert_eq!(trees[1].branch, None);
    assert!(trees[2].bare);
}

#[test]
fn worktree_add_list_remove_round_trip() {
    let (_dir, ws) = repo();
    let holder = tempfile::tempdir().unwrap();
    let wt = holder.path().join("wt");
    worktree_add(ws.root(), &wt, "feat/x", "HEAD").unwrap();
    let trees = worktree_list(ws.root()).unwrap();
    assert_eq!(trees.len(), 2);
    assert_eq!(trees[0].branch.as_deref(), Some("main"));
    assert!(trees.iter().any(|t| t.branch.as_deref() == Some("feat/x")));
    worktree_remove(ws.root(), &wt, false).unwrap();
    assert_eq!(worktree_list(ws.root()).unwrap().len(), 1);
    assert!(!wt.exists());
}

#[test]
fn worktree_remove_refuses_dirty_without_force() {
    let (_dir, ws) = repo();
    let holder = tempfile::tempdir().unwrap();
    let wt = holder.path().join("dirty");
    worktree_add(ws.root(), &wt, "dirty", "HEAD").unwrap();
    std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
    assert!(worktree_remove(ws.root(), &wt, false).is_err());
    worktree_remove(ws.root(), &wt, true).unwrap();
    assert!(!wt.exists());
}

#[test]
fn worktree_add_keep_changes_carries_tracked_and_untracked_edits() {
    let (_dir, ws) = repo();
    std::fs::write(ws.root().join("a.txt"), "edited\n").unwrap();
    std::fs::write(ws.root().join("new.txt"), "fresh\n").unwrap();
    let holder = tempfile::tempdir().unwrap();
    let wt = holder.path().join("kept");
    assert!(worktree_add_keep_changes(ws.root(), &wt, "kept").unwrap());
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("new.txt")).unwrap(),
        "fresh\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.root().join("a.txt")).unwrap(),
        "edited\n"
    );
}

#[test]
fn clean_source_checkout_reverts_tracked_edits_and_removes_untracked_files() {
    let (_dir, ws) = repo();
    std::fs::write(ws.root().join("a.txt"), "edited\n").unwrap();
    std::fs::write(ws.root().join("new.txt"), "fresh\n").unwrap();
    clean_source_checkout(ws.root()).unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.root().join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!ws.root().join("new.txt").exists());
}
