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
