use super::*;
use clap::Parser;

#[derive(Parser)]
struct Wrapper {
    #[command(flatten)]
    args: Args,
}

fn parse(args: &[&str]) -> Result<Args, clap::Error> {
    let mut all = vec!["review"];
    all.extend_from_slice(args);
    Wrapper::try_parse_from(all).map(|wrapper| wrapper.args)
}

#[test]
fn each_target_kind_resolves_to_its_review_target_words() {
    let cases: &[(&[&str], &str, &str)] = &[
        (&[], "uncommitted", ""),
        (&["--uncommitted"], "uncommitted", ""),
        (&["--base", "main"], "base main", ""),
        (&["--commit", "abc123"], "commit abc123", ""),
        (
            &["check", "error", "paths"],
            "uncommitted",
            "check error paths",
        ),
    ];
    for (args, target, instructions) in cases {
        let request = parse(args).unwrap().request().unwrap();
        assert_eq!(request.target, *target, "{args:?}");
        assert_eq!(request.instructions, *instructions, "{args:?}");
    }
}

#[test]
fn conflicting_targets_are_rejected_by_the_parser() {
    for conflict in [
        &["--uncommitted", "--base", "main"][..],
        &["--uncommitted", "--commit", "abc"],
        &["--base", "main", "--commit", "abc"],
        &["--base", "main", "focus words"],
        &["--commit", "abc", "focus words"],
        &["--uncommitted", "focus words"],
    ] {
        assert!(parse(conflict).is_err(), "{conflict:?} must conflict");
    }
}

#[test]
fn refs_must_be_single_words() {
    for bad in [
        &["--base=-x"][..],
        &["--base", "two words"],
        &["--commit", " "],
    ] {
        let args = parse(bad).unwrap();
        assert!(args.request().is_err(), "{bad:?} must not form a target");
    }
}

#[test]
fn the_output_flags_are_parsed_on_the_subcommand() {
    let args = parse(&[
        "--output-format",
        "json",
        "-o",
        "report.json",
        "--base",
        "main",
    ])
    .unwrap();
    assert_eq!(args.output_format, Some(OutputFormat::Json));
    assert_eq!(args.output_last_message, Some(PathBuf::from("report.json")));
}

#[test]
fn guidance_expands_the_review_template_for_the_frozen_target() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(dir.path(), dir.path().join("dial")).unwrap();
    let request = Request {
        target: "base main".into(),
        instructions: "focus on error handling".into(),
    };
    let text = guidance(&layout, &request);
    assert!(text.contains("Review base main"), "{text}");
    assert!(text.ends_with("Additional review focus: focus on error handling"));
}

#[test]
fn unknown_base_or_commit_fails_before_the_review_starts() {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "first"]);
    let layout = Layout::with_user_dir(dir.path(), dir.path().join("dial")).unwrap();
    let error = parse(&["--base", "no-such-branch"])
        .unwrap()
        .run(&layout)
        .expect_err("unknown base is refused");
    assert!(
        error.to_string().contains("no such branch or commit"),
        "{error}"
    );
    assert!(parse(&["--commit", "nope"]).unwrap().run(&layout).is_err());
    assert!(parse(&["--base", "HEAD"]).unwrap().run(&layout).is_ok());
}
