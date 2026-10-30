//! Best-effort side-pane launch for `/watch`. The watcher command is always data: separate argv
//! entries for tmux, Zellij, WezTerm and kitty, and one script argument (never AppleScript source)
//! for iTerm2. No launcher goes through a shell string.
//!
//! Detection checks the innermost layer first. A multiplexer inherits the outer terminal's
//! variables (`ITERM_SESSION_ID`, `WEZTERM_PANE`, `KITTY_WINDOW_ID`), and those can be stale after
//! a detach and reattach, so splitting the outer terminal would open the pane outside the
//! multiplexer, possibly in another window. tmux and Zellij are therefore asked to split
//! themselves. A direct iTerm2 session (`TERM_PROGRAM=iTerm.app` on macOS) is checked before
//! WezTerm and kitty so a stray inherited variable cannot change its behaviour. Two cases are
//! refused, with a reason:
//! - GNU screen: splitting needs several `screen -X` commands against whatever region the attached
//!   display has focused, which cannot be pinned to Dial's window.
//! - kitty without `KITTY_LISTEN_ON`: remote control would then travel as escape sequences over
//!   the terminal Dial is drawing, competing with its input. Dial only uses kitty's socket.
//!
//! Every failure, refusal or missing terminal ends in the exact `dial … watch SESSION` command.
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SCRIPT: &str = r#"on run argv
    set wantedID to item 1 of argv
    set watchCommand to item 2 of argv
    tell application "iTerm2"
        repeat with aWindow in windows
            repeat with aTab in tabs of aWindow
                repeat with aSession in sessions of aTab
                    if (unique ID of aSession) is wantedID then
                        tell aSession to split vertically with default profile command watchCommand
                        return
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    error "The originating iTerm2 session was not found; no pane was opened."
end run"#;

/// Terminals that `/watch` can split, for the message shown when none is detected.
pub(super) const SUPPORTED: &str = "tmux, Zellij, iTerm2, WezTerm or kitty with remote control";

/// The terminal or multiplexer that will open the side pane, with its validated target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Launcher {
    /// `TMUX_PANE` (`%N`) when exported; tmux otherwise splits the client's current pane.
    Tmux {
        pane: Option<String>,
    },
    Zellij,
    /// `WEZTERM_PANE`, a numeric pane ID.
    WezTerm {
        pane: String,
    },
    /// `KITTY_LISTEN_ON` and the numeric `KITTY_WINDOW_ID`.
    Kitty {
        socket: String,
        window: String,
    },
    /// The UUID part of `ITERM_SESSION_ID`.
    ITerm2 {
        session: String,
    },
}

impl Launcher {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Tmux { .. } => "tmux",
            Self::Zellij => "Zellij",
            Self::WezTerm { .. } => "WezTerm",
            Self::Kitty { .. } => "kitty",
            Self::ITerm2 { .. } => "iTerm2",
        }
    }

    fn failure_hint(&self) -> &'static str {
        match self {
            Self::ITerm2 { .. } => "Check macOS Automation permission for your terminal. ",
            Self::Kitty { .. } => {
                "kitty needs `allow_remote_control socket-only` (or `yes`) and `listen_on` in kitty.conf. "
            }
            Self::Tmux { .. } => "Splitting with a command needs tmux 3.0 or newer. ",
            Self::Zellij | Self::WezTerm { .. } => "",
        }
    }
}

pub(super) fn target() -> Result<Option<Launcher>> {
    // Tests must never open a real pane, whatever terminal runs them.
    if cfg!(test) {
        return Ok(None);
    }
    detect(cfg!(target_os = "macos"), |key| std::env::var(key).ok())
}

/// Chooses the launcher from the environment alone. Empty variables count as unset.
fn detect(macos: bool, env: impl Fn(&str) -> Option<String>) -> Result<Option<Launcher>> {
    let var = |key: &str| env(key).filter(|value| !value.is_empty());
    if var("TMUX").is_some() {
        let pane = var("TMUX_PANE")
            .map(|pane| {
                if pane.strip_prefix('%').is_some_and(numeric) {
                    Ok(pane)
                } else {
                    bail!("unrecognized TMUX_PANE; use the printed command")
                }
            })
            .transpose()?;
        return Ok(Some(Launcher::Tmux { pane }));
    }
    if var("ZELLIJ").is_some() || var("ZELLIJ_SESSION_NAME").is_some() {
        return Ok(Some(Launcher::Zellij));
    }
    if var("STY").is_some() {
        bail!("GNU screen cannot split a region pinned to Dial's window; use the printed command");
    }
    if macos && var("TERM_PROGRAM").as_deref() == Some("iTerm.app") {
        return iterm2(var("ITERM_SESSION_ID")).map(Some);
    }
    if let Some(pane) = var("WEZTERM_PANE") {
        if !numeric(&pane) {
            bail!("unrecognized WEZTERM_PANE; use the printed command");
        }
        return Ok(Some(Launcher::WezTerm { pane }));
    }
    if let Some(window) = var("KITTY_WINDOW_ID") {
        if !numeric(&window) {
            bail!("unrecognized KITTY_WINDOW_ID; use the printed command");
        }
        let socket = var("KITTY_LISTEN_ON").context(
            "kitty remote control is not listening on a socket (set `allow_remote_control socket-only` and `listen_on unix:/tmp/kitty` in kitty.conf); Dial will not send remote-control escapes through the terminal it is drawing",
        )?;
        return Ok(Some(Launcher::Kitty { socket, window }));
    }
    Ok(None)
}

fn iterm2(raw: Option<String>) -> Result<Launcher> {
    let raw = raw.context("iTerm2 did not provide ITERM_SESSION_ID; use the printed command")?;
    // iTerm2 exports either UUID or wXtYpZ:UUID. Its scripting API exposes only UUID.
    let id = raw.rsplit(':').next().unwrap_or_default();
    if id.len() != 36
        || !id.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        bail!("unrecognized ITERM_SESSION_ID; use the printed command");
    }
    Ok(Launcher::ITerm2 {
        session: id.to_owned(),
    })
}

fn numeric(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn script_args<'a>(session: &'a str, command: &'a str) -> [&'a str; 5] {
    ["-e", SCRIPT, "--", session, command]
}

/// The program and arguments that open a pane running `argv`. `argv` stays separate entries
/// after `--`; only iTerm2, whose API takes one command line, gets the shell-quoted join.
fn invocation(launcher: &Launcher, argv: &[String]) -> Result<(&'static str, Vec<String>)> {
    let owned = |words: &[&str]| {
        words
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    };
    let (program, mut args) = match launcher {
        Launcher::ITerm2 { session } => {
            let command = shlex::try_join(argv.iter().map(String::as_str))
                .context("quoting the watch command for iTerm2")?;
            return Ok(("/usr/bin/osascript", owned(&script_args(session, &command))));
        }
        Launcher::Tmux { pane } => {
            // -d keeps focus on Dial's pane, as the composer stays the place to type.
            let mut args = owned(&["split-window", "-h", "-d"]);
            if let Some(pane) = pane {
                args.extend(["-t".to_owned(), pane.clone()]);
            }
            ("tmux", args)
        }
        Launcher::Zellij => ("zellij", owned(&["run", "--direction", "right"])),
        Launcher::WezTerm { pane } => (
            "wezterm",
            owned(&["cli", "split-pane", "--right", "--pane-id", pane]),
        ),
        Launcher::Kitty { socket, window } => (
            "kitty",
            vec![
                "@".to_owned(),
                format!("--to={socket}"),
                "launch".to_owned(),
                "--location=vsplit".to_owned(),
                "--keep-focus".to_owned(),
                format!("--next-to=id:{window}"),
            ],
        ),
    };
    args.push("--".to_owned());
    args.extend(argv.iter().cloned());
    Ok((program, args))
}

pub(super) fn launch(launcher: &Launcher, argv: &[String], cancel: &AtomicBool) -> Result<()> {
    // Defense in depth: no test may open a pane or invoke Automation through this helper.
    if cfg!(test) {
        bail!("automatic launch is unavailable in this build");
    }
    if matches!(launcher, Launcher::ITerm2 { .. }) && !cfg!(target_os = "macos") {
        bail!("iTerm2 Automation is unavailable in this build");
    }
    let name = launcher.name();
    let (program, args) = invocation(launcher, argv)?;
    // None of these talk to the terminal Dial owns: tmux, Zellij, WezTerm and kitty use their
    // sockets and iTerm2 uses Apple Events, so stdin and stdout stay detached.
    let mut child = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting `{program}` to split {name}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{name} was cancelled or exceeded its five-second deadline");
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.by_ref().take(4096).read_to_string(&mut stderr);
                }
                let reason = match launcher {
                    // osascript errors can quote the script; keep the old, fixed explanation.
                    Launcher::ITerm2 { .. } => {
                        "permission denied or originating session unavailable".to_owned()
                    }
                    _ => first_line(&stderr),
                };
                if reason.is_empty() {
                    bail!("`{program}` exited with {status}");
                }
                bail!("`{program}` exited with {status}: {reason}");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).with_context(|| format!("waiting for `{program}`"));
            }
        }
    }
}

/// The first non-empty stderr line, bounded so a noisy tool cannot flood the transcript.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    line.chars().take(200).collect()
}

/// Shown before the split is attempted, so the command survives a busy job slot.
pub(super) fn opening_message(launcher: &Launcher, command: &str) -> String {
    format!(
        "Opening the live feed in a {} split. If no pane appears, run this in another terminal: {command}",
        launcher.name()
    )
}

pub(super) fn opened_message(launcher: &Launcher) -> String {
    format!("Opened the live feed in a {} split.", launcher.name())
}

pub(super) fn failed_message(launcher: &Launcher, error: &anyhow::Error, command: &str) -> String {
    format!(
        "Could not open the {} split: {error:#}. {}Run this in another terminal: {command}",
        launcher.name(),
        launcher.failure_hint()
    )
}

/// `reason` is `None` when no supported terminal was detected at all.
pub(super) fn unavailable_message(reason: Option<&anyhow::Error>, command: &str) -> String {
    match reason {
        Some(error) => format!(
            "Automatic split unavailable: {error:#}. Run this in another terminal: {command}"
        ),
        None => format!(
            "No terminal with automatic splits detected ({SUPPORTED}). Run this in another terminal: {command}"
        ),
    }
}

#[cfg(test)]
mod tests;
