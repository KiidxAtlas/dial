use super::*;
#[test]
fn parses_metrics() {
    assert_eq!(parse_metric("ASI score=3.25", "score"), Some(3.25));
    assert_eq!(parse_metric("{\"score\":4}", "score"), Some(4.0));
    assert_eq!(parse_metric("score=NaN", "score"), None);
}

#[test]
fn direction_is_max_or_min_only() {
    assert!(parse_direction("MAX").unwrap());
    assert!(!parse_direction(" min ").unwrap());
    assert!(parse_direction("maximize").is_err());
    assert!(parse_direction("").is_err());
}

fn git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Dial",
            "-c",
            "user.email=dial@example.invalid",
        ])
        .args(args)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn runs_refuse_a_dirty_tree_and_exit_returns_to_the_origin_branch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let state = dir.path().join("state");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    crate::tools::testkit::disable_inherited_git_filters(&root);
    fs::write(root.join("f.txt"), "1\n").unwrap();
    git(&root, &["add", "f.txt"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    init(&root, &state, "speed", "echo metric=1", "metric", true).unwrap();
    assert_eq!(current(&state).unwrap().unwrap().origin, "main");
    fs::write(root.join("dirty.txt"), "x").unwrap();
    let error = run(&root, &state).unwrap_err().to_string();
    assert!(
        error.contains("uncommitted changes") && error.contains("git stash"),
        "{error}"
    );
    fs::remove_file(root.join("dirty.txt")).unwrap();
    assert!(exit(&root, &state).unwrap().starts_with("Returned to main"));
    assert_eq!(
        run_checked(&root, &["branch", "--show-current"], false)
            .unwrap()
            .trim(),
        "main"
    );
}

#[test]
fn clean_check_preserves_the_git_inspection_error() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    crate::tools::testkit::disable_inherited_git_filters(dir.path());
    // Filter drivers are disabled per read; a text-conversion driver is still refused.
    git(
        dir.path(),
        &["config", "diff.audit.textconv", "executable-filter"],
    );
    let cause = run_checked(dir.path(), &["status", "--porcelain"], false)
        .unwrap_err()
        .to_string();
    let error = ensure_clean(dir.path(), "benchmark")
        .unwrap_err()
        .to_string();
    assert!(error.contains(&cause), "cause: {cause}; error: {error}");
}

#[test]
fn policy_parses_caps_and_stops_on_patience_or_the_iteration_cap() {
    let policy = Policy::parse("").unwrap();
    assert_eq!(
        (policy.max_iterations, policy.patience),
        (Policy::DEFAULT_ITERATIONS, Policy::DEFAULT_PATIENCE)
    );
    let mut policy = Policy::parse("4 --patience 2").unwrap();
    assert_eq!((policy.max_iterations, policy.patience), (4, 2));
    assert!(Policy::parse("0").is_err());
    assert!(Policy::parse("--patience").is_err());
    assert!(Policy::parse("many").is_err());
    assert!(Policy::parse("101").is_err());

    assert_eq!(policy.next(), 1);
    assert_eq!(policy.record(Verdict::Reverted), None);
    // A kept run resets patience.
    assert_eq!(policy.record(Verdict::Kept), None);
    assert_eq!(policy.record(Verdict::NoChange), None);
    assert_eq!(policy.next(), 4);
    let stop = policy.record(Verdict::Crashed).unwrap();
    assert!(stop.contains("without improvement"), "{stop}");

    let mut capped = Policy::parse("2 --patience 5").unwrap();
    assert_eq!(capped.record(Verdict::Kept), None);
    assert!(capped.record(Verdict::Kept).unwrap().contains("cap of 2"));
}

#[test]
fn evaluate_keeps_improvements_and_reverts_everything_else() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let state = dir.path().join("state");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    crate::tools::testkit::disable_inherited_git_filters(&root);
    git(&root, &["config", "user.name", "Dial"]);
    git(&root, &["config", "user.email", "dial@example.invalid"]);
    fs::write(root.join("score.txt"), "score=5\n").unwrap();
    git(&root, &["add", "score.txt"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    init(&root, &state, "speed", "cat score.txt", "score", true).unwrap();
    // The embedded shell re-executes the current binary, which in a unit test is the test harness.
    let ws = crate::tools::Workspace::new(&root)
        .unwrap()
        .with_shell_backend(crate::config::types::ShellBackend::System);
    let baseline = ensure_baseline(&ws, &state).unwrap().unwrap();
    assert_eq!(baseline.metric, 5.0);
    assert!(ensure_baseline(&ws, &state).unwrap().is_none());

    // An improvement is committed and kept.
    let start = head(&root).unwrap();
    fs::write(root.join("score.txt"), "score=7\n").unwrap();
    let kept = evaluate(&ws, &state, &start, 1).unwrap();
    assert_eq!(kept.verdict, Verdict::Kept);
    assert_eq!(kept.metric, Some(7.0));
    assert_eq!(head(&root).unwrap(), kept.commit.clone().unwrap());
    assert_eq!(best(&state).unwrap(), Some(7.0));

    // A regression, including a new file, is reverted to the iteration's start.
    let start = head(&root).unwrap();
    fs::write(root.join("score.txt"), "score=6\n").unwrap();
    fs::write(root.join("extra.txt"), "x").unwrap();
    let reverted = evaluate(&ws, &state, &start, 2).unwrap();
    assert_eq!(reverted.verdict, Verdict::Reverted);
    assert_eq!(head(&root).unwrap(), start);
    assert_eq!(
        fs::read_to_string(root.join("score.txt")).unwrap(),
        "score=7\n"
    );
    assert!(!root.join("extra.txt").exists());

    // A broken benchmark output is a crash and is reverted too.
    fs::write(root.join("score.txt"), "no metric\n").unwrap();
    let crashed = evaluate(&ws, &state, &start, 3).unwrap();
    assert_eq!(crashed.verdict, Verdict::Crashed);
    assert!(crashed.detail.contains("metric"), "{}", crashed.detail);
    assert_eq!(head(&root).unwrap(), start);

    // Nothing changed: nothing is measured.
    let unchanged = evaluate(&ws, &state, &start, 4).unwrap();
    assert_eq!(unchanged.verdict, Verdict::NoChange);

    // Leaving the experiment branch stops the loop without touching the tree.
    git(&root, &["switch", "-q", "main"]);
    let error = evaluate(&ws, &state, &start, 5).unwrap_err().to_string();
    assert!(error.contains("not the experiment branch"), "{error}");

    git(&root, &["switch", "-q", "autoresearch/speed"]);
    let prompt = iteration_prompt(&state, 6, 10).unwrap();
    assert!(prompt.contains("iteration 6 of 10") && prompt.contains("maximize `score`"));
    assert!(prompt.contains("Best so far: 7"), "{prompt}");
}
