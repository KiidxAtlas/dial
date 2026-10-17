use super::{MAX_CONTENT, Workspace};
use crate::tools::exec::shell::ProcessGroup;
#[cfg(windows)]
use std::process::Command;
use std::{
    fs::File,
    io::{self, Read},
    process::Stdio,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub(super) fn read(
    ws: &Workspace,
    input: &File,
    selector: Option<&str>,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let pages = if let Some(s) = selector {
        let s = s
            .strip_prefix("page=")
            .ok_or("PDF selector must be :page=N or :page=A-B")?;
        let (a, b) = s.split_once('-').unwrap_or((s, s));
        let a: u32 = a.parse().map_err(|_| "invalid PDF page number")?;
        let b: u32 = b.parse().map_err(|_| "invalid PDF page number")?;
        if a == 0 || b < a || b > 10000 || b - a > 200 {
            return Err("PDF pages must be 1..10000 with at most 201 pages per selection".into());
        }
        format!("-f {a} -l {b}")
    } else {
        String::new()
    };
    if cancel.load(Ordering::Acquire) {
        return Err("PDF parser cancelled before startup".into());
    }
    // Feed the converter the already-authorized descriptor, never reopen a replaced path.
    #[cfg(not(windows))]
    let mut command = {
        let script = format!(
            "command -v pdftotext >/dev/null 2>&1 || {{ printf '%s\\n' 'PDF parser unavailable: install Poppler, providing pdftotext on PATH'; exit 127; }}; exec pdftotext -layout -enc UTF-8 {pages} - -"
        );
        crate::tools::exec::shell::shell_command(ws, &script, ws.root())?
    };
    #[cfg(windows)]
    let mut command = {
        if !ws.unsandboxed {
            return Err("PDF parsing on Windows requires an OS sandbox; set sandbox.mode = \"off\" only if you explicitly accept running the PDF converter unsandboxed".into());
        }
        let mut command = Command::new("pdftotext");
        command
            .args(["-layout", "-enc", "UTF-8"])
            .args(pages.split_whitespace())
            .args(["-", "-"]);
        command
    };
    let (mut reader, writer) = io::pipe().map_err(|e| e.to_string())?;
    command
        .stdin(Stdio::from(input.try_clone().map_err(|e| e.to_string())?))
        .stdout(writer)
        .stderr(Stdio::null());
    let child = crate::platform::process::spawn_group(&mut command)
        .map_err(|e| format!("starting PDF parser: {e}"))?;
    drop(command);
    let mut process = ProcessGroup::new(child);
    crate::platform::io::set_nonblocking(&reader)
        .map_err(|e| format!("configuring PDF output pipe: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut eof = false;
    let waiter = crate::platform::wake::Waiter::new()
        .map_err(|e| format!("creating PDF wait channel: {e}"))?;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("PDF parser cancelled; process group terminated".into());
        }
        if Instant::now() >= deadline {
            return Err("PDF parser timed out after 15 seconds; process group terminated".into());
        }
        match reader.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(n) => {
                if data.len() + n > MAX_CONTENT as usize {
                    return Err(
                        "PDF extracted text exceeds 8 MiB; select :page=N or :page=A-B".into(),
                    );
                }
                data.extend_from_slice(&buffer[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(format!("reading PDF parser output: {e}")),
        }
        // Observe without reaping: managed cleanup must not kill a recycled PID.
        if eof && process.exited().map_err(|e| e.to_string())? {
            break;
        }
        let pipe = (!eof).then_some(&reader);
        waiter
            .wait(
                pipe,
                Some(&process.child),
                Some(deadline.saturating_duration_since(Instant::now())),
            )
            .map_err(|e| format!("waiting for PDF parser: {e}"))?;
    }
    process.terminate();
    let status = process.child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!(
            "PDF parser failed (exit status {:?}); no text extracted",
            status.code()
        ));
    }
    super::text(data)
}
