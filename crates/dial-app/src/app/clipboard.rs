//! User-initiated clipboard delivery shared by the main answer and side threads.

use anyhow::Result;
use base64::Engine;
use std::io::Write;
use std::process::{Command, Stdio};

pub(super) fn copy_text(text: &str) -> Result<&'static str> {
    for (program, args) in [
        ("pbcopy", &[][..]),
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
    ] {
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
    Ok("clipboard sent via OSC52 (terminal support required)")
}
