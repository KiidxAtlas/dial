//! What the hosting terminal understands beyond plain text, read from its environment: which
//! notification escape it turns into a desktop toast (OSC 9, OSC 99, OSC 777 or only a bell).
//! Detection checks the terminal's capabilities; multiplexers (tmux, Zellij, GNU
//! screen) are accounted for.
use std::{io::Write, sync::OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalId {
    Kitty,
    Ghostty,
    WezTerm,
    Iterm2,
    Vscode,
    Alacritty,
    Warp,
    Otty,
    Rio,
    Tern,
    Monstar,
    Foot,
    Urxvt,
    TrueColor,
    Base,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyProtocol {
    /// Only `BEL`: the terminal flags the tab/window (urgency hint) but shows no text.
    Bell,
    /// `ESC ] 9 ; message ST` (iTerm2, Ghostty, WezTerm, Warp, …).
    Osc9,
    /// `ESC ] 99 ; ; message ST` (kitty's desktop notification protocol).
    Osc99,
    /// `ESC ] 777 ; notify ; title ; body ST` (foot, rxvt-unicode).
    Osc777,
}

/// Why the user is being told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyKind {
    /// A turn finished normally.
    Completion,
    /// A turn stopped with an error.
    Error,
    /// A question or approval is waiting.
    Question,
}

/// Facts derived once from the environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalInfo {
    pub id: TerminalId,
    pub notify: NotifyProtocol,
}

/// The process's terminal, detected on first use.
pub fn current() -> TerminalInfo {
    static INFO: OnceLock<TerminalInfo> = OnceLock::new();
    *INFO.get_or_init(|| detect(&|name| std::env::var(name).ok()))
}

pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> TerminalInfo {
    let id = terminal_id(env);
    TerminalInfo {
        id,
        notify: notify_protocol(id),
    }
}

fn set(env: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    env(name).is_some_and(|value| !value.is_empty())
}

fn terminal_id(env: &dyn Fn(&str) -> Option<String>) -> TerminalId {
    let from_program = |program: &str| match program.to_ascii_lowercase().as_str() {
        "kitty" => Some(TerminalId::Kitty),
        "ghostty" => Some(TerminalId::Ghostty),
        "wezterm" => Some(TerminalId::WezTerm),
        "iterm.app" | "iterm2" => Some(TerminalId::Iterm2),
        "vscode" => Some(TerminalId::Vscode),
        "alacritty" => Some(TerminalId::Alacritty),
        "warpterminal" => Some(TerminalId::Warp),
        "otty" => Some(TerminalId::Otty),
        "rio" => Some(TerminalId::Rio),
        "tern" => Some(TerminalId::Tern),
        "monstar" => Some(TerminalId::Monstar),
        _ => None,
    };
    if set(env, "KITTY_WINDOW_ID") {
        return TerminalId::Kitty;
    }
    if set(env, "GHOSTTY_RESOURCES_DIR") {
        return TerminalId::Ghostty;
    }
    if set(env, "WEZTERM_PANE") {
        return TerminalId::WezTerm;
    }
    if set(env, "ITERM_SESSION_ID") {
        return TerminalId::Iterm2;
    }
    if set(env, "VSCODE_PID") {
        return TerminalId::Vscode;
    }
    if set(env, "ALACRITTY_WINDOW_ID") {
        return TerminalId::Alacritty;
    }
    if let Some(id) = env("TERM_PROGRAM").as_deref().and_then(from_program) {
        return id;
    }
    // tmux >= 3.2 replaces TERM_PROGRAM with its own; the client's terminal is in LC_TERMINAL
    // for iTerm2 and otherwise unknown without asking the tmux server.
    if let Some(id) = env("LC_TERMINAL").as_deref().and_then(from_program) {
        return id;
    }
    let term = env("TERM").unwrap_or_default().to_ascii_lowercase();
    if term.contains("ghostty") {
        return TerminalId::Ghostty;
    }
    if term == "monstar" {
        return TerminalId::Monstar;
    }
    if term.starts_with("foot") {
        return TerminalId::Foot;
    }
    if term.starts_with("rxvt-unicode") {
        return TerminalId::Urxvt;
    }
    if env("COLORTERM").is_some_and(|value| {
        value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
    }) {
        return TerminalId::TrueColor;
    }
    TerminalId::Base
}

fn notify_protocol(id: TerminalId) -> NotifyProtocol {
    match id {
        TerminalId::Kitty | TerminalId::Otty | TerminalId::Tern => NotifyProtocol::Osc99,
        TerminalId::Ghostty
        | TerminalId::WezTerm
        | TerminalId::Iterm2
        | TerminalId::Warp
        | TerminalId::Monstar => NotifyProtocol::Osc9,
        TerminalId::Foot | TerminalId::Urxvt => NotifyProtocol::Osc777,
        TerminalId::Vscode
        | TerminalId::Alacritty
        | TerminalId::Rio
        | TerminalId::TrueColor
        | TerminalId::Base => NotifyProtocol::Bell,
    }
}

/// Printable, single-line, bounded text for an OSC payload (no ESC/BEL can end it early).
fn payload(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if out.chars().count() > max {
        out = out.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
    }
    out
}

/// Wrap an escape sequence for tmux's DCS passthrough (`allow-passthrough on`).
fn tmux_passthrough(sequence: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
}

/// Warp's structured CLI-agent events, when Warp negotiated the protocol.
fn warp_event(
    env: &dyn Fn(&str) -> Option<String>,
    kind: NotifyKind,
    body: &str,
    session: &str,
) -> Option<String> {
    let version = env("WARP_CLI_AGENT_PROTOCOL_VERSION")?
        .trim()
        .parse::<u32>()
        .ok()?;
    if version < 1 {
        return None;
    }
    let cwd = std::env::current_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();
    let project = std::path::Path::new(&cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let event = match kind {
        NotifyKind::Completion => "stop",
        NotifyKind::Error => "stop_failure",
        NotifyKind::Question => "question_asked",
    };
    let mut value = serde_json::json!({
        "event": event,
        "v": 1,
        "agent": "dial",
        "session_id": session,
        "cwd": cwd,
        "project": project,
        "plugin_version": env!("CARGO_PKG_VERSION"),
    });
    if kind == NotifyKind::Error {
        value["error_type"] = "error".into();
        value["response"] = payload(body, 400).into();
    }
    Some(format!("\x1b]777;notify;warp://cli-agent;{value}\x07"))
}

/// The bytes that raise `title`/`body` on this terminal, multiplexer wrapping included.
pub fn notification_sequence(
    env: &dyn Fn(&str) -> Option<String>,
    info: TerminalInfo,
    kind: NotifyKind,
    title: &str,
    body: &str,
    session: &str,
) -> String {
    let inside_tmux = set(env, "TMUX");
    if let Some(event) = warp_event(env, kind, body, session) {
        if !inside_tmux {
            return event;
        }
        let ring = if kind == NotifyKind::Completion {
            ""
        } else {
            "\x07"
        };
        return format!("{}{ring}", tmux_passthrough(&event));
    }
    let title = payload(title, 80);
    let body = payload(body, 200);
    let line = match (title.is_empty(), body.is_empty()) {
        (false, false) => format!("{title}: {body}"),
        (false, true) => title.clone(),
        _ => body.clone(),
    };
    let sequence = match info.notify {
        NotifyProtocol::Bell => return "\x07".into(),
        NotifyProtocol::Osc9 => format!("\x1b]9;{line}\x1b\\"),
        NotifyProtocol::Osc99 => format!("\x1b]99;;{line}\x1b\\"),
        // `;` separates the title from the body in OSC 777.
        NotifyProtocol::Osc777 => format!(
            "\x1b]777;notify;{};{}\x1b\\",
            title.replace(';', ","),
            body.replace(';', ",")
        ),
    };
    // tmux drops bare OSC 9/99/777 and Zellij swallows them: pass through where possible and
    // always follow with a BEL so a backgrounded pane is still flagged.
    if inside_tmux {
        format!("{}\x07", tmux_passthrough(&sequence))
    } else if set(env, "ZELLIJ") {
        format!("{sequence}\x07")
    } else {
        sequence
    }
}

/// Raise a notification on the controlling terminal unless `DIAL_NOTIFICATIONS=off|0|false`.
/// Written between frames, so it never splits a paint.
pub fn notify(kind: NotifyKind, title: &str, body: &str, session: &str) {
    let env = |name: &str| std::env::var(name).ok();
    if env("DIAL_NOTIFICATIONS")
        .is_some_and(|value| matches!(value.as_str(), "off" | "0" | "false"))
    {
        return;
    }
    let sequence = notification_sequence(&env, current(), kind, title, body, session);
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(sequence.as_bytes());
    let _ = stdout.flush();
}

/// The notification for a finished turn, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnEndNotice {
    pub kind: NotifyKind,
    pub body: &'static str,
}

/// Notification policy for a finished turn, keyed by the agent's authoritative outcome (the
/// transcript's status) rather than by whether the request returned `Ok`.
///
/// - `Completed` → completion, subject to `notify`.
/// - `NeedsInput` → question, subject to `notify`: the agent is waiting for the user.
/// - `Failed`, `Blocked`, `Incomplete` → error, subject to `notify` and `notify_errors`. Blocked
///   and incomplete work did not finish, so they count as failures for this policy.
/// - `Interrupted` and `Running` → silent; the user stopped the turn or it has not ended.
pub fn turn_end_notice(
    outcome: crate::runtime::Phase,
    notify: bool,
    notify_errors: bool,
) -> Option<TurnEndNotice> {
    use crate::runtime::Phase;
    let (kind, body, failure) = match outcome {
        Phase::Completed => (NotifyKind::Completion, "Turn finished", false),
        Phase::NeedsInput => (NotifyKind::Question, "Turn needs your input", false),
        Phase::Failed => (NotifyKind::Error, "Turn failed", true),
        Phase::Blocked => (NotifyKind::Error, "Turn blocked", true),
        Phase::Incomplete => (NotifyKind::Error, "Turn incomplete", true),
        Phase::Running | Phase::Interrupted => return None,
    };
    (notify && (!failure || notify_errors)).then_some(TurnEndNotice { kind, body })
}

#[cfg(test)]
mod tests {
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
}
