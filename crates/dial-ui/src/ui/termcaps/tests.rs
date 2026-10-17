use super::*;
use std::collections::HashMap;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |name| map.get(name).cloned()
}

#[test]
fn terminals_are_identified_and_get_their_notification_escape() {
    let cases = [
        (
            vec![("KITTY_WINDOW_ID", "1")],
            TerminalId::Kitty,
            NotifyProtocol::Osc99,
        ),
        (
            vec![("TERM_PROGRAM", "ghostty")],
            TerminalId::Ghostty,
            NotifyProtocol::Osc9,
        ),
        (
            vec![("TERM_PROGRAM", "iTerm.app")],
            TerminalId::Iterm2,
            NotifyProtocol::Osc9,
        ),
        (
            vec![("TERM", "foot")],
            TerminalId::Foot,
            NotifyProtocol::Osc777,
        ),
        (
            vec![("TERM_PROGRAM", "vscode")],
            TerminalId::Vscode,
            NotifyProtocol::Bell,
        ),
        (
            vec![("TERM", "xterm")],
            TerminalId::Base,
            NotifyProtocol::Bell,
        ),
    ];
    for (vars, id, notify) in cases {
        let info = detect(&env(&vars));
        assert_eq!((info.id, info.notify), (id, notify), "{vars:?}");
    }
}

#[test]
fn sequences_match_each_protocol_and_cannot_be_terminated_early() {
    let plain = env(&[]);
    let info = |notify| TerminalInfo {
        id: TerminalId::Base,
        notify,
    };
    let osc9 = notification_sequence(
        &plain,
        info(NotifyProtocol::Osc9),
        NotifyKind::Completion,
        "dial",
        "done\x1b]0;evil\x07",
        "s",
    );
    assert_eq!(osc9, "\x1b]9;dial: done ]0;evil\x1b\\");
    assert_eq!(
        notification_sequence(
            &plain,
            info(NotifyProtocol::Osc99),
            NotifyKind::Question,
            "dial",
            "waiting",
            "s"
        ),
        "\x1b]99;;dial: waiting\x1b\\"
    );
    assert_eq!(
        notification_sequence(
            &plain,
            info(NotifyProtocol::Osc777),
            NotifyKind::Error,
            "a;b",
            "c;d",
            "s"
        ),
        "\x1b]777;notify;a,b;c,d\x1b\\"
    );
    assert_eq!(
        notification_sequence(
            &plain,
            info(NotifyProtocol::Bell),
            NotifyKind::Completion,
            "t",
            "b",
            "s"
        ),
        "\x07"
    );
}

#[test]
fn multiplexers_wrap_or_add_a_bell() {
    let info = TerminalInfo {
        id: TerminalId::Ghostty,
        notify: NotifyProtocol::Osc9,
    };
    let tmux = notification_sequence(
        &env(&[("TMUX", "/tmp/x")]),
        info,
        NotifyKind::Completion,
        "t",
        "b",
        "s",
    );
    assert_eq!(tmux, "\x1bPtmux;\x1b\x1b]9;t: b\x1b\x1b\\\x1b\\\x07");
    let zellij = notification_sequence(
        &env(&[("ZELLIJ", "0")]),
        info,
        NotifyKind::Completion,
        "t",
        "b",
        "s",
    );
    assert_eq!(zellij, "\x1b]9;t: b\x1b\\\x07");
}

#[test]
fn warp_structured_events_replace_desktop_toasts() {
    let vars = env(&[
        ("WARP_CLI_AGENT_PROTOCOL_VERSION", "1"),
        ("TERM_PROGRAM", "WarpTerminal"),
    ]);
    let info = detect(&vars);
    let sequence = notification_sequence(&vars, info, NotifyKind::Question, "dial", "x", "abc");
    assert!(sequence.starts_with("\x1b]777;notify;warp://cli-agent;{"));
    assert!(sequence.contains("\"event\":\"question_asked\""));
    assert!(sequence.contains("\"session_id\":\"abc\""));
    assert!(sequence.ends_with("\x07"));
}

#[test]
fn turn_notices_follow_the_authoritative_outcome() {
    use crate::runtime::Phase;
    // (outcome, notify, notify_errors) -> expected (kind, body), or None for silence.
    let cases = [
        (
            Phase::Completed,
            true,
            true,
            Some((NotifyKind::Completion, "Turn finished")),
        ),
        (
            Phase::Completed,
            true,
            false,
            Some((NotifyKind::Completion, "Turn finished")),
        ),
        (Phase::Completed, false, true, None),
        (
            Phase::NeedsInput,
            true,
            false,
            Some((NotifyKind::Question, "Turn needs your input")),
        ),
        (
            Phase::Failed,
            true,
            true,
            Some((NotifyKind::Error, "Turn failed")),
        ),
        (Phase::Failed, true, false, None),
        (Phase::Failed, false, true, None),
        (
            Phase::Blocked,
            true,
            true,
            Some((NotifyKind::Error, "Turn blocked")),
        ),
        (Phase::Blocked, true, false, None),
        (
            Phase::Incomplete,
            true,
            true,
            Some((NotifyKind::Error, "Turn incomplete")),
        ),
        (Phase::Incomplete, true, false, None),
        (Phase::Interrupted, true, true, None),
        (Phase::Running, true, true, None),
    ];
    for (outcome, notify, notify_errors, expected) in cases {
        let actual = turn_end_notice(outcome, notify, notify_errors)
            .map(|notice| (notice.kind, notice.body));
        assert_eq!(
            actual, expected,
            "{outcome:?} notify={notify} errors={notify_errors}"
        );
    }
}
