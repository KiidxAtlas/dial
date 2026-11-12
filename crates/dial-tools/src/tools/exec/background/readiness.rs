//! Port readiness requires both policy permission and a listening socket in the owned group.
//! lsof is an explicit OS prerequisite, never a best-effort ownership guess.
#[cfg(unix)]
use super::wake::{self, Waker};
#[cfg(unix)]
use crate::tools::exec::shell::ProcessGroup;
use serde_json::Value;
#[cfg(unix)]
use std::io::{self, Read};
#[cfg(unix)]
use std::net::TcpStream;
use std::net::{IpAddr, SocketAddr};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::atomic::Ordering;
#[cfg(unix)]
use std::time::Duration;
use std::time::Instant;

pub(super) struct Ready {
    pub(super) log: Option<regex::Regex>,
    pub(super) port: Option<SocketAddr>,
    pub(super) timeout: Option<u64>,
}
impl Ready {
    pub(super) fn parse(args: &Value) -> Result<Option<Self>, String> {
        let Some(value) = args.get("ready").filter(|value| !value.is_null()) else {
            return Ok(None);
        };
        let obj = value.as_object().ok_or("ready must be an object")?;
        if obj
            .keys()
            .any(|key| !matches!(key.as_str(), "log" | "host" | "port" | "timeout"))
        {
            return Err("ready accepts only log, host, port and timeout".into());
        }
        let log = super::opt_str(value, "log")?
            .map(|pattern| {
                if pattern.is_empty() {
                    return Err("ready.log must not be empty".into());
                }
                super::compile(pattern)
            })
            .transpose()?;
        let port = super::opt_u64(value, "port")?
            .map(|port| {
                let port = u16::try_from(port)
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or("ready.port must be in 1..=65535")?;
                let host = super::opt_str(value, "host")?.unwrap_or("127.0.0.1");
                // No unbounded DNS lookup, nor network access prior to policy validation.
                let host: IpAddr = host
                    .parse()
                    .map_err(|_| "ready.host must be a numeric IPv4 or IPv6 address")?;
                if host.is_unspecified() {
                    return Err(
                        "ready.host must be a connection address, not an unspecified bind address"
                            .into(),
                    );
                }
                Ok::<_, String>(SocketAddr::new(host, port))
            })
            .transpose()?;
        if crate::tools::present(value, "host") && port.is_none() {
            return Err("ready.host requires ready.port".into());
        }
        if log.is_none() && port.is_none() {
            return Err("ready needs ready.log (a regex the job prints) or ready.port (a TCP port the job listens on); ready.timeout alone waits for nothing. Example: \"ready\": {\"log\": \"listening on\", \"timeout\": 30}".into());
        }
        let timeout = super::opt_u64(value, "timeout")?;
        if timeout.is_some_and(|secs| secs > super::MAX_WAIT_SECS) {
            return Err("ready.timeout must be in 0..=300".into());
        }
        Ok(Some(Self { log, port, timeout }))
    }
}

/// Windows proves ownership without external tools: the IP Helper table names each listener's
/// PID, and the PID must sit inside the Job Object Dial created for this background group.
#[cfg(windows)]
pub(super) fn port_ready(
    group_pid: u32,
    address: SocketAddr,
    deadline: Option<Instant>,
    cancel: &AtomicBool,
    interrupt: &AtomicBool,
) -> Result<bool, String> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    if cancel.load(Ordering::Acquire) || interrupt.load(Ordering::Acquire) {
        return Err("cancelled readiness probe; background process remains managed by bg".into());
    }
    let listeners = crate::platform::processes::listeners()
        .map_err(|e| format!("reading the socket tables for listener ownership: {e}"))?;
    let owned = owned_listener(
        listeners
            .iter()
            .map(|l| (l.protocol, l.address, l.port, l.pid)),
        address,
        |pid| crate::platform::process::in_group(group_pid, pid),
    );
    if !owned {
        return Ok(false);
    }
    // Bounded TCP probe follows positive socket ownership evidence, as on Unix.
    let local_end = Instant::now() + Duration::from_millis(100);
    let end = deadline.map_or(local_end, |end| end.min(local_end));
    let remaining = end.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(false);
    }
    Ok(std::net::TcpStream::connect_timeout(&address, remaining).is_ok())
}

/// Whether a listening TCP socket on `address`'s port belongs to the group. A wildcard socket
/// (`0.0.0.0` or `::`, often dual-stack) counts for either family, as lsof's `*:port` does; the
/// TCP probe then decides reachability.
#[cfg(any(windows, test))]
fn owned_listener(
    listeners: impl IntoIterator<Item = (&'static str, IpAddr, u16, u32)>,
    address: SocketAddr,
    owned: impl Fn(u32) -> bool,
) -> bool {
    listeners.into_iter().any(|(protocol, ip, port, pid)| {
        let family = matches!(
            (protocol, address.ip()),
            ("tcp", IpAddr::V4(_)) | ("tcp6", IpAddr::V6(_))
        );
        let tcp = matches!(protocol, "tcp" | "tcp6");
        port == address.port()
            && tcp
            && (ip.is_unspecified() || (family && ip == address.ip()))
            && owned(pid)
    })
}
#[cfg(unix)]
pub(super) fn port_ready(
    group_pid: u32,
    address: SocketAddr,
    deadline: Option<Instant>,
    cancel: &AtomicBool,
    interrupt: &AtomicBool,
) -> Result<bool, String> {
    let program = ["/usr/sbin/lsof", "/usr/bin/lsof", "/bin/lsof"].into_iter().find(|path| Path::new(path).is_file())
        .ok_or("port readiness requires operator-installed lsof at /usr/sbin/lsof, /usr/bin/lsof or /bin/lsof; ownership cannot be verified")?;
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("LC_ALL", "C")
        .args([
            "-nP",
            "-a",
            "-g",
            &group_pid.to_string(),
            &format!("-iTCP:{}", address.port()),
            "-sTCP:LISTEN",
            "-Fptn",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("starting listener ownership query: {e}"))?;
    let mut output = child.stdout.take().ok_or("listener query has no stdout")?;
    let mut group = ProcessGroup::new(child);
    crate::platform::io::set_nonblocking(&output)
        .map_err(|e| format!("configuring listener query: {e}"))?;
    let local_end = Instant::now() + Duration::from_secs(2);
    let end = deadline.map_or(local_end, |end| end.min(local_end));
    let waker = Waker::new().map_err(|e| format!("configuring listener query: {e}"))?;
    wake::watch_exit(group.id());
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut eof = false;
    loop {
        let seen = wake::generation();
        if cancel.load(Ordering::Acquire) || interrupt.load(Ordering::Acquire) {
            return Err(
                "cancelled readiness probe; background process remains managed by bg".into(),
            );
        }
        if Instant::now() >= end {
            return Err(
                "listener ownership query deadline exceeded; readiness not established".into(),
            );
        }
        if !eof {
            match output.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    if bytes.len() + n > 64 * 1024 {
                        return Err(
                            "listener ownership query exceeded 64 KiB; readiness not established"
                                .into(),
                        );
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    waker.wait(output.as_raw_fd(), libc::POLLIN, Some(end));
                }
                Err(e) => return Err(format!("reading listener ownership query: {e}")),
            }
        }
        if eof
            && group
                .exited()
                .map_err(|e| format!("listener query status: {e}"))?
        {
            break;
        }
        if eof {
            wake::wait_past(seen, Some(end));
        }
    }
    group.terminate();
    let status = group
        .wait()
        .map_err(|e| format!("waiting for listener ownership query: {e}"))?;
    if !status.success() && status.code() != Some(1) {
        return Err(format!("listener ownership query failed: {status}"));
    }
    let text = String::from_utf8(bytes).map_err(|_| "listener query returned non-UTF8 metadata")?;
    let mut owned_pid = false;
    let mut family_matches = false;
    let mut owned_listener = false;
    let wildcard = format!("*:{}", address.port());
    let expected = address.to_string();
    for line in text.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            owned_pid = pid.parse::<libc::pid_t>().ok().is_some_and(|pid| {
                // SAFETY: getpgid only queries the parsed PID; it borrows no memory,
                // and an absent/forbidden PID returns -1 rather than a matching group.
                (unsafe { libc::getpgid(pid) }) == group_pid as libc::pid_t
            });
            family_matches = false;
        } else if let Some(kind) = line.strip_prefix('t') {
            family_matches = matches!(
                (kind, address.ip()),
                ("IPv4", IpAddr::V4(_)) | ("IPv6", IpAddr::V6(_))
            );
        } else if let Some(endpoint) = line.strip_prefix('n') {
            // A `*:port` listener is a wildcard socket (often IPv6 dual-stack, which also accepts IPv4); the TCP probe below decides reachability.
            owned_listener |=
                owned_pid && ((endpoint == wildcard) || (family_matches && endpoint == expected));
        }
    }
    if !owned_listener {
        return Ok(false);
    }
    // Bounded TCP probe follows positive socket ownership evidence. Readiness is a point-in-time
    // observation, not a promise the process will remain healthy after returning.
    let remaining = end
        .saturating_duration_since(Instant::now())
        .min(Duration::from_millis(100));
    if remaining.is_zero() {
        return Ok(false);
    }
    Ok(TcpStream::connect_timeout(&address, remaining).is_ok())
}

#[cfg(test)]
mod tests;
