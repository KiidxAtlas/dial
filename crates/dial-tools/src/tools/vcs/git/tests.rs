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
    crate::tools::testkit::disable_inherited_git_filters(dir.path());
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
    let ws = ws.with_artifacts(crate::storage::artifact::ArtifactStore::new(state.path()).unwrap());
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
    assert!(show.text.len() < crate::tools::vcs::gh_common::MAX_OUTPUT + 1024);
    let uris = crate::storage::artifact::uris(&show.text);
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

#[cfg(unix)]
#[test]
fn worktree_add_keep_changes_keeps_untracked_symlinks_as_links() {
    let (_dir, ws) = repo();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "outside\n").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        ws.root().join("file-link"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.root().join("dir-link")).unwrap();
    let holder = tempfile::tempdir().unwrap();
    let wt = holder.path().join("links");
    assert!(worktree_add_keep_changes(ws.root(), &wt, "links").unwrap());
    for name in ["file-link", "dir-link"] {
        let meta = std::fs::symlink_metadata(wt.join(name)).unwrap();
        assert!(meta.file_type().is_symlink(), "{name} stays a link");
    }
    assert_eq!(
        std::fs::read_link(wt.join("file-link")).unwrap(),
        outside.path().join("secret.txt")
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

#[test]
fn jj_file_escapes_as_a_jj_string_literal() {
    assert_eq!(jj_file("src/a.rs"), r#"file:"src/a.rs""#);
    assert_eq!(
        jj_file("we\"ird\\na\u{e9}me.rs"),
        "file:\"we\\\"ird\\\\na\u{e9}me.rs\""
    );
    assert_eq!(jj_file("a\tb\n\u{1b}\u{1}"), r#"file:"a\tb\n\e\x01""#);
}

#[test]
fn large_status_reports_counts_samples_and_retains_every_path() {
    let (dir, ws) = repo();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(crate::storage::artifact::ArtifactStore::new(state.path()).unwrap());
    std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
    std::fs::create_dir(dir.path().join("new")).unwrap();
    for i in 0..120 {
        std::fs::write(dir.path().join(format!("new/{i:03}.txt")), "new\n").unwrap();
    }
    let output = call(&ws, json!({"action":"status"}));
    assert!(!output.is_error, "{}", output.text);
    assert!(
        output.text.contains("Status: 121 entries"),
        "{}",
        output.text
    );
    assert!(output.text.contains("\" M\"=1"), "{}", output.text);
    assert!(output.text.contains("\"??\"=120"), "{}", output.text);
    assert!(output.text.contains("Sample paths (first 20)"));
    assert!(!output.text.contains("new/119.txt"));
    assert!(output.text.len() < 4096);
    let uris = crate::storage::artifact::uris(&output.text);
    assert_eq!(uris.len(), 1);
    let retained = ws.artifacts().unwrap().retrieve(uris[0], 1, 500).unwrap();
    assert!(retained.contains("new/119.txt"), "{retained}");
    for i in 0..120 {
        assert!(
            retained.contains(&format!("?? new/{i:03}.txt")),
            "missing retained path {i}: {retained}"
        );
    }
    let scoped = call(&ws, json!({"action":"status", "path":"new/119.txt"}));
    assert!(!scoped.is_error, "{}", scoped.text);
    assert!(scoped.text.contains("?? new/119.txt"));
    assert!(!scoped.text.contains("a.txt"));
    assert!(!scoped.text.contains("new/000.txt"));
    assert!(call(&ws, json!({"action":"status", "path":"../outside"})).is_error);
}

#[test]
fn status_without_artifacts_keeps_complete_evidence_inline() {
    let (_dir, ws) = repo();
    let text = format!(
        "## main\n{}",
        (0..500)
            .map(|i| format!("?? path-{i:03}.txt\n"))
            .collect::<String>()
    );
    let output = status_output(&ws, &text);
    assert!(output.contains(&text));
    assert!(output.contains("no artifact store"));
    assert!(output.contains("complete status returned inline"));
    assert!(!output.contains("Sample only"));
}

#[test]
fn status_sample_bounds_individual_paths_without_splitting_utf8() {
    let (_dir, ws) = repo();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(crate::storage::artifact::ArtifactStore::new(state.path()).unwrap());
    let text = format!("## main\n?? {}\n", "界".repeat(5000));
    let output = status_output(&ws, &text);
    assert!(output.contains("Status: 1 entries"));
    assert!(output.contains("[path shortened]"));
    assert!(!output.contains('\u{fffd}'));
    assert!(output.len() < 2048);
}

#[test]
fn failed_status_retention_keeps_complete_evidence_inline() {
    let (_dir, ws) = repo();
    let state = tempfile::tempdir().unwrap();
    let root = state.path().join("artifacts");
    let store = crate::storage::artifact::ArtifactStore::new(&root).unwrap();
    std::fs::remove_dir(&root).unwrap();
    std::fs::write(&root, "blocked").unwrap();
    let ws = ws.with_artifacts(store);
    let text = format!(
        "## main\n{}",
        (0..500)
            .map(|i| format!("?? path-{i:03}.txt\n"))
            .collect::<String>()
    );
    let output = status_output(&ws, &text);
    assert!(output.contains(&text));
    assert!(output.contains("could not be retained"));
    assert!(!output.contains("Sample only"));
}

#[test]
fn capped_status_artifact_keeps_complete_evidence_inline() {
    let (_dir, ws) = repo();
    let state = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(state.path())
        .unwrap()
        .with_limits(&crate::storage::artifact::ArtifactConfig {
            max_bytes: 1024,
            head_bytes: 512,
        });
    let ws = ws.with_artifacts(store);
    let text = format!(
        "## main\n{}",
        (0..500)
            .map(|i| format!("?? path-{i:03}.txt\n"))
            .collect::<String>()
    );
    let output = status_output(&ws, &text);
    assert!(output.contains(&text));
    assert!(output.contains("artifact retention was incomplete"));
    assert!(!output.contains("Sample only"));
}

#[test]
fn host_reads_refuse_executable_filters_without_running_them() {
    for (key, attributes) in [
        ("filter.audit.clean", false),
        ("filter.audit.clean", true),
        ("filter.audit.process", true),
        ("filter.audit.smudge", true),
    ] {
        let (dir, ws) = repo();
        let canary = dir.path().join("filter-ran");
        assert!(
            Command::new("git")
                .args(["config", key, "touch filter-ran; cat"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        if attributes {
            std::fs::write(dir.path().join(".gitattributes"), "* filter=audit\n").unwrap();
        }
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        for action in ["status", "diff"] {
            let result = call(&ws, json!({"action":action}));
            assert!(
                result.is_error && result.text.contains("filter configuration"),
                "{key} {action}: {}",
                result.text
            );
            assert!(!canary.exists(), "{key} executed during {action}");
            assert!(run_checked(ws.root(), &[action], false).is_err());
            assert!(!canary.exists());
        }
    }
}

#[test]
fn host_diff_and_show_refuse_text_conversion_without_running_it() {
    let (dir, ws) = repo();
    assert!(
        Command::new("git")
            .args(["config", "diff.audit.textconv", "touch textconv-ran; cat"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(dir.path().join(".gitattributes"), "a.txt diff=audit\n").unwrap();
    std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
    for args in [vec!["diff"], vec!["show", "HEAD", "--", "a.txt"]] {
        assert!(run_checked(ws.root(), &args, false).is_err(), "{args:?}");
        assert!(!dir.path().join("textconv-ran").exists(), "{args:?}");
    }
}

#[cfg(unix)]
#[test]
fn host_diff_disables_repository_external_driver_without_running_it() {
    let (dir, ws) = repo();
    let driver = dir.path().join(".git/audit-external-diff");
    let marker = dir.path().join(".git/external-diff-ran");
    std::fs::write(
        &driver,
        "#!/bin/sh\nprintf called > \"$(dirname \"$0\")/external-diff-ran\"\n",
    )
    .unwrap();
    crate::platform::fs::set_mode(&driver, 0o755).unwrap();
    assert!(
        Command::new("git")
            .args(["config", "diff.external"])
            .arg(&driver)
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    std::fs::write(dir.path().join("a.txt"), "modified\n").unwrap();
    assert!(
        Command::new("git")
            .args(["diff", "--", "a.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "called");
    std::fs::remove_file(&marker).unwrap();
    let output = run_checked(ws.root(), &["diff", "--", "a.txt"], false).unwrap();
    assert!(output.contains("+modified"), "{output}");
    assert!(!marker.exists(), "host diff executed the external driver");
}

#[cfg(unix)]
#[test]
fn host_history_reads_never_execute_repository_signature_verifiers() {
    let (dir, ws) = repo();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let tree = git(&["rev-parse", "HEAD^{tree}"]);
    // Git only needs an armored signature header to dispatch to the configured verifier.
    // This deliberately invalid signature needs neither a real key nor a GPG installation.
    let commit = format!(
        "tree {}\nauthor Test <test@example.invalid> 1700000000 +0000\ncommitter Test <test@example.invalid> 1700000000 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n ZmFrZQ==\n -----END PGP SIGNATURE-----\n\naudit signed history\n",
        tree.trim()
    );
    let object = dir.path().join(".git/audit-commit");
    std::fs::write(&object, commit).unwrap();
    let oid = git(&[
        "hash-object",
        "-t",
        "commit",
        "-w",
        object.to_str().unwrap(),
    ]);
    git(&["update-ref", "HEAD", oid.trim()]);
    let verifier = dir.path().join(".git/audit-gpg");
    let marker = dir.path().join(".git/signature-verifier-ran");
    std::fs::write(
        &verifier,
        "#!/bin/sh\ncat >/dev/null\nprintf called > \"$(dirname \"$0\")/signature-verifier-ran\"\nexit 1\n",
    )
    .unwrap();
    crate::platform::fs::set_mode(&verifier, 0o755).unwrap();
    git(&["config", "gpg.format", "openpgp"]);
    git(&["config", "gpg.program", verifier.to_str().unwrap()]);
    git(&["config", "log.showSignature", "true"]);

    for args in [&["log", "-1"][..], &["show", "HEAD"][..]] {
        let reference = git(args);
        assert!(
            reference.contains("audit signed history"),
            "{args:?}: {reference}"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "called",
            "unguarded {args:?} must exercise the verifier"
        );
        std::fs::remove_file(&marker).unwrap();
        let guarded = run_checked(ws.root(), args, false).unwrap();
        assert!(
            guarded.contains("audit signed history"),
            "{args:?}: {guarded}"
        );
        assert!(!marker.exists(), "guarded {args:?} executed the verifier");
    }
}

#[test]
fn invalid_git_configuration_fails_closed_on_host_reads() {
    let (dir, ws) = repo();
    std::fs::write(dir.path().join(".git/config"), "[invalid\n").unwrap();
    let result = call(&ws, json!({"action":"status"}));
    assert!(
        result.is_error && result.text.contains("could not be inspected safely"),
        "{}",
        result.text
    );
}

#[test]
fn included_filter_and_textconv_config_is_refused_until_explicitly_disabled() {
    for (key, config, attributes) in [
        (
            "filter.included.clean",
            "[filter \"included\"]\nclean = touch included-ran; cat\n",
            "* filter=included\n",
        ),
        (
            "diff.included.textconv",
            "[diff \"included\"]\ntextconv = touch included-ran; cat\n",
            "* diff=included\n",
        ),
    ] {
        let (dir, ws) = repo();
        let included = dir.path().join("inherited-config");
        std::fs::write(&included, config).unwrap();
        assert!(
            Command::new("git")
                .args(["config", "include.path"])
                .arg(&included)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success(),
            "{key}"
        );
        std::fs::write(dir.path().join(".gitattributes"), attributes).unwrap();
        std::fs::write(dir.path().join("a.txt"), "modified\n").unwrap();
        assert!(call(&ws, json!({"action":"status"})).is_error, "{key}");
        assert!(
            run_checked(ws.root(), &["show", "HEAD"], false).is_err(),
            "{key}"
        );
        assert!(!dir.path().join("included-ran").exists(), "{key}");
        assert!(
            Command::new("git")
                .args(["config", key, ""])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success(),
            "{key}"
        );
        let out = call(&ws, json!({"action":"status"}));
        assert!(
            !out.is_error && out.text.contains("a.txt"),
            "{key}: {}",
            out.text
        );
        let args = &["show", "HEAD", "--", "a.txt"];
        let history = run_checked(ws.root(), args, false).unwrap();
        assert!(history.contains("init"), "{key}: {history}");
        let history = run_read(ws.root(), args, &[], LOCAL_TIMEOUT).unwrap();
        assert!(history.contains("init"), "{key}: {history}");
        assert!(!dir.path().join("included-ran").exists(), "{key}");
    }
}

#[test]
fn global_filter_activated_by_untracked_attributes_is_never_run_by_inspection() {
    let (dir, _ws) = repo();
    let global = dir.path().join("global-config");
    let included = dir.path().join("included-global-config");
    std::fs::write(
        &included,
        "[filter \"global-audit\"]\nclean = touch global-filter-ran; cat\n",
    )
    .unwrap();
    let root = dir.path().to_string_lossy().replace('\\', "/");
    assert!(
        Command::new("git")
            .args(["config", "--file"])
            .arg(&global)
            .arg(format!("includeIf.gitdir:{root}/.path"))
            .arg(&included)
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    std::fs::write(dir.path().join(".gitattributes"), "* filter=global-audit\n").unwrap();
    std::fs::write(dir.path().join("a.txt"), "modified\n").unwrap();
    // Establish that this is an execution risk, not merely an unused config entry.
    assert!(
        Command::new("git")
            .args(["diff", "--no-textconv"])
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let canary = dir.path().join("global-filter-ran");
    assert!(canary.exists());
    std::fs::remove_file(&canary).unwrap();
    let mut query = filter_config_query(dir.path());
    query
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let error = inspect_filter_config(query, &NEVER_CANCELLED).unwrap_err();
    assert!(error.contains("filter configuration"), "{error}");
    assert!(!canary.exists());
    // The same global include does not activate for a different repository root.
    let (other, _) = repo();
    let mut query = filter_config_query(other.path());
    query
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    assert!(inspect_filter_config(query, &NEVER_CANCELLED).is_ok());
}

#[test]
fn a_worker_snapshot_never_ingests_build_output_or_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    for path in [
        "src/main.rs",
        "target/debug/huge.rlib",
        "dist/app.js",
        "web/node_modules/pkg/index.js",
        "pkg/build/out.o",
        "py/__pycache__/m.pyc",
        ".cache/blob",
    ] {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, "x").unwrap();
    }
    initialize_private_snapshot(dir.path()).unwrap();
    let listed = std::process::Command::new("git")
        .args(["ls-files"])
        .env("GIT_DIR", dir.path().join(".git"))
        .env("GIT_WORK_TREE", dir.path())
        .output()
        .unwrap();
    let files = String::from_utf8(listed.stdout).unwrap();
    assert_eq!(files.trim(), "src/main.rs", "{files}");
}
