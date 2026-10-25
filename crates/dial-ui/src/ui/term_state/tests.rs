use super::*;

fn iterm(tmux: bool) -> Integration {
    Integration {
        hyperlinks: !tmux,
        line_fragments: true,
        cwd: true,
        tab_color: true,
        attention: true,
        user_vars: true,
        progress: true,
        tmux,
    }
}

fn writer(caps: Integration) -> SignalWriter {
    SignalWriter {
        caps,
        host: "mac".to_owned(),
        sent: Signals::default(),
    }
}

#[test]
fn waiting_colors_the_tab_requests_attention_and_pauses_progress() {
    let mut writer = writer(iterm(false));
    let out = writer.update(&Signals {
        activity: Activity::Waiting,
        progress: Progress::Paused,
        ..Default::default()
    });
    assert_eq!(
        out,
        "\x1b]6;1;bg;red;brightness;230\x07\x1b]6;1;bg;green;brightness;64\x07\x1b]6;1;bg;blue;brightness;64\x07\
         \x1b]1337;RequestAttention=yes\x07\x1b]9;4;4\x07"
    );
    // Failed keeps the same tab color; attention is withdrawn.
    let out = writer.update(&Signals {
        activity: Activity::Failed,
        ..Default::default()
    });
    assert_eq!(out, "\x1b]1337;RequestAttention=no\x07\x1b]9;4;0\x07");
}

#[test]
fn unchanged_state_writes_nothing_and_vars_are_diffed() {
    let mut writer = writer(iterm(false));
    let want = Signals {
        vars: vec![("dialModel", "qwen".into()), ("dialState", "idle".into())],
        ..Default::default()
    };
    assert!(!writer.update(&want).is_empty());
    assert!(writer.is_current(&want));
    assert_eq!(writer.update(&want), "");
    let out = writer.update(&Signals {
        vars: vec![("dialModel", "qwen".into())],
        ..Default::default()
    });
    assert_eq!(out, "\x1b]1337;SetUserVar=dialState=\x07");
}

#[test]
fn user_var_values_are_one_bounded_line() {
    let mut writer = writer(iterm(false));
    let out = writer.update(&Signals {
        vars: vec![(
            "dialContext",
            format!("a\nb\x1b]0;x\x07{}", "z".repeat(500)),
        )],
        ..Default::default()
    });
    let encoded = out
        .strip_prefix("\x1b]1337;SetUserVar=dialContext=")
        .and_then(|rest| rest.strip_suffix('\x07'))
        .unwrap();
    let value = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    let value = String::from_utf8(value).unwrap();
    assert!(!value.chars().any(char::is_control), "{value:?}");
    assert_eq!(value.chars().count(), VAR_LIMIT);
}

#[test]
fn unsupported_signals_are_never_written() {
    let mut writer = writer(Integration::default());
    let want = Signals {
        activity: Activity::Waiting,
        progress: Progress::Percent(50),
        cwd: Some("/tmp".into()),
        vars: vec![("dialState", "waiting".into())],
    };
    assert_eq!(writer.update(&want), "");
    assert!(writer.is_current(&want));
    assert!(writer.is_clear());
}

#[test]
fn tmux_wraps_terminal_sequences_but_not_the_working_directory() {
    let mut writer = writer(iterm(true));
    let out = writer.update(&Signals {
        progress: Progress::Indeterminate,
        cwd: Some("/work/my proj".into()),
        ..Default::default()
    });
    assert_eq!(
        out,
        "\x1bPtmux;\x1b\x1b]9;4;3\x07\x1b\\\x1b]7;file://mac/work/my%20proj\x1b\\"
    );
}

#[test]
fn reset_restores_everything_but_the_directory() {
    let mut writer = writer(iterm(false));
    writer.update(&Signals {
        activity: Activity::Working,
        progress: Progress::Percent(10),
        cwd: Some("/work".into()),
        vars: vec![("dialState", "working".into())],
    });
    let preview = writer.reset_sequence();
    assert!(!writer.is_clear());
    assert_eq!(writer.reset(), preview);
    assert_eq!(
        preview,
        "\x1b]6;1;bg;*;default\x07\x1b]9;4;0\x07\x1b]1337;SetUserVar=dialState=\x07"
    );
    assert!(writer.is_clear());
    assert_eq!(writer.reset(), "");
}

#[test]
fn turn_outcomes_map_to_activity() {
    use crate::runtime::Phase;
    assert_eq!(Activity::after_turn(Phase::Completed), Activity::Done);
    assert_eq!(Activity::after_turn(Phase::NeedsInput), Activity::Waiting);
    assert_eq!(Activity::after_turn(Phase::Blocked), Activity::Failed);
    assert_eq!(Activity::after_turn(Phase::Interrupted), Activity::Idle);
}
