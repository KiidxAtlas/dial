use super::*;

#[test]
fn slash_cli_hints_resolve_through_the_real_clap_tree() {
    use clap::CommandFactory;
    let root = Cli::command();
    for spec in dial::ui::command_surface::COMMANDS {
        let Some(cli) = spec.cli else { continue };
        let mut command = &root;
        for word in cli.split_whitespace() {
            command = command
                .get_subcommands()
                .find(|sub| {
                    sub.get_name() == word || sub.get_all_aliases().any(|alias| alias == word)
                })
                .unwrap_or_else(|| panic!("/{} CLI hint has no subcommand: {cli}", spec.name));
        }
        assert_ne!(command.get_name(), root.get_name());
    }
}

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

#[test]
fn restore_is_a_resume_flag_that_excludes_print_mode_and_no_session() {
    let cli = Cli::try_parse_from(["dial", "--restore"]).unwrap();
    assert!(cli.restore);
    assert!(!cli.continue_session);
    assert!(Cli::try_parse_from(["dial", "--restore", "--print", "hi"]).is_err());
    assert!(Cli::try_parse_from(["dial", "--restore", "--no-session"]).is_err());
}

#[test]
fn rules_cli_samples_are_exclusive_and_do_not_clash_with_global_flags() {
    use dial::cli::rules::RulesAction;
    // `--reasoning`, `--tool`, `--args` and `--agent` must stay distinct from global flag ids
    // (a `--thinking` sample once collided with the global thinking level and panicked).
    for (args, ok) in [
        (&["dial", "rules", "test", "some text"][..], true),
        (
            &[
                "dial",
                "rules",
                "test",
                "--reasoning",
                "plan",
                "--agent",
                "w",
            ],
            true,
        ),
        (
            &["dial", "rules", "test", "--tool", "bash", "--args", "{}"],
            true,
        ),
        (&["dial", "--thinking", "high", "rules", "test", "x"], true),
        (&["dial", "rules", "--json", "list"], true),
        (&["dial", "rules"], true),
        (&["dial", "rules", "test", "--tool", "bash"], false),
        (
            &[
                "dial", "rules", "test", "x", "--tool", "bash", "--args", "{}",
            ],
            false,
        ),
        (
            &["dial", "rules", "test", "x", "--reasoning", "plan"],
            false,
        ),
    ] {
        assert_eq!(Cli::try_parse_from(args).is_ok(), ok, "{args:?}");
    }
    let cli = Cli::try_parse_from([
        "dial",
        "rules",
        "test",
        "--reasoning",
        "plan",
        "--agent",
        "w",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Developer(DevCommand::Rules(args))) if matches!(
            &args.action,
            Some(RulesAction::Test { reasoning: Some(plan), agent, .. }) if plan == "plan" && agent == "w"
        )
    ));
}

#[test]
fn weights_analyze_and_improve_parse_without_extra_flags() {
    let cli = Cli::try_parse_from(["dial", "weights", "analyze", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Developer(DevCommand::Weights {
            action: WeightsCommand::Analyze { json: true }
        }))
    ));
    let cli =
        Cli::try_parse_from(["dial", "weights", "improve", "stop", "endless", "loops"]).unwrap();
    let Some(Command::Developer(DevCommand::Weights {
        action: WeightsCommand::Improve { goal },
    })) = cli.command
    else {
        panic!("expected weights improve");
    };
    assert_eq!(goal.join(" "), "stop endless loops");
    assert!(Cli::try_parse_from(["dial", "weights", "improve"]).is_err());
}

#[test]
fn dev_commands_parse_under_dev_and_at_the_hidden_top_level() {
    let grouped = Cli::try_parse_from(["dial", "dev", "gc", "--apply"])
        .unwrap()
        .command
        .map(Command::normalized);
    let top = Cli::try_parse_from(["dial", "gc", "--apply"])
        .unwrap()
        .command
        .map(Command::normalized);
    for command in [grouped, top] {
        assert!(matches!(
            command,
            Some(Command::Developer(DevCommand::Gc(_)))
        ));
    }
    let cli = Cli::try_parse_from(["dial", "dev", "bench", "--ablate-assistance"]).unwrap();
    assert!(matches!(
        cli.command.map(Command::normalized),
        Some(Command::Developer(DevCommand::Benchmark {
            ablate_assistance: true,
            ..
        }))
    ));

    let mut root = command();
    let visible: Vec<&str> = root
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .map(|sub| sub.get_name())
        .collect();
    assert!(visible.contains(&"dev"), "{visible:?}");
    assert!(visible.contains(&"sessions"), "{visible:?}");
    let dev = root.find_subcommand("dev").unwrap();
    for sub in dev.get_subcommands() {
        let name = sub.get_name();
        assert!(
            !visible.contains(&name),
            "{name} is listed at the top level"
        );
        assert!(root.find_subcommand(name).is_some(), "{name} not accepted");
    }
    let dev_help = root
        .find_subcommand_mut("dev")
        .unwrap()
        .render_help()
        .to_string();
    for listed in [
        "weights",
        "steering",
        "laya",
        "edit-bench",
        "anonymize",
        "gc",
    ] {
        assert!(dev_help.contains(listed), "{listed} missing:\n{dev_help}");
    }
}
