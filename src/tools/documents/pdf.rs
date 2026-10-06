use super::{MAX_CONTENT, Workspace};
use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Stdio},
    time::{Duration, Instant},
};
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
pub(super) fn read(ws: &Workspace, path: &Path, selector: Option<&str>) -> Result<String, String> {
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
    let file = path.to_str().ok_or("PDF path must be UTF-8")?;
    // The executable is resolved by the scrubbed sandbox PATH; no shell interpolation of input.
    let script = format!(
        "command -v pdftotext >/dev/null 2>&1 || {{ printf '%s\\n' 'PDF parser unavailable: install Poppler (brew install poppler), providing pdftotext on PATH'; exit 127; }}; exec pdftotext -layout -enc UTF-8 {pages} {} -",
        quote(file)
    );
    let mut command = crate::tools::shell::shell_command(ws, &script, ws.root())?;
    let (mut reader, writer) = io::pipe().map_err(|e| e.to_string())?;
    let err = writer.try_clone().map_err(|e| e.to_string())?;
    command
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(err)
        .process_group(0);
    let child = command
        .spawn()
        .map_err(|e| format!("starting sandboxed PDF parser: {e}"))?;
    drop(command);
    let mut process = Process(child);
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(format!(
            "configuring PDF output pipe: {}",
            io::Error::last_os_error()
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut status = None;
    let mut eof = false;
    loop {
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
        if status.is_none() {
            status = process.0.try_wait().map_err(|e| e.to_string())?;
        }
        if eof && status.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            return Err("PDF parser timed out after 15 seconds; process group terminated".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let text = super::text(data)?;
    if !status.unwrap().success() {
        return Err(format!("PDF parser failed: {text}"));
    }
    Ok(text)
}
