//! User-initiated clipboard delivery shared by the main answer and side threads.

use anyhow::Result;
use base64::Engine;
use std::io::Write;
use std::process::{Command, Stdio};

/// Windows PowerShell's `Set-Clipboard`, reading stdin as UTF-8 (`clip.exe` would read it in the
/// console code page and garble non-ASCII text).
const WINDOWS_COPY: &str = "[Console]::InputEncoding = [Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())";

/// Clipboard writers to try in order: PowerShell on Windows, `pbcopy`/`wl-copy`/`xclip` elsewhere.
fn copy_programs(windows: bool) -> &'static [(&'static str, &'static [&'static str])] {
    if windows {
        &[(
            "powershell.exe",
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                WINDOWS_COPY,
            ],
        )]
    } else {
        &[
            ("pbcopy", &[]),
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
        ]
    }
}

pub(super) fn copy_text(text: &str) -> Result<&'static str> {
    for &(program, args) in copy_programs(cfg!(windows)) {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let wrote = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        if child.wait().is_ok_and(|status| status.success()) && wrote {
            return Ok("answer copied");
        }
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    crate::ui::term::write_control(format!("\x1b]52;c;{encoded}\x07").into_bytes())?;
    Ok("sent to terminal via OSC52; paste to confirm — your terminal may not support it")
}

#[cfg(test)]
mod tests;
