use super::*;

#[test]
fn approval_choices_and_aliases_are_accepted_by_the_cli() {
    for &policy in dial::config::types::Approval::NAMES {
        let cli = Cli::try_parse_from(["dial", "--approval", policy]).unwrap();
        assert_eq!(cli.approval.as_deref(), Some(policy));
    }
    for &(alias, _) in dial::cli::cli_modes::APPROVAL_ALIASES {
        let cli = Cli::try_parse_from(["dial", "--approval-mode", alias]).unwrap();
        assert_eq!(cli.approval_mode.as_deref(), Some(alias));
    }
    assert!(Cli::try_parse_from(["dial", "--approval", "invalid"]).is_err());
}

#[test]
fn schedule_cli_accepts_comma_and_repeated_allow_lists() {
    let cli = Cli::try_parse_from([
        "dial",
        "schedule",
        "add",
        "--every",
        "1h",
        "--allow",
        "read,edit",
        "--allow",
        "bash",
        "inspect the project",
    ])
    .unwrap();
    let Some(Command::Schedule {
        action:
            ScheduleCommand::Add {
                prompt,
                every,
                watch,
                allow,
                id,
            },
    }) = cli.command
    else {
        panic!("expected schedule add");
    };
    let entry =
        dial::schedule::Entry::new(id, prompt, every.as_deref(), watch, allow, cli.yes).unwrap();
    assert_eq!(entry.allow, ["read", "edit", "bash"]);
    assert_eq!(entry.approval, "readonly");
    assert_eq!(entry.prompt, "inspect the project");
}

#[test]
fn watch_cli_requires_a_session_and_accepts_once_and_project_directory() {
    let cli = Cli::try_parse_from([
        "dial",
        "-C",
        "/tmp/project",
        "watch",
        "session_123",
        "--once",
    ])
    .unwrap();
    assert!(
        matches!(cli.command, Some(Command::Watch(args)) if args.session == "session_123" && args.once)
    );
    assert!(Cli::try_parse_from(["dial", "watch"]).is_err());
}
