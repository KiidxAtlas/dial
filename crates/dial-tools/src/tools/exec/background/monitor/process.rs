//! Liveness and identity of an existing process for `pid` watches. Reads only; never signals.

/// The command name and start time of `pid`, owned by this user. The start time distinguishes the
/// process from a later one that reuses its PID.
#[cfg(unix)]
pub(super) fn identity(pid: u32) -> Result<(String, String), String> {
    // Signal 0 only checks existence and permission; nothing is delivered.
    // SAFETY: kill with signal 0 has no side effects.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return Err(match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) => {
                format!(
                    "process {pid} belongs to another user; only your own processes can be watched"
                )
            }
            _ => format!("no process {pid}"),
        });
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-o", "comm=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("reading process {pid}: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().map(str::trim).unwrap_or("");
    if !output.status.success() || line.is_empty() {
        return Err(format!("no process {pid}"));
    }
    // `lstart` is five words, for example `Thu Nov 12 06:55:37 2026`; the command follows.
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.len() < 6 {
        return Err(format!("could not read the start time of process {pid}"));
    }
    let started = words[..5].join(" ");
    let command = words[5..].join(" ");
    let command = std::path::Path::new(&command)
        .file_name()
        .map_or(command.clone(), |name| name.to_string_lossy().into_owned());
    Ok((command, started))
}

#[cfg(unix)]
pub(super) fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 has no side effects.
    let delivered = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    delivered || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub(super) fn identity(_pid: u32) -> Result<(String, String), String> {
    Err("pid watches are supported on macOS and Linux only".into())
}

#[cfg(not(unix))]
pub(super) fn alive(_pid: u32) -> bool {
    false
}
