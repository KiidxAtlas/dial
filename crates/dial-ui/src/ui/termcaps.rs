//! What the hosting terminal understands beyond plain text, read from its environment: which
//! notification escape it turns into a desktop toast (OSC 9, OSC 99, OSC 777 or only a bell), and
//! which optional integrations it offers (hyperlinks, working directory, tab color, attention,
//! user variables, progress; see [`Integration`]).
//! Detection checks the terminal's capabilities; multiplexers (tmux, Zellij, GNU
//! screen) are accounted for.
use std::sync::OnceLock;

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

/// Optional terminal integrations Dial drives while the inline UI runs. Each flag is set only for
/// terminals known to implement the sequence, so an unknown terminal never sees one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Integration {
    /// OSC 8 hyperlinks on URLs and existing file paths in scrollback.
    pub hyperlinks: bool,
    /// `file://…#line` opens the file through the terminal's own rules for that line (iTerm2
    /// Semantic History); other terminals get no fragment.
    pub line_fragments: bool,
    /// OSC 7 working directory, so new tabs and splits start in the project.
    pub cwd: bool,
    /// iTerm2 OSC 6 tab color by state.
    pub tab_color: bool,
    /// iTerm2 `OSC 1337 ; RequestAttention` (dock bounce) while Dial waits for the user.
    pub attention: bool,
    /// `OSC 1337 ; SetUserVar` (iTerm2, WezTerm) for status bars, badges and tab titles.
    pub user_vars: bool,
    /// OSC 9;4 progress (iTerm2 3.6+, Ghostty 1.2+). Older versions show OSC 9 as a notification,
    /// so an unknown version gets no progress.
    pub progress: bool,
    /// Inside tmux: terminal-only sequences go through DCS passthrough (`allow-passthrough on`).
    pub tmux: bool,
}

impl Integration {
    /// Wrap a terminal-only sequence for the multiplexer in between, if any.
    pub fn wrap(&self, sequence: &str) -> String {
        if self.tmux {
            tmux_passthrough(sequence)
        } else {
            sequence.to_owned()
        }
    }
}

/// This process's terminal integrations, detected on first use.
pub fn integration() -> Integration {
    static INTEGRATION: OnceLock<Integration> = OnceLock::new();
    *INTEGRATION.get_or_init(|| {
        let env = |name: &str| std::env::var(name).ok();
        detect_integration(&env, current().id)
    })
}

pub fn detect_integration(env: &dyn Fn(&str) -> Option<String>, id: TerminalId) -> Integration {
    // Zellij and GNU screen own the screen and have no reliable passthrough for these.
    if set(env, "ZELLIJ") || set(env, "STY") {
        return Integration::default();
    }
    let tmux = set(env, "TMUX");
    let version = terminal_version(env, id);
    let at_least = |want: (u32, u32)| version.is_some_and(|have| have >= want);
    use TerminalId as T;
    Integration {
        // tmux rewrites cells and drops OSC 8 unless configured for it; links stay plain there.
        hyperlinks: !tmux
            && matches!(
                id,
                T::Iterm2 | T::Kitty | T::Ghostty | T::WezTerm | T::Vscode | T::Foot | T::Rio
            ),
        line_fragments: id == T::Iterm2,
        // tmux reads OSC 7 itself (the pane's path), so it is sent unwrapped there.
        cwd: tmux || matches!(id, T::Iterm2 | T::Kitty | T::Ghostty | T::WezTerm | T::Foot),
        tab_color: id == T::Iterm2,
        attention: id == T::Iterm2,
        user_vars: matches!(id, T::Iterm2 | T::WezTerm),
        progress: match id {
            T::Iterm2 => at_least((3, 6)),
            T::Ghostty => at_least((1, 2)),
            _ => false,
        },
        tmux,
    }
}

/// `(major, minor)` of the hosting terminal. Inside tmux `TERM_PROGRAM_VERSION` is tmux's own, and
/// iTerm2 reports itself in `LC_TERMINAL_VERSION` instead.
fn terminal_version(env: &dyn Fn(&str) -> Option<String>, id: TerminalId) -> Option<(u32, u32)> {
    let own_program = env("TERM_PROGRAM").is_some_and(|program| {
        let program = program.to_ascii_lowercase();
        match id {
            TerminalId::Iterm2 => program == "iterm.app" || program == "iterm2",
            TerminalId::Ghostty => program == "ghostty",
            _ => false,
        }
    });
    let raw = if own_program {
        env("TERM_PROGRAM_VERSION")
    } else if id == TerminalId::Iterm2 {
        env("LC_TERMINAL_VERSION")
    } else {
        None
    }?;
    let mut parts = raw.trim().split(['.', '-', '+']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()
        .and_then(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .unwrap_or(0);
    Some((major, minor))
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
        TerminalId::Kitty | TerminalId::Otty => NotifyProtocol::Osc99,
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
/// Serialized through the active terminal owner so it cannot split an asynchronous paint.
pub fn notify(kind: NotifyKind, title: &str, body: &str, session: &str) {
    let env = |name: &str| std::env::var(name).ok();
    if env("DIAL_NOTIFICATIONS")
        .is_some_and(|value| matches!(value.as_str(), "off" | "0" | "false"))
    {
        return;
    }
    let sequence = notification_sequence(&env, current(), kind, title, body, session);
    let _ = super::term::write_control(sequence.into_bytes());
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
mod tests;
