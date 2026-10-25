use super::*;

const ID: &str = "01234567-89AB-CDEF-0123-456789abcdef";

fn target_with(extra: &[(&str, &str)]) -> Result<Option<String>> {
    eligible_target(true, |key| {
        extra
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_owned())
    })
}

#[test]
fn session_id_is_exact_and_accepts_iterm_prefix() {
    for raw in [ID.to_owned(), format!("w0t1p2:{ID}")] {
        assert_eq!(
            target_with(&[("TERM_PROGRAM", "iTerm.app"), ("ITERM_SESSION_ID", &raw)]).unwrap(),
            Some(ID.to_owned())
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
fn unknown_terminals_platforms_and_multiplexers_do_not_launch() {
    assert!(
        eligible_target(false, |_| Some("iTerm.app".into()))
            .unwrap()
            .is_none()
    );
    for terminal in ["", "Apple_Terminal", "WezTerm"] {
        assert!(
            target_with(&[("TERM_PROGRAM", terminal), ("ITERM_SESSION_ID", ID)])
                .unwrap()
                .is_none()
        );
    }
    for mux in ["TMUX", "STY", "ZELLIJ", "ZELLIJ_SESSION_NAME"] {
        assert!(
            target_with(&[
                ("TERM_PROGRAM", "iTerm.app"),
                ("ITERM_SESSION_ID", ID),
                (mux, "active")
            ])
            .unwrap()
            .is_none()
        );
    }
    assert!(target_with(&[("TERM_PROGRAM", "iTerm.app")]).is_err());
    assert!(
        target().unwrap().is_none(),
        "unit tests must never launch Automation"
    );
}

#[test]
fn shell_quoted_command_is_one_script_argument_not_source() {
    let words = [
        "/a path/dial",
        "-C",
        "/project/with 'quotes' and \"double\"; $(touch nope)",
        "watch",
        "session",
    ];
    let command = shlex::try_join(words).unwrap();
    assert_eq!(shlex::split(&command).unwrap(), words);
    let args = script_args(ID, &command);
    assert_eq!(args, ["-e", SCRIPT, "--", ID, command.as_str()]);
    assert!(!SCRIPT.contains(&command));
    assert!(SCRIPT.contains("split vertically with default profile command watchCommand"));
}
