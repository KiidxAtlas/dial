//! Best-effort iTerm2 launch. The command and session ID are data, never AppleScript source.
use anyhow::{Context, Result, bail};
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

pub(super) fn target() -> Result<Option<String>> {
    // Tests must never open a real pane, even if launched from iTerm2.
    eligible_target(cfg!(target_os = "macos") && !cfg!(test), |key| {
        std::env::var(key).ok()
    })
}

fn eligible_target(macos: bool, env: impl Fn(&str) -> Option<String>) -> Result<Option<String>> {
    if !macos
        || env("TERM_PROGRAM").as_deref() != Some("iTerm.app")
        || ["TMUX", "STY", "ZELLIJ", "ZELLIJ_SESSION_NAME"]
            .iter()
            .any(|key| env(key).is_some())
    {
        return Ok(None);
    }
    let raw = env("ITERM_SESSION_ID")
        .context("iTerm2 did not provide ITERM_SESSION_ID; use the printed command")?;
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
    Ok(Some(id.to_owned()))
}

fn script_args<'a>(session: &'a str, command: &'a str) -> [&'a str; 5] {
    ["-e", SCRIPT, "--", session, command]
}

pub(super) fn launch(session: &str, command: &str, cancel: &AtomicBool) -> Result<()> {
    // Defense in depth: no test may invoke Automation through this helper.
    if cfg!(test) || !cfg!(target_os = "macos") {
        bail!("automatic launch is unavailable in this build");
    }
    let mut child = Command::new("/usr/bin/osascript")
        .args(script_args(session, command))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting iTerm2 Automation")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("iTerm2 Automation was cancelled or exceeded its five-second deadline");
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => bail!(
                "iTerm2 Automation exited with {status} (permission denied or originating session unavailable)"
            ),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("waiting for iTerm2 Automation");
            }
        }
    }
}

#[cfg(test)]
mod tests;
