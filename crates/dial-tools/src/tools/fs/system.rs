//! `sys://` read targets: live, read-only host state for diagnosing services and environments.
//!
//! - `sys://` lists the views.
//! - `sys://procs` running processes; `sys://procs/<pid>` one process with its working directory,
//!   executable, children, listening sockets and open files.
//! - `sys://ports` listening TCP and bound UDP sockets with their owning process when visible.
//! - `sys://host` the environment inventory ([`inventory`]): OS, CPU, memory, disk and toolchains.
//!
//! Linux parses `/proc` directly, so minimal containers without `ps`, `ss` or `netstat` still
//! answer. macOS asks `ps` and `lsof`, which show only this user's processes unless run as root.
//! Windows reads a Toolhelp snapshot, each process's parameters block and the IP Helper socket
//! tables ([`win`]); open files are not listed there. Command lines are credential-redacted;
//! environment variable values are never read.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MAX_PROCS: usize = 200;
const MAX_FILES: usize = 30;
const COMMAND_CHARS: usize = 240;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Read one `sys://` target.
pub(super) fn read(input: &str, cancel: &AtomicBool) -> Result<String, String> {
    let rest = input
        .strip_prefix("sys://")
        .unwrap_or(input)
        .trim_end_matches('/');
    match rest {
        "" => Ok(INDEX.into()),
        "procs" => procs(cancel),
        "ports" => ports(cancel),
        "host" => Ok(inventory(None).render()),
        _ => match rest.strip_prefix("procs/").map(str::parse::<u32>) {
            Some(Ok(pid)) => process(pid, cancel),
            _ => Err(format!("unknown sys:// view `{input}`.\n{INDEX}")),
        },
    }
}

const INDEX: &str = "sys:// views (live, read-only host state):\n\
sys://procs        running processes (pid, parent, state, memory, age, command)\n\
sys://procs/<pid>  one process: cwd, executable, children, listening sockets, open files\n\
sys://ports        listening TCP and bound UDP sockets with their owning process\n\
sys://host         OS, CPU, memory, disk, container and installed toolchains";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Proc {
    pid: u32,
    ppid: u32,
    state: String,
    rss_bytes: Option<u64>,
    elapsed_secs: Option<u64>,
    command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Socket {
    protocol: &'static str,
    address: String,
    port: u16,
    pid: Option<u32>,
    program: Option<String>,
}

fn redacted(command: &str) -> String {
    let text = crate::storage::redact::redact_text(command);
    if text.chars().count() > COMMAND_CHARS {
        let cut: String = text.chars().take(COMMAND_CHARS).collect();
        format!("{cut}…")
    } else {
        text.into_owned()
    }
}

fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["K", "M", "G", "T"];
    let mut size = value as f64 / 1024.0;
    for unit in UNITS {
        if size < 1024.0 || unit == "T" {
            return if size < 10.0 {
                format!("{size:.1}{unit}")
            } else {
                format!("{size:.0}{unit}")
            };
        }
        size /= 1024.0;
    }
    unreachable!("the last unit always returns")
}

fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        3600..86400 => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d{:02}h", secs / 86400, secs % 86400 / 3600),
    }
}

fn render_procs(mut list: Vec<Proc>, source: &str, hidden: usize) -> String {
    // Newest first: processes started for the task matter more than long-lived system daemons.
    list.sort_by_key(|p| (p.elapsed_secs.unwrap_or(u64::MAX), p.pid));
    let total = list.len();
    let own = std::process::id();
    let mut out = format!(
        "sys://procs: {total} process{}, newest first ({source}; command lines credential-redacted)\n",
        if total == 1 { "" } else { "es" }
    );
    if hidden > 0 {
        out.push_str(&format!("[{hidden} kernel threads omitted]\n"));
    }
    out.push_str("PID      PPID     S  RSS     AGE      COMMAND\n");
    for p in list.iter().take(MAX_PROCS) {
        out.push_str(&format!(
            "{:<8} {:<8} {:<2} {:<7} {:<8} {}{}\n",
            p.pid,
            p.ppid,
            p.state,
            p.rss_bytes.map_or("-".into(), bytes),
            p.elapsed_secs.map_or("-".into(), duration),
            redacted(&p.command),
            if p.pid == own { "  [dial]" } else { "" }
        ));
    }
    if total > MAX_PROCS {
        out.push_str(&format!(
            "[{} older processes omitted; read sys://procs/<pid> for one]\n",
            total - MAX_PROCS
        ));
    }
    out.trim_end().into()
}

fn render_ports(mut list: Vec<Socket>, source: &str) -> String {
    list.sort();
    list.dedup();
    if list.is_empty() {
        return format!("sys://ports: no listening sockets visible ({source})");
    }
    let mut out = format!(
        "sys://ports: {} listening socket(s) ({source})\n",
        list.len()
    );
    out.push_str("PROTO  ADDRESS                                  PORT   PID      PROGRAM\n");
    for s in &list {
        out.push_str(&format!(
            "{:<6} {:<40} {:<6} {:<8} {}\n",
            s.protocol,
            s.address,
            s.port,
            s.pid.map_or("?".into(), |pid| pid.to_string()),
            s.program.as_deref().map_or("?".into(), redacted)
        ));
    }
    out.trim_end().into()
}

/// Run a fixed, model-independent probe with a deadline. Output is read on a thread so a
/// large listing cannot fill the pipe and stall the child.
fn probe(program: &str, args: &[&str], cancel: &AtomicBool) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = dial_core::platform::process::spawn(&mut command)
        .map_err(|error| format!("{program} is unavailable: {error}"))?;
    let mut pipe = child.stdout.take().ok_or("probe stdout unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = pipe.read_to_end(&mut text);
        text
    });
    let started = std::time::Instant::now();
    loop {
        let exited = dial_core::platform::process::wait_exited(&child, Duration::from_millis(100))
            .map_err(|error| error.to_string())?;
        if exited {
            break;
        }
        if cancel.load(Ordering::Relaxed) || started.elapsed() > PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program} did not finish within {PROBE_TIMEOUT:?}"));
        }
    }
    let _ = child.wait();
    let text = reader.join().map_err(|_| "probe reader panicked")?;
    Ok(String::from_utf8_lossy(&text).into_owned())
}

// ---------- Linux: /proc ----------

/// Fields of `/proc/<pid>/stat` after the parenthesized command name, which may itself contain
/// spaces and parentheses. Returns (comm, state, ppid, starttime ticks, rss pages).
fn parse_stat(stat: &str) -> Option<(String, String, u32, u64, u64)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_owned();
    let fields: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    Some((
        comm,
        fields.first()?.to_string(),
        fields.get(1)?.parse().ok()?,
        fields.get(19)?.parse().ok()?,
        fields.get(21)?.parse().ok()?,
    ))
}

/// Decode a `/proc/net/{tcp,udp}[6]` address `HEX_IP:HEX_PORT`. The kernel prints each 32-bit
/// word of the IP in host (little-endian) byte order; the port is plain hex.
fn parse_net_address(field: &str) -> Option<(String, u16)> {
    let (ip, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    if ip.len() % 8 != 0 {
        return None;
    }
    let mut octets = Vec::with_capacity(ip.len() / 2);
    for i in 0..ip.len() / 8 {
        let word = u32::from_str_radix(ip.get(i * 8..i * 8 + 8)?, 16).ok()?;
        octets.extend(word.to_le_bytes());
    }
    let address = match octets.len() {
        4 => std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]).to_string(),
        16 => std::net::Ipv6Addr::from(<[u8; 16]>::try_from(octets).ok()?).to_string(),
        _ => return None,
    };
    Some((address, port))
}

/// Listening rows of one `/proc/net` table: TCP state `0A` (LISTEN) or UDP state `07` (bound).
fn parse_net_table(protocol: &'static str, table: &str) -> Vec<(Socket, u64)> {
    let listening = if protocol.starts_with("tcp") {
        "0A"
    } else {
        "07"
    };
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(3) != Some(&listening) {
                return None;
            }
            let (address, port) = parse_net_address(fields.get(1)?)?;
            let inode = fields.get(9)?.parse().ok()?;
            Some((
                Socket {
                    protocol,
                    address,
                    port,
                    pid: None,
                    program: None,
                },
                inode,
            ))
        })
        .collect()
}

fn proc_root() -> &'static Path {
    Path::new("/proc")
}

fn linux_pids() -> Vec<u32> {
    std::fs::read_dir(proc_root())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn clock_ticks() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no preconditions and only reads a configuration value.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks > 0 {
            return ticks as u64;
        }
    }
    100
}

fn page_size() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no preconditions and only reads a configuration value.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if size > 0 {
            return size as u64;
        }
    }
    4096
}

fn linux_uptime() -> Option<f64> {
    std::fs::read_to_string(proc_root().join("uptime"))
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn linux_proc(pid: u32, uptime: Option<f64>) -> Option<(Proc, bool)> {
    let dir = proc_root().join(pid.to_string());
    let (comm, state, ppid, start, rss) =
        parse_stat(&std::fs::read_to_string(dir.join("stat")).ok()?)?;
    let raw = std::fs::read(dir.join("cmdline")).unwrap_or_default();
    let kernel = raw.is_empty() && (ppid == 2 || pid == 2);
    let command = if raw.is_empty() {
        format!("[{comm}]")
    } else {
        raw.split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let elapsed = uptime.map(|up| (up - start as f64 / clock_ticks() as f64).max(0.0) as u64);
    Some((
        Proc {
            pid,
            ppid,
            state,
            rss_bytes: Some(rss * page_size()),
            elapsed_secs: elapsed,
            command,
        },
        kernel,
    ))
}

fn linux_procs() -> (Vec<Proc>, usize) {
    let uptime = linux_uptime();
    let mut hidden = 0;
    let list = linux_pids()
        .into_iter()
        .filter_map(|pid| linux_proc(pid, uptime))
        .filter_map(|(proc, kernel)| {
            hidden += usize::from(kernel);
            (!kernel).then_some(proc)
        })
        .collect();
    (list, hidden)
}

/// Socket inode → owning pid, from the `socket:[N]` links under each visible `/proc/<pid>/fd`.
fn linux_socket_owners() -> BTreeMap<u64, u32> {
    let mut owners = BTreeMap::new();
    for pid in linux_pids() {
        let Ok(fds) = std::fs::read_dir(proc_root().join(pid.to_string()).join("fd")) else {
            continue;
        };
        for fd in fds.filter_map(Result::ok) {
            if let Some(inode) = std::fs::read_link(fd.path())
                .ok()
                .and_then(|target| socket_inode(&target.to_string_lossy()))
            {
                owners.entry(inode).or_insert(pid);
            }
        }
    }
    owners
}

fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

fn linux_sockets() -> Vec<(Socket, u64)> {
    ["tcp", "tcp6", "udp", "udp6"]
        .into_iter()
        .flat_map(|protocol| {
            std::fs::read_to_string(proc_root().join("net").join(protocol))
                .map(|table| parse_net_table(protocol, &table))
                .unwrap_or_default()
        })
        .collect()
}

fn linux_ports() -> Vec<Socket> {
    let owners = linux_socket_owners();
    let uptime = linux_uptime();
    linux_sockets()
        .into_iter()
        .map(|(mut socket, inode)| {
            socket.pid = owners.get(&inode).copied();
            socket.program = socket
                .pid
                .and_then(|pid| linux_proc(pid, uptime))
                .map(|(proc, _)| proc.command);
            socket
        })
        .collect()
}

fn linux_process(pid: u32) -> Result<String, String> {
    let uptime = linux_uptime();
    let (proc, _) = linux_proc(pid, uptime).ok_or(format!("no visible process {pid}"))?;
    let dir = proc_root().join(pid.to_string());
    let link = |name: &str| {
        std::fs::read_link(dir.join(name))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "(not visible)".into())
    };
    let status = std::fs::read_to_string(dir.join("status")).unwrap_or_default();
    let threads = status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .map_or("?".into(), |value| value.trim().to_owned());
    let children: Vec<Proc> = linux_procs()
        .0
        .into_iter()
        .filter(|child| child.ppid == pid)
        .collect();
    let mut sockets = BTreeMap::new();
    let mut files = Vec::new();
    let mut fd_count = 0;
    if let Ok(fds) = std::fs::read_dir(dir.join("fd")) {
        for fd in fds.filter_map(Result::ok) {
            fd_count += 1;
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let target = target.to_string_lossy().into_owned();
            if let Some(inode) = socket_inode(&target) {
                sockets.insert(inode, ());
            } else if target.starts_with('/') && !target.starts_with("/dev/") {
                files.push(target);
            }
        }
    }
    files.sort();
    files.dedup();
    let listening: Vec<Socket> = linux_sockets()
        .into_iter()
        .filter(|(_, inode)| sockets.contains_key(inode))
        .map(|(socket, _)| socket)
        .collect();
    Ok(render_process(
        &proc,
        &[
            ("cwd", link("cwd")),
            ("executable", link("exe")),
            ("threads", threads),
            ("open descriptors", fd_count.to_string()),
        ],
        &children,
        &listening,
        Some(&files),
    ))
}

// ---------- macOS: ps and lsof ----------

/// `ps -axo pid=,ppid=,stat=,rss=,etime=,command=` rows. RSS is KiB; etime is `[[dd-]hh:]mm:ss`.
fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let state = parts.next()?.chars().next()?.to_string();
            let rss = parts.next()?.parse::<u64>().ok().map(|kib| kib * 1024);
            let elapsed = parse_etime(parts.next()?);
            let command = parts.collect::<Vec<_>>().join(" ");
            Some(Proc {
                pid,
                ppid,
                state,
                rss_bytes: rss,
                elapsed_secs: elapsed,
                command,
            })
        })
        .collect()
}

fn parse_etime(text: &str) -> Option<u64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let parts = clock
        .split(':')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let secs = match parts.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(days * 86400 + secs)
}

/// `lsof -F pcPn` records: `p<pid>`, `c<command>`, then per descriptor `P<protocol>` and
/// `n<name>` (`host:port`, `*:port` or `[v6]:port`).
fn parse_lsof_sockets(text: &str) -> Vec<Socket> {
    let mut out = Vec::new();
    let (mut pid, mut program, mut protocol) = (None, None, None);
    for line in text.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => {
                pid = value.parse().ok();
                program = None;
            }
            "c" => program = Some(value.to_owned()),
            "P" => protocol = Some(value.to_ascii_lowercase()),
            "n" => {
                let Some((address, port)) = value.rsplit_once(':') else {
                    continue;
                };
                let Ok(port) = port.parse() else {
                    continue;
                };
                // Connected UDP sockets name a peer (`a->b`); only bound endpoints listen.
                if address.contains("->") {
                    continue;
                }
                let address = address.trim_start_matches('[').trim_end_matches(']');
                let v6 = address.contains(':');
                let protocol = match (protocol.as_deref(), v6) {
                    (Some("tcp"), false) => "tcp",
                    (Some("tcp"), true) => "tcp6",
                    (Some("udp"), false) => "udp",
                    (Some("udp"), true) => "udp6",
                    _ => continue,
                };
                out.push(Socket {
                    protocol,
                    address: if address == "*" {
                        "0.0.0.0".into()
                    } else {
                        address.into()
                    },
                    port,
                    pid,
                    program: program.clone(),
                });
            }
            _ => {}
        }
    }
    out
}

fn ps_procs(args: &[&str], cancel: &AtomicBool) -> Result<Vec<Proc>, String> {
    let mut full = vec!["-o", "pid=,ppid=,stat=,rss=,etime=,command="];
    full.extend_from_slice(args);
    Ok(parse_ps(&probe("ps", &full, cancel)?))
}

fn lsof_ports(cancel: &AtomicBool) -> Result<Vec<Socket>, String> {
    Ok(parse_lsof_sockets(&probe(
        "lsof",
        &["-nP", "-iTCP", "-sTCP:LISTEN", "-iUDP", "-FpcPn"],
        cancel,
    )?))
}

fn ps_process(pid: u32, cancel: &AtomicBool) -> Result<String, String> {
    let pid_text = pid.to_string();
    let proc = ps_procs(&["-p", &pid_text], cancel)?
        .into_iter()
        .next()
        .ok_or(format!("no visible process {pid}"))?;
    let children: Vec<Proc> = ps_procs(&["-ax"], cancel)?
        .into_iter()
        .filter(|child| child.ppid == pid)
        .collect();
    let listening: Vec<Socket> = lsof_ports(cancel)
        .unwrap_or_default()
        .into_iter()
        .filter(|socket| socket.pid == Some(pid))
        .collect();
    let mut cwd = "(not visible)".to_owned();
    let mut executable = "(not visible)".to_owned();
    let mut files = Vec::new();
    let mut fd_count = 0;
    let mut fd = String::new();
    for line in probe("lsof", &["-nP", "-p", &pid_text, "-Ffn"], cancel)
        .unwrap_or_default()
        .lines()
    {
        if let Some(value) = line.strip_prefix('f') {
            fd = value.to_owned();
            fd_count += usize::from(fd.chars().all(|c| c.is_ascii_digit()));
        } else if let Some(name) = line.strip_prefix('n') {
            match fd.as_str() {
                "cwd" => cwd = name.to_owned(),
                "txt" if executable.starts_with('(') => executable = name.to_owned(),
                digits
                    if digits.chars().all(|c| c.is_ascii_digit())
                        && name.starts_with('/')
                        && !name.starts_with("/dev/") =>
                {
                    files.push(name.to_owned())
                }
                _ => {}
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(render_process(
        &proc,
        &[
            ("cwd", cwd),
            ("executable", executable),
            ("open descriptors", fd_count.to_string()),
        ],
        &children,
        &listening,
        Some(&files),
    ))
}

fn render_process(
    proc: &Proc,
    facts: &[(&str, String)],
    children: &[Proc],
    listening: &[Socket],
    files: Option<&[String]>,
) -> String {
    let mut out = format!(
        "sys://procs/{}: {}\nparent {}  state {}  rss {}  age {}\n",
        proc.pid,
        redacted(&proc.command),
        proc.ppid,
        proc.state,
        proc.rss_bytes.map_or("-".into(), bytes),
        proc.elapsed_secs.map_or("-".into(), duration)
    );
    for (name, value) in facts {
        out.push_str(&format!("{name}: {value}\n"));
    }
    out.push_str(&format!("children ({}):", children.len()));
    for child in children.iter().take(20) {
        out.push_str(&format!("\n  {} {}", child.pid, redacted(&child.command)));
    }
    out.push_str(&format!("\nlistening ({}):", listening.len()));
    for socket in listening {
        out.push_str(&format!(
            "\n  {} {}:{}",
            socket.protocol, socket.address, socket.port
        ));
    }
    let Some(files) = files else {
        out.push_str("\nopen files: not listed on this platform");
        return out;
    };
    out.push_str(&format!("\nopen files ({}):", files.len()));
    for file in files.iter().take(MAX_FILES) {
        out.push_str(&format!("\n  {file}"));
    }
    if files.len() > MAX_FILES {
        out.push_str(&format!("\n  [{} more omitted]", files.len() - MAX_FILES));
    }
    out
}

// ---------- Dispatch ----------

fn procs(cancel: &AtomicBool) -> Result<String, String> {
    if cfg!(target_os = "linux") && proc_root().join("self").exists() {
        let (list, hidden) = linux_procs();
        return Ok(render_procs(list, "/proc", hidden));
    }
    if cfg!(unix) {
        return Ok(render_procs(ps_procs(&["-ax"], cancel)?, "ps", 0));
    }
    #[cfg(windows)]
    return win::procs();
    #[cfg(not(windows))]
    Err("sys://procs is unavailable on this platform".into())
}

fn ports(cancel: &AtomicBool) -> Result<String, String> {
    if cfg!(target_os = "linux") && proc_root().join("net").exists() {
        return Ok(render_ports(
            linux_ports(),
            "/proc/net; owners need permission to see their descriptors",
        ));
    }
    if cfg!(unix) {
        return Ok(render_ports(
            lsof_ports(cancel)?,
            "lsof; other users' sockets need root",
        ));
    }
    #[cfg(windows)]
    return win::ports();
    #[cfg(not(windows))]
    Err("sys://ports is unavailable on this platform".into())
}

fn process(pid: u32, cancel: &AtomicBool) -> Result<String, String> {
    if cfg!(target_os = "linux") && proc_root().join("self").exists() {
        return linux_process(pid);
    }
    if cfg!(unix) {
        return ps_process(pid, cancel);
    }
    #[cfg(windows)]
    return win::process(pid);
    #[cfg(not(windows))]
    Err("sys://procs/<pid> is unavailable on this platform".into())
}

mod inventory;
#[cfg(windows)]
mod win;
pub(crate) use inventory::find_on_path;
pub use inventory::{Inventory, inventory, session_inventory};

#[cfg(test)]
mod tests;
