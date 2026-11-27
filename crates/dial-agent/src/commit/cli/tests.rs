use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn parse_accepts_all_flags_and_aliases() {
    let parsed = parse_commit_args(&args(&[
        "--push",
        "--dry-run",
        "--no-changelog",
        "--legacy",
        "-c",
        "focus on api",
        "--model",
        "small",
    ]))
    .unwrap()
    .unwrap();
    assert!(parsed.push && parsed.dry_run && parsed.no_changelog && parsed.legacy);
    assert_eq!(parsed.context.as_deref(), Some("focus on api"));
    assert_eq!(parsed.model.as_deref(), Some("small"));
}

#[test]
fn parse_rejects_unknown_flag_and_missing_values() {
    assert_eq!(
        parse_commit_args(&args(&["--bogus"])).unwrap_err(),
        "Error: Unknown flag --bogus"
    );
    assert_eq!(
        parse_commit_args(&args(&["--context"])).unwrap_err(),
        "Error: --context requires a value"
    );
    assert_eq!(
        parse_commit_args(&args(&["-m", "--push"])).unwrap_err(),
        "Error: --model requires a value"
    );
}

#[test]
fn help_returns_none() {
    assert_eq!(parse_commit_args(&args(&["-h"])).unwrap(), None);
}

#[test]
fn parse_accepts_json_flag_and_outcome_serializes_dry_run() {
    let parsed = parse_commit_args(&args(&["--dry-run", "--no-changelog", "--json"]))
        .unwrap()
        .unwrap();
    assert!(parsed.dry_run && parsed.no_changelog && parsed.json);
    let json = crate::commit::run::CommitRunOutcome::DryRun {
        message: "feat: json".into(),
        validation_error: Some("check body".into()),
    }
    .to_json();
    assert_eq!(json["kind"], "dry_run");
    assert_eq!(json["message"], "feat: json");
    assert_eq!(json["warning"], "check body");
}
