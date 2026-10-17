use super::*;
use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    flags: SessionFlagArgs,
}

fn parse(args: &[&str]) -> SessionFlagArgs {
    let mut argv = vec!["dial"];
    argv.extend_from_slice(args);
    Cli::try_parse_from(argv).expect("parse").flags
}

#[test]
fn parses_fork_and_bare_from_flags() {
    assert_eq!(parse(&["--fork", "abc"]).fork.as_deref(), Some("abc"));
    assert_eq!(parse(&["--from-claude"]).from_claude.as_deref(), Some(""));
    assert_eq!(
        parse(&["--from-codex", "/x/rollout.jsonl"])
            .from_codex
            .as_deref(),
        Some("/x/rollout.jsonl")
    );
}

#[test]
fn selection_rejects_conflicts() {
    let ctx = Context::default();
    let both = parse(&["--from-claude", "--from-codex"]);
    assert!(both.selection(ctx).is_err());

    let with_fork = parse(&["--from-claude", "a.jsonl", "--fork", "x"]);
    assert!(with_fork.selection(ctx).is_err());

    let with_resume = Context {
        continue_or_resume: true,
        ..ctx
    };
    assert!(
        parse(&["--from-codex", "a.jsonl"])
            .selection(with_resume)
            .is_err()
    );

    let no_session = Context {
        no_session: true,
        ..ctx
    };
    assert!(parse(&["--fork", "x"]).selection(no_session).is_err());
    assert!(
        parse(&["--from-claude", "a.jsonl"])
            .selection(no_session)
            .is_err()
    );

    let protocol = Context {
        protocol_mode: true,
        ..ctx
    };
    assert!(
        parse(&["--from-codex", "a.jsonl"])
            .selection(protocol)
            .is_err()
    );
}

#[test]
fn selection_maps_flags_to_actions() {
    let ctx = Context::default();
    assert_eq!(parse(&[]).selection(ctx).unwrap(), None);
    assert_eq!(
        parse(&["--fork", " abc "]).selection(ctx).unwrap(),
        Some(Selection::Fork("abc".into()))
    );
    assert_eq!(
        parse(&["--from-claude", "/t/s.jsonl"])
            .selection(ctx)
            .unwrap(),
        Some(Selection::Import {
            source: Source::Claude,
            path: Some(PathBuf::from("/t/s.jsonl")),
        })
    );
    assert_eq!(
        parse(&["--from-codex"]).selection(ctx).unwrap(),
        Some(Selection::Import {
            source: Source::Codex,
            path: None
        })
    );
}

#[test]
fn path_selector_detection() {
    assert!(is_path_selector("./s/abc"));
    assert!(is_path_selector("C:\\s\\abc"));
    assert!(is_path_selector("rollout.jsonl"));
    assert!(!is_path_selector("0f3a9c"));
}
