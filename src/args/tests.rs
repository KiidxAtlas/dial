use super::*;

#[test]
fn root_help_groups_every_visible_command_once_and_help_spelling_still_works() {
    let help = help_command().render_help().to_string();
    let everyday = help.find("Everyday commands:").expect("grouped");
    assert!(
        everyday < help.find("Inspect and research:").unwrap(),
        "{help}"
    );
    assert!(!help.contains("\nCommands:"), "{help}");
    let root = command();
    for sub in root.get_subcommands().filter(|sub| !sub.is_hide_set()) {
        let row = format!("\n  {} ", sub.get_name());
        assert_eq!(
            help.matches(&row).count(),
            1,
            "{} in {help}",
            sub.get_name()
        );
    }
    // Parsing is unchanged: commands still run and `dial help <cmd>` asks for that command's help.
    let parsed = help_command().try_get_matches_from(["dial", "stats", "--json"]);
    assert!(parsed.is_ok());
    let asked = help_spelling(["dial", "help", "sessions"].map(Into::into).to_vec());
    assert_eq!(asked, ["dial", "sessions", "--help"]);
    let err = help_command().try_get_matches_from(asked).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    assert!(err.to_string().contains("sessions"), "{err}");
}

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
fn clients_can_append_to_the_system_prompt_after_rpc() {
    // Dial Desktop starts `dial rpc <flags>`; the flag must parse after the subcommand too.
    for argv in [
        ["dial", "rpc", "--append-system-prompt", "Draw diagrams."],
        ["dial", "--append-system-prompt", "Draw diagrams.", "rpc"],
    ] {
        let cli = Cli::try_parse_from(argv).unwrap();
        assert!(matches!(cli.command, Some(Command::Rpc)), "{argv:?}");
        assert_eq!(
            cli.tool_flags.append_system_prompt.as_deref(),
            Some("Draw diagrams."),
            "{argv:?}"
        );
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
fn yolo_is_explicit_and_the_legacy_yolo_spellings_keep_full_auto() {
    let approval_of = |argv: &[&str]| {
        let cli = Cli::try_parse_from(argv).unwrap();
        crate::overlay::overlay(&cli).map(|table| {
            table
                .get("approval")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
    };
    assert_eq!(
        approval_of(&["dial", "--approval", "yolo"])
            .unwrap()
            .as_deref(),
        Some("yolo")
    );
    for argv in [
        &["dial", "-y"][..],
        &["dial", "--yolo"],
        &["dial", "--auto-approve"],
        &["dial", "--approval-mode", "yolo"],
    ] {
        assert_eq!(
            approval_of(argv).unwrap().as_deref(),
            Some("auto"),
            "{argv:?}"
        );
    }
    // Case and typos never elevate: clap refuses them before any mode is applied.
    for argv in [
        &["dial", "--approval", "YOLO"][..],
        &["dial", "--approval", "yol"],
    ] {
        assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
    }
    // Two approval spellings at once stay refused rather than picking one silently.
    assert!(approval_of(&["dial", "--approval", "yolo", "--approval-mode", "yolo"]).is_err());
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
fn weights_direction_takes_a_span_instead_of_negatives_and_edit_takes_add() {
    let cli = Cli::try_parse_from([
        "dial",
        "weights",
        "direction",
        "stop",
        "--experiment",
        "x",
        "--positive",
        "p.json",
        "--span-last",
        "64",
        "--span-before",
        "think_end",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Developer(DevCommand::Weights {
            action: WeightsCommand::Direction { negative: None, span_last: Some(64), span_before: Some(ref before), .. }
        })) if before == "think_end"
    ));
    assert!(
        Cli::try_parse_from([
            "dial",
            "weights",
            "direction",
            "d",
            "--experiment",
            "x",
            "--positive",
            "p.json"
        ])
        .is_err()
    );
    let cli = Cli::try_parse_from([
        "dial",
        "weights",
        "edit",
        "e",
        "--experiment",
        "x",
        "--method",
        "add",
        "--direction",
        "stop",
        "--layers",
        "3,5",
        "--alpha",
        "0.1",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Developer(DevCommand::Weights {
            action: WeightsCommand::Edit { method: Some(ref method), .. }
        })) if method == "add"
    ));
}

#[test]
fn weights_analyze_and_improve_parse_without_extra_flags() {
    let cli = Cli::try_parse_from(["dial", "weights", "analyze", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Developer(DevCommand::Weights {
            action: WeightsCommand::Analyze {
                json: true,
                explainer: None
            }
        }))
    ));
    let cli =
        Cli::try_parse_from(["dial", "weights", "analyze", "--explainer", "openai/gpt-5"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Developer(DevCommand::Weights {
            action: WeightsCommand::Analyze { json: false, explainer: Some(ref model) }
        })) if model == "openai/gpt-5"
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

    // Benchmarks use existing subscription sign-ins unless told not to; `--share-auth` stays
    // accepted for scripts written when sharing was opt-in.
    let isolated = |args: &[&str]| match Cli::try_parse_from(args)
        .unwrap()
        .command
        .map(Command::normalized)
    {
        Some(Command::Developer(DevCommand::Benchmark { isolated_auth, .. })) => isolated_auth,
        _ => panic!("not a benchmark: {args:?}"),
    };
    assert!(!isolated(&["dial", "bench", "suite.toml"]));
    assert!(!isolated(&["dial", "bench", "suite.toml", "--share-auth"]));
    assert!(isolated(&[
        "dial",
        "bench",
        "suite.toml",
        "--isolated-auth"
    ]));
    assert!(
        Cli::try_parse_from(["dial", "bench", "s.toml", "--share-auth", "--isolated-auth"])
            .is_err()
    );

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

#[test]
fn subcommand_help_omits_sectioned_globals_that_still_parse() {
    use clap::FromArgMatches;
    let mut root = command();
    let help = root
        .find_subcommand_mut("ps")
        .unwrap()
        .render_help()
        .to_string();
    assert!(!help.contains("--thinking"), "{help}");
    assert!(help.contains("--json"), "{help}");
    assert!(help.contains("also apply"), "{help}");
    let matches = command()
        .try_get_matches_from(["dial", "review", "--uncommitted", "-m", "some-model"])
        .unwrap();
    let cli = Cli::from_arg_matches(&matches).unwrap();
    assert_eq!(cli.model.as_deref(), Some("some-model"));
    let matches = command()
        .try_get_matches_from(["dial", "-m", "root-model", "ps"])
        .unwrap();
    let cli = Cli::from_arg_matches(&matches).unwrap();
    assert_eq!(cli.model.as_deref(), Some("root-model"));
}

#[test]
fn a_global_option_named_by_a_subcommand_stays_in_its_help() {
    let mut root = command();
    let add = root
        .find_subcommand_mut("schedule")
        .unwrap()
        .find_subcommand_mut("add")
        .unwrap();
    let help = add.render_help().to_string();
    assert!(help.contains("-y, --yes"), "{help}");
    assert!(!help.contains("--thinking"), "{help}");
    assert!(mentions_flag("unless --yes or", "yes"));
    assert!(!mentions_flag("--yes-paid", "yes"));
}

#[test]
fn user_facing_arguments_all_have_help_text() {
    fn walk(command: &clap::Command, path: &str, missing: &mut Vec<String>) {
        for arg in command.get_arguments() {
            let id = arg.get_id().as_str();
            if arg.is_hide_set() || matches!(id, "help" | "version") {
                continue;
            }
            if arg.get_help().is_none() && arg.get_long_help().is_none() {
                missing.push(format!("{path} {id}"));
            }
        }
        for sub in command.get_subcommands() {
            // `dev` is developer and research tooling, documented in its own pages.
            if sub.is_hide_set() || matches!(sub.get_name(), "dev" | "help") {
                continue;
            }
            walk(sub, &format!("{path} {}", sub.get_name()), missing);
        }
    }
    let mut missing = Vec::new();
    walk(&command(), "dial", &mut missing);
    assert!(missing.is_empty(), "arguments without help: {missing:#?}");
}

#[test]
fn share_accepts_json_flag() {
    let cli = Cli::try_parse_from(["dial", "share", "abc123", "--gist", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Share {
            id: Some(ref id),
            gist: true,
            json: true,
            ..
        }) if id == "abc123"
    ));
}
