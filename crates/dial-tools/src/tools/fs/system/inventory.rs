//! Environment inventory: OS, CPU, memory, disk, container, user and installed toolchains.
//! Collected locally without network access; toolchain versions come from bounded, parallel
//! `--version` probes of executables already on `PATH`.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Toolchains whose version is worth one probe.
const VERSIONED: &[(&str, &[&str])] = &[
    ("python3", &["--version"]),
    ("node", &["--version"]),
    ("bun", &["--version"]),
    ("deno", &["--version"]),
    ("go", &["version"]),
    ("rustc", &["--version"]),
    ("cargo", &["--version"]),
    ("java", &["-version"]),
    ("gcc", &["--version"]),
    ("clang", &["--version"]),
    ("make", &["--version"]),
    ("cmake", &["--version"]),
    ("git", &["--version"]),
    ("ruby", &["--version"]),
    ("perl", &["-e", "print $^V"]),
    ("php", &["--version"]),
    ("dotnet", &["--version"]),
    ("sqlite3", &["--version"]),
    ("uv", &["--version"]),
    ("pip3", &["--version"]),
    ("npm", &["--version"]),
    ("docker", &["--version"]),
];

/// Tools whose presence alone changes the plan.
const PRESENT: &[&str] = &[
    "python",
    "pip",
    "conda",
    "apt-get",
    "apk",
    "dnf",
    "yum",
    "pacman",
    "brew",
    "curl",
    "wget",
    "jq",
    "gdb",
    "strace",
    "ltrace",
    "valgrind",
    "tmux",
    "psql",
    "mysql",
    "redis-cli",
    "nc",
    "lsof",
    "ps",
    "ss",
    "netstat",
    "sudo",
    "g++",
    "javac",
    "pytest",
    "xxd",
    "od",
    "hexdump",
    "file",
    "strings",
];

/// Commonly assumed tools; their absence is reported explicitly.
const EXPECTED: &[&str] = &["python3", "git", "curl", "make", "gcc", "ps"];

const PROBE_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub os: String,
    pub kernel: Option<String>,
    pub arch: String,
    pub cpus: usize,
    pub memory_total: Option<u64>,
    pub memory_available: Option<u64>,
    pub disk_free: Option<(PathBuf, u64)>,
    pub container: Option<String>,
    pub user: String,
    pub root: bool,
    /// (tool, first version line); `None` when the probe timed out or printed nothing.
    pub toolchains: Vec<(String, Option<String>)>,
    pub present: Vec<String>,
    pub missing: Vec<String>,
}

impl Inventory {
    /// Full listing for `read sys://host`.
    pub fn render(&self) -> String {
        let mut out = String::from("sys://host (local, no network probes)\n");
        out.push_str(&format!("os: {}\n", self.os));
        if let Some(kernel) = &self.kernel {
            out.push_str(&format!("kernel: {kernel}\n"));
        }
        out.push_str(&format!("arch: {}  cpus: {}\n", self.arch, self.cpus));
        if let Some(total) = self.memory_total {
            out.push_str(&format!(
                "memory: {} total{}\n",
                super::bytes(total),
                self.memory_available.map_or(String::new(), |free| format!(
                    ", {} available",
                    super::bytes(free)
                ))
            ));
        }
        if let Some((path, free)) = &self.disk_free {
            out.push_str(&format!(
                "disk: {} free at {}\n",
                super::bytes(*free),
                path.display()
            ));
        }
        out.push_str(&format!(
            "container: {}\nuser: {}{}\n",
            self.container.as_deref().unwrap_or("none detected"),
            self.user,
            if self.root { " (root)" } else { "" }
        ));
        out.push_str("toolchains:\n");
        for (tool, version) in &self.toolchains {
            out.push_str(&format!(
                "  {tool}: {}\n",
                version.as_deref().unwrap_or("(version unknown)")
            ));
        }
        if !self.present.is_empty() {
            out.push_str(&format!("also on PATH: {}\n", self.present.join(", ")));
        }
        if !self.missing.is_empty() {
            out.push_str(&format!("not installed: {}\n", self.missing.join(", ")));
        }
        out.trim_end().into()
    }

    /// Prompt bullets for the session's environment section. Only fields that stay constant for
    /// the process are rendered (no free disk or available memory), so rebuilding the system
    /// prompt keeps identical bytes; `read sys://host` has the live values.
    pub fn compact(&self) -> String {
        let mut machine = format!("{} {}, {} CPUs", self.os, self.arch, self.cpus);
        if let Some(total) = self.memory_total {
            machine.push_str(&format!(", {} RAM", super::bytes(total)));
        }
        if let Some(container) = &self.container {
            machine.push_str(&format!(", container ({container})"));
        }
        if self.root {
            machine.push_str(", running as root");
        }
        let tools = self
            .toolchains
            .iter()
            .map(|(tool, version)| match version {
                Some(version) => format!("{tool} {}", short_version(version)),
                None => tool.clone(),
            })
            .chain(self.present.iter().cloned())
            .collect::<Vec<_>>()
            .join(", ");
        let mut out = format!("- Machine: {machine}.\n- On PATH: {tools}.\n");
        if !self.missing.is_empty() {
            out.push_str(&format!("- Not installed: {}.\n", self.missing.join(", ")));
        }
        out.push_str("- Live host state: read `sys://procs`, `sys://ports`, `sys://host`.\n");
        if cfg!(target_os = "linux") && self.present.iter().any(|tool| tool == "strace") {
            out.push_str(
                "- `bash` with `trace: true` runs a command under strace and summarizes the programs, paths, permissions and network it touched.\n",
            );
        }
        out
    }
}

/// The inventory collected once per process, so every prompt rebuild reuses the same bytes.
pub fn session_inventory() -> &'static Inventory {
    static CACHED: std::sync::OnceLock<Inventory> = std::sync::OnceLock::new();
    CACHED.get_or_init(|| inventory(None))
}

/// The first dotted version number in a probe line (`Python 3.12.1` → `3.12.1`), else the line.
fn short_version(line: &str) -> &str {
    line.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
        .map(|word| word.trim_start_matches(['v', 'V']))
        .find(|word| word.contains('.') && word.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or(line)
}

fn first_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(line.chars().take(100).collect())
}
/// An executable named `name` on `path` (a `PATH` value), without running it. On Windows each
/// `PATHEXT` suffix is tried, so `git` finds `git.exe`.
pub(crate) fn find_on_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    let suffixes: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|suffix| !suffix.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(path).find_map(|dir| {
        suffixes
            .iter()
            .map(|suffix| dir.join(format!("{name}{suffix}")))
            .find(|candidate| dial_core::platform::fs::is_executable(candidate))
    })
}

/// Outcome of one version probe.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Probe {
    /// Exited successfully; the first non-empty output line, if any.
    Ran(Option<String>),
    /// Still running at the deadline (killed); the tool exists but its version is unknown.
    TimedOut,
    /// Could not start or exited unsuccessfully (for example the Windows Store `python3` alias),
    /// so the tool is not usable.
    Failed,
}

/// One version probe with a deadline. The child is killed, not abandoned, when it overruns.
pub(super) fn version(executable: &Path, args: &[&str], deadline: Instant) -> Probe {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = dial_core::platform::process::spawn(&mut command) else {
        return Probe::Failed;
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let exited = dial_core::platform::process::wait_exited(&child, remaining).unwrap_or(false);
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        return Probe::TimedOut;
    }
    match child.wait_with_output() {
        Ok(output) if output.status.success() => {
            Probe::Ran(first_line(&output.stdout).or_else(|| first_line(&output.stderr)))
        }
        _ => Probe::Failed,
    }
}

fn os_name() -> String {
    if let Ok(release) = std::fs::read_to_string("/etc/os-release")
        && let Some(name) = release
            .lines()
            .find_map(|line| line.strip_prefix("PRETTY_NAME="))
    {
        return name.trim_matches('"').to_owned();
    }
    if cfg!(target_os = "macos")
        && let Ok(out) = Command::new("sw_vers").arg("-productVersion").output()
        && let Some(version) = first_line(&out.stdout)
    {
        return format!("macOS {version}");
    }
    std::env::consts::OS.to_owned()
}

#[cfg(unix)]
fn kernel() -> Option<String> {
    // SAFETY: `uname` fills the zero-initialized struct it is given; every field is a
    // NUL-terminated C string on success.
    unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut name) != 0 {
            return None;
        }
        let field = |raw: &[libc::c_char]| {
            std::ffi::CStr::from_ptr(raw.as_ptr())
                .to_string_lossy()
                .into_owned()
        };
        Some(format!("{} {}", field(&name.sysname), field(&name.release)))
    }
}

#[cfg(not(unix))]
fn kernel() -> Option<String> {
    None
}

fn memory() -> (Option<u64>, Option<u64>) {
    if let Ok(info) = std::fs::read_to_string("/proc/meminfo") {
        let field = |key: &str| {
            info.lines()
                .find_map(|line| line.strip_prefix(key))?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .map(|kib| kib * 1024)
        };
        return (field("MemTotal:"), field("MemAvailable:"));
    }
    if cfg!(target_os = "macos")
        && let Ok(out) = Command::new("sysctl").args(["-n", "hw.memsize"]).output()
    {
        return (
            first_line(&out.stdout).and_then(|text| text.parse().ok()),
            None,
        );
    }
    (None, None)
}

#[cfg(unix)]
fn disk_free(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` reads a NUL-terminated path and fills the zero-initialized struct.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
}

#[cfg(not(unix))]
fn disk_free(_: &Path) -> Option<u64> {
    None
}

fn container() -> Option<String> {
    if Path::new("/.dockerenv").exists() {
        return Some("docker".into());
    }
    if Path::new("/run/.containerenv").exists() {
        return Some("podman".into());
    }
    let cgroup = std::fs::read_to_string("/proc/1/cgroup").unwrap_or_default();
    ["kubepods", "docker", "containerd", "lxc"]
        .into_iter()
        .find(|kind| cgroup.contains(kind))
        .map(str::to_owned)
}

fn user() -> (String, bool) {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let name = std::env::var("USER").unwrap_or_else(|_| format!("uid {uid}"));
        (name, uid == 0)
    }
    #[cfg(not(unix))]
    {
        (
            std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()),
            false,
        )
    }
}

/// Collect the inventory. `disk_root` selects the filesystem whose free space is reported
/// (the workspace when known, else the current directory).
pub fn inventory(disk_root: Option<&Path>) -> Inventory {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let deadline = Instant::now() + PROBE_DEADLINE;
    let probes: Vec<_> = VERSIONED
        .iter()
        .filter_map(|(tool, args)| Some((*tool, *args, find_on_path(tool, &path)?)))
        .map(|(tool, args, executable)| {
            (
                tool,
                std::thread::spawn(move || version(&executable, args, deadline)),
            )
        })
        .collect();
    let (memory_total, memory_available) = memory();
    let disk = disk_root
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());
    let (user, root) = user();
    let toolchains: Vec<(String, Option<String>)> = probes
        .into_iter()
        .filter_map(
            |(tool, probe)| match probe.join().unwrap_or(Probe::Failed) {
                Probe::Ran(line) => Some((tool.to_owned(), line)),
                Probe::TimedOut => Some((tool.to_owned(), None)),
                Probe::Failed => None,
            },
        )
        .collect();
    let present: Vec<String> = PRESENT
        .iter()
        .filter(|tool| find_on_path(tool, &path).is_some())
        .map(|tool| (*tool).to_owned())
        .collect();
    // A versioned tool counts only when its probe ran, so a broken shim is reported missing.
    let missing = EXPECTED
        .iter()
        .filter(|tool| {
            let versioned = VERSIONED.iter().any(|(name, _)| name == *tool);
            if versioned {
                !toolchains.iter().any(|(name, _)| name == *tool)
            } else {
                find_on_path(tool, &path).is_none()
            }
        })
        .map(|tool| (*tool).to_owned())
        .collect();
    Inventory {
        os: os_name(),
        kernel: kernel(),
        arch: std::env::consts::ARCH.to_owned(),
        cpus: std::thread::available_parallelism().map_or(1, usize::from),
        memory_total,
        memory_available,
        disk_free: disk.and_then(|path| Some((path.clone(), disk_free(&path)?))),
        container: container(),
        user,
        root,
        toolchains,
        present,
        missing,
    }
}
