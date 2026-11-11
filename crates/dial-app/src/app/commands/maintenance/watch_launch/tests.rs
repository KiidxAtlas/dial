use super::*;

const ID: &str = "01234567-89AB-CDEF-0123-456789abcdef";

fn detect_with(macos: bool, vars: &[(&str, &str)]) -> Result<Option<Launcher>> {
    detect_on(macos, false, vars)
}

fn detect_on(macos: bool, windows: bool, vars: &[(&str, &str)]) -> Result<Option<Launcher>> {
    detect(macos, windows, |key| {
        vars.iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_owned())
    })
}

fn target_with(extra: &[(&str, &str)]) -> Result<Option<Launcher>> {
    detect_with(true, extra)
}

fn iterm(session: &str) -> Option<Launcher> {
    Some(Launcher::ITerm2 {
        session: session.to_owned(),
    })
}

fn argv() -> Vec<String> {
    [
        "/a path/dial",
        "-C",
        "/project/with 'quotes' and \"double\"; $(touch nope)",
        "watch",
        "session",
    ]
    .map(str::to_owned)
    .to_vec()
}

#[test]
fn session_id_is_exact_and_accepts_iterm_prefix() {
    for raw in [ID.to_owned(), format!("w0t1p2:{ID}")] {
        assert_eq!(
            target_with(&[("TERM_PROGRAM", "iTerm.app"), ("ITERM_SESSION_ID", &raw)]).unwrap(),
            iterm(ID)
        );
    }
    for raw in [
        "",
        "current session",
        "01234567",
        "01234567-89AB-CDEF-0123-456789abcdeg",
    ] {
        assert!(target_with(&[("TERM_PROGRAM", "iTerm.app"), ("ITERM_SESSION_ID", raw)]).is_err());
    }
    assert!(SCRIPT.contains("if (unique ID of aSession) is wantedID then"));
    assert!(!SCRIPT.contains("current session"));
}

#[test]
fn iterm2_wins_over_inherited_outer_terminal_variables_and_needs_macos() {
    assert_eq!(
        target_with(&[
            ("TERM_PROGRAM", "iTerm.app"),
            ("ITERM_SESSION_ID", ID),
            ("WEZTERM_PANE", "3"),
            ("KITTY_WINDOW_ID", "1"),
        ])
        .unwrap(),
        iterm(ID)
    );
    assert!(
        detect_with(
            false,
            &[("TERM_PROGRAM", "iTerm.app"), ("ITERM_SESSION_ID", ID)]
        )
        .unwrap()
        .is_none()
    );
    assert!(target_with(&[("TERM_PROGRAM", "iTerm.app")]).is_err());
}

#[test]
fn unknown_terminals_do_not_launch() {
    for terminal in ["", "Apple_Terminal", "WezTerm", "vscode"] {
        assert!(
            target_with(&[("TERM_PROGRAM", terminal), ("ITERM_SESSION_ID", ID)])
                .unwrap()
                .is_none()
        );
    }
    assert!(target_with(&[]).unwrap().is_none());
    // Empty variables, as left by `VAR= dial`, count as unset.
    assert!(
        target_with(&[("TMUX", ""), ("ZELLIJ", ""), ("WEZTERM_PANE", "")])
            .unwrap()
            .is_none()
    );
    assert!(
        target().unwrap().is_none(),
        "unit tests must never open a real pane"
    );
}

#[test]
fn tmux_is_detected_first_and_targets_its_own_pane() {
    let tmux = ("TMUX", "/tmp/tmux-501/default,1234,0");
    assert_eq!(
        target_with(&[tmux, ("TMUX_PANE", "%12")]).unwrap(),
        Some(Launcher::Tmux {
            pane: Some("%12".into())
        })
    );
    assert_eq!(
        target_with(&[tmux]).unwrap(),
        Some(Launcher::Tmux { pane: None })
    );
    // Inside tmux the outer terminal's variables are inherited and may be stale.
    assert_eq!(
        target_with(&[
            tmux,
            ("TERM_PROGRAM", "iTerm.app"),
            ("ITERM_SESSION_ID", ID),
            ("WEZTERM_PANE", "3"),
            ("KITTY_WINDOW_ID", "1"),
            ("KITTY_LISTEN_ON", "unix:/tmp/kitty"),
            ("ZELLIJ", "0"),
        ])
        .unwrap(),
        Some(Launcher::Tmux { pane: None })
    );
    for pane in ["12", "%", "%1a", "%1 -t x"] {
        assert!(target_with(&[tmux, ("TMUX_PANE", pane)]).is_err(), "{pane}");
    }
}

#[test]
fn zellij_is_detected_by_either_variable() {
    for key in ["ZELLIJ", "ZELLIJ_SESSION_NAME"] {
        assert_eq!(
            target_with(&[
                (key, "0"),
                ("TERM_PROGRAM", "iTerm.app"),
                ("ITERM_SESSION_ID", ID),
                ("WEZTERM_PANE", "3"),
            ])
            .unwrap(),
            Some(Launcher::Zellij)
        );
    }
}

#[test]
fn screen_is_refused_with_a_reason() {
    let error = target_with(&[
        ("STY", "1234.pts-0.host"),
        ("TERM_PROGRAM", "iTerm.app"),
        ("ITERM_SESSION_ID", ID),
    ])
    .unwrap_err();
    assert!(format!("{error:#}").contains("GNU screen"));
}

#[test]
fn wezterm_needs_a_numeric_pane() {
    assert_eq!(
        target_with(&[("WEZTERM_PANE", "7"), ("TERM_PROGRAM", "WezTerm")]).unwrap(),
        Some(Launcher::WezTerm { pane: "7".into() })
    );
    for pane in ["x", "7; rm", "-1"] {
        assert!(target_with(&[("WEZTERM_PANE", pane)]).is_err(), "{pane}");
    }
}

#[test]
fn kitty_needs_a_remote_control_socket() {
    assert_eq!(
        target_with(&[
            ("KITTY_WINDOW_ID", "4"),
            ("KITTY_LISTEN_ON", "unix:/tmp/kitty-123")
        ])
        .unwrap(),
        Some(Launcher::Kitty {
            socket: "unix:/tmp/kitty-123".into(),
            window: "4".into(),
        })
    );
    let error = target_with(&[("KITTY_WINDOW_ID", "4")]).unwrap_err();
    assert!(format!("{error:#}").contains("allow_remote_control"));
    assert!(
        target_with(&[
            ("KITTY_WINDOW_ID", "id:4"),
            ("KITTY_LISTEN_ON", "unix:/tmp/kitty")
        ])
        .is_err()
    );
}

#[test]
fn split_commands_pass_the_watcher_as_separate_arguments() {
    let argv = argv();
    let tail = |args: &[String]| {
        let split = args.iter().position(|arg| arg == "--").unwrap();
        args[split + 1..].to_vec()
    };
    let cases = [
        (
            Launcher::Tmux {
                pane: Some("%3".into()),
            },
            "tmux",
            vec!["split-window", "-h", "-d", "-t", "%3"],
        ),
        (
            Launcher::Tmux { pane: None },
            "tmux",
            vec!["split-window", "-h", "-d"],
        ),
        (
            Launcher::Zellij,
            "zellij",
            vec!["run", "--direction", "right"],
        ),
        (
            Launcher::WezTerm { pane: "7".into() },
            "wezterm",
            vec!["cli", "split-pane", "--right", "--pane-id", "7"],
        ),
        (
            Launcher::Kitty {
                socket: "unix:/tmp/kitty".into(),
                window: "4".into(),
            },
            "kitty",
            vec![
                "@",
                "--to=unix:/tmp/kitty",
                "launch",
                "--location=vsplit",
                "--keep-focus",
                "--next-to=id:4",
            ],
        ),
    ];
    for (launcher, expected_program, prefix) in cases {
        let (program, args) = invocation(&launcher, &argv).unwrap();
        assert_eq!(program, expected_program);
        assert_eq!(args[..prefix.len()], prefix);
        assert_eq!(args[prefix.len()], "--");
        assert_eq!(tail(&args), argv, "{launcher:?}");
    }
}

#[test]
fn shell_quoted_command_is_one_script_argument_not_source() {
    let words = argv();
    let command = shlex::try_join(words.iter().map(String::as_str)).unwrap();
    assert_eq!(shlex::split(&command).unwrap(), words);
    let args = script_args(ID, &command);
    assert_eq!(args, ["-e", SCRIPT, "--", ID, command.as_str()]);
    assert!(!SCRIPT.contains(&command));
    assert!(SCRIPT.contains("split vertically with default profile command watchCommand"));
    let (program, args) = invocation(&iterm(ID).unwrap(), &words).unwrap();
    assert_eq!(program, "/usr/bin/osascript");
    assert_eq!(args, ["-e", SCRIPT, "--", ID, command.as_str()]);
}

#[test]
fn every_fallback_names_the_exact_watch_command() {
    let command = shlex::try_join(argv().iter().map(String::as_str)).unwrap();
    let tmux = Launcher::Tmux { pane: None };
    let error = anyhow::anyhow!("no server running");
    let refusal = target_with(&[("STY", "1.x")]).unwrap_err();
    for message in [
        opening_message(&tmux, &command),
        failed_message(&tmux, &error, &command),
        unavailable_message(None, &command),
        unavailable_message(Some(&refusal), &command),
    ] {
        assert!(
            message.ends_with(&format!("another terminal: {command}")),
            "{message}"
        );
    }
    assert!(unavailable_message(None, &command).contains(SUPPORTED));
    assert!(failed_message(&tmux, &error, &command).contains("no server running"));
    let kitty = Launcher::Kitty {
        socket: "unix:/tmp/kitty".into(),
        window: "1".into(),
    };
    assert!(failed_message(&kitty, &error, &command).contains("allow_remote_control"));
    assert!(
        failed_message(&iterm(ID).unwrap(), &error, &command).contains("Automation permission")
    );
    assert_eq!(
        opened_message(&tmux),
        "Opened the live feed in a tmux split."
    );
    assert_eq!(first_line("\n  usage: x  \nmore"), "usage: x");
    assert_eq!(first_line(&"e".repeat(500)).len(), 200);
}

#[test]
fn windows_terminal_splits_only_on_native_windows() {
    let wt = [("WT_SESSION", "0f1e2d3c-4b5a-6978-8a9b-0c1d2e3f4a5b")];
    assert_eq!(
        detect_on(false, true, &wt).unwrap(),
        Some(Launcher::WindowsTerminal)
    );
    // WSL inherits WT_SESSION through WSLENV but cannot hand wt.exe a Linux path.
    assert_eq!(detect_on(false, false, &wt).unwrap(), None);
    assert_eq!(detect_on(false, true, &[("WT_SESSION", "")]).unwrap(), None);
    // A multiplexer running inside Windows Terminal still owns the split.
    assert_eq!(
        detect_on(false, true, &[("WT_SESSION", "x"), ("TMUX", "/tmp/t")]).unwrap(),
        Some(Launcher::Tmux { pane: None })
    );
    let argv: Vec<String> = [
        r"C:\Program Files\dial\dial.exe",
        "-C",
        r"C:\a;b",
        "watch",
        "s",
    ]
    .map(str::to_owned)
    .to_vec();
    let (program, args) = invocation(&Launcher::WindowsTerminal, &argv).unwrap();
    assert_eq!(program, "wt");
    assert_eq!(
        args,
        [
            "-w",
            "0",
            "split-pane",
            "-V",
            "--",
            r"C:\Program Files\dial\dial.exe",
            "-C",
            r"C:\a\;b",
            "watch",
            "s"
        ]
    );
}

#[test]
fn windows_manual_command_is_pasteable_into_powershell() {
    let words: Vec<String> = [
        r"C:\Users\O'Neil\dial.exe",
        "-C",
        r"C:\work\my project",
        "watch",
        "01ABC-def",
    ]
    .map(str::to_owned)
    .to_vec();
    assert_eq!(
        display_command(&words, true).unwrap(),
        r"& 'C:\Users\O''Neil\dial.exe' -C 'C:\work\my project' watch 01ABC-def"
    );
    assert_eq!(
        display_command(&[r"C:\d.exe".to_owned(), "x\u{2019}y".to_owned()], true).unwrap(),
        "& 'C:\\d.exe' 'x\u{2019}\u{2019}y'"
    );
    let unix = argv();
    assert_eq!(
        display_command(&unix, false).unwrap(),
        shlex::try_join(unix.iter().map(String::as_str)).unwrap()
    );
}

#[test]
fn launch_never_spawns_under_test() {
    let cancel = AtomicBool::new(false);
    for launcher in [Launcher::Tmux { pane: None }, iterm(ID).unwrap()] {
        assert!(launch(&launcher, &argv(), &cancel).is_err());
    }
}
