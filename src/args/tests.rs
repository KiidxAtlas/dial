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

#[test]
fn a_lone_near_miss_word_is_a_mistyped_subcommand_not_a_request() {
    let suggest = |args: &[&str]| {
        let cli = Cli::try_parse_from(args).unwrap();
        mistyped_subcommand(&cli, args)
    };
    assert_eq!(suggest(&["dial", "sesions"]).as_deref(), Some("sessions"));
    assert_eq!(suggest(&["dial", "modles"]).as_deref(), Some("models"));
    assert_eq!(
        suggest(&["dial", "plugins"]),
        None,
        "aliases parse directly"
    );
    assert_eq!(suggest(&["dial", "img"]), None, "aliases parse directly");
    assert_eq!(suggest(&["dial", "bnech"]).as_deref(), Some("benchmark"));
    // Real requests, short words, explicit `--` and unrelated words still reach the model.
    assert_eq!(suggest(&["dial", "fix", "the", "sessions"]), None);
    assert_eq!(suggest(&["dial", "hi"]), None);
    assert_eq!(suggest(&["dial", "--", "sesions"]), None);
    // Explicit print mode selects a model request even if its text resembles a command.
    assert_eq!(suggest(&["dial", "-p", "status"]), None);
    assert_eq!(suggest(&["dial", "--print", "sesions"]), None);
    assert_eq!(suggest(&["dial", "refactor"]), None);
    assert_eq!(suggest(&["dial", "Sesions"]), None);
    let message = mistyped_subcommand_error("sesions", "sessions").to_string();
    assert!(message.contains("'sessions'") && message.contains("dial -- sesions"));
}

#[test]
fn subcommand_help_lists_its_own_options_before_grouped_global_flags() {
    use clap::CommandFactory;
    // Global flags reach subcommands when the command is built, as it is when parsing.
    let mut command = Cli::command();
    command.build();
    let help = command
        .find_subcommand_mut("config")
        .unwrap()
        .render_help()
        .to_string();
    let own = help.find("--json").unwrap();
    let model = help.find("Model and provider:").unwrap();
    let safety = help.find("Permissions and isolation:").unwrap();
    assert!(own < model && model < safety, "{help}");
    let root = Cli::command().render_help().to_string();
    assert!(root.contains("Examples:") && root.contains("Print mode exit codes:"));
    assert!(
        root.find("Running a prompt:").unwrap() < root.find("Protocols and diagnostics:").unwrap()
    );
    assert!(Cli::try_parse_from(["dial", "--sandbox", "nope"]).is_err());
}
