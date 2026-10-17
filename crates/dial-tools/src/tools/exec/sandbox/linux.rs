//! Linux/WSL2 confinement using bubblewrap's user, mount, PID, IPC and network namespaces.
//!
//! Unlike macOS Dev mode, Linux Dev reads are bounded to the system runtime, the workspace
//! and explicit extra-write directories. No host home, /run or /tmp is exposed wholesale.
//! Nested private paths are masked after all binds; unsafe overlaps fail closed.
//! This deliberately stricter policy avoids host IPC sockets and protects absent secret paths
//! without an unsafe allow-by-default mount. There is never an unsandboxed fallback.
// Compiled on macOS only for tests; its helpers are used by the Linux build and self-tests.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use super::{Policy, Sandbox, resolve, run_probe_command};
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

const BWRAP: &str = "/usr/bin/bwrap";
const SYSTEM_READ: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"];

struct Mounts {
    read: Vec<(PathBuf, PathBuf)>,
    write: Vec<PathBuf>,
    network: bool,
    denied: Vec<PathBuf>,
    masks: Vec<(PathBuf, bool)>,
}

impl Mounts {
    fn new(workspace: &Path, policy: &Policy) -> Result<Self, String> {
        let workspace = directory(workspace)?;
        let (network, denied, extra) = match policy {
            Policy::Strict {
                deny_read,
                allow_network,
                extra_write,
            } => (*allow_network, deny_read.as_slice(), extra_write.as_slice()),
            Policy::Dev {
                deny_read,
                extra_write,
                allow_network,
            } => (*allow_network, deny_read.as_slice(), extra_write.as_slice()),
        };
        let mut read = Vec::new();
        for path in SYSTEM_READ {
            let target = PathBuf::from(path);
            if target.exists() {
                read.push((resolve(&target)?, target));
            }
        }
        // Home toolchain roots are read-only by default, but an explicit write root at or below one
        // (for example `extra_write = ["~/.cargo"]`) may shadow it: write binds are mounted after
        // read binds. The workspace may itself be the home directory; its existing
        // write grant covers nested toolchains too. System runtime trees stay read-only.
        let mut toolchain_read = Vec::new();
        for path in [".cargo", ".rustup", ".nvm", ".pyenv", ".local/bin"] {
            if let Some(home) = std::env::var_os("HOME") {
                let path = PathBuf::from(home).join(path);
                if path.is_dir() {
                    let resolved = resolve(&path)?;
                    if !read.iter().any(|(source, _)| source == &resolved) {
                        toolchain_read.push(resolved.clone());
                        read.push((resolved.clone(), resolved));
                    }
                }
            }
        }
        for path in ["/opt", "/nix", "/usr/local"] {
            let path = PathBuf::from(path);
            if path.is_dir() {
                let resolved = resolve(&path)?;
                if !read.iter().any(|(source, _)| source == &resolved) {
                    read.push((resolved.clone(), resolved));
                }
            }
        }
        // WSL's resolv.conf can point outside /etc (e.g. /mnt/wsl/resolv.conf).
        // Expose that one read-only file, never its containing Windows mount.
        if let Ok(dns) = fs::canonicalize("/etc/resolv.conf")
            && !dns.starts_with("/etc")
            && dns.is_file()
        {
            read.push((dns.clone(), dns));
        }
        let mut write = vec![workspace];
        for path in extra {
            let path = directory(path)?;
            if !write.contains(&path) {
                write.push(path);
            }
        }
        for path in &write {
            if ["/proc", "/dev"]
                .iter()
                .any(|root| overlaps(path, Path::new(root)))
                || read.iter().any(|(source, target)| {
                    (overlaps(path, source) || overlaps(path, target))
                        && !toolchain_read.iter().any(|root| {
                            source == root
                                && (path.starts_with(root)
                                    || (path == &write[0] && root.starts_with(path)))
                        })
                })
            {
                return Err(format!(
                    "Linux sandbox cannot make a system/runtime tree writable: {}",
                    path.display()
                ));
            }
        }
        let mut denied = denied
            .iter()
            .map(|p| resolve(p))
            .collect::<Result<Vec<_>, _>>()?;
        // Reduce resolved exclusions before inspecting their contents. An outer
        // private tree already hides all descendants, including state directories
        // that have not yet been created inside it.
        denied.sort_by_key(|path| path.components().count());
        let mut outer: Vec<PathBuf> = Vec::new();
        for path in denied {
            if !outer.iter().any(|root| path.starts_with(root)) {
                outer.push(path);
            }
        }
        let denied = outer;
        let mut masks = Vec::new();
        for denied in &denied {
            // Never mask runtime mounts or an entire writable root. Nested exclusions
            // are applied only after every writable bind, so extra roots cannot undo them.
            if write.iter().any(|path| path.starts_with(denied))
                || read
                    .iter()
                    .any(|(source, target)| overlaps(source, denied) || overlaps(target, denied))
                || ["/proc", "/dev"]
                    .iter()
                    .any(|root| overlaps(Path::new(root), denied))
            {
                return Err(format!(
                    "Linux sandbox refuses overlapping allowed and denied paths: {}",
                    denied.display()
                ));
            }
            if write.iter().any(|path| denied.starts_with(path)) {
                let metadata = fs::metadata(denied).map_err(|e| {
                    format!(
                        "cannot safely mask private path {} (must already exist): {e}",
                        denied.display()
                    )
                })?;
                if !metadata.is_dir() && !metadata.is_file() {
                    return Err(format!(
                        "cannot safely mask private special file {}",
                        denied.display()
                    ));
                }
                masks.push((denied.clone(), metadata.is_dir()));
            }
        }
        Ok(Self {
            read,
            write,
            network,
            denied,
            masks,
        })
    }

    /// Makes the program the sandbox runs readable and executable. A program already under a read
    /// mount or a writable root needs nothing; any other program gets one read-only bind.
    fn grant_program(&mut self, program: &str) -> Result<(), String> {
        let path = Path::new(program);
        if !path.is_absolute() {
            return Ok(());
        }
        let resolved = resolve(path)?;
        if self
            .denied
            .iter()
            .any(|denied| resolved.starts_with(denied))
        {
            return Err(format!(
                "program is inside a denied path: {}",
                resolved.display()
            ));
        }
        if self.write.iter().any(|root| resolved.starts_with(root))
            || self
                .read
                .iter()
                .any(|(source, _)| resolved.starts_with(source))
        {
            return Ok(());
        }
        self.read.push((resolved.clone(), resolved));
        Ok(())
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn directory(path: &Path) -> Result<PathBuf, String> {
    let path = resolve(path)?;
    if path == Path::new("/") || !path.is_dir() {
        return Err(format!(
            "sandbox requires an existing non-root directory: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn windows_interop_safe(kernel: &str, config: &str, handler: Option<&str>) -> bool {
    if !kernel.to_ascii_lowercase().contains("microsoft") {
        return true;
    }
    // Editing wsl.conf without restarting must not admit an enabled interpreter.
    if handler.is_some_and(|text| text.lines().next() != Some("disabled")) {
        return false;
    }
    let mut in_interop = false;
    let mut sections = 0;
    let mut settings = Vec::new();
    for line in config.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_interop = line.eq_ignore_ascii_case("[interop]");
            sections += usize::from(in_interop);
        } else if in_interop
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("enabled")
        {
            settings.push(value.trim().eq_ignore_ascii_case("false"));
        }
    }
    sections == 1 && settings == [true]
}

fn check_windows_interop() -> Result<(), String> {
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").map_err(|e| e.to_string())?;
    if !kernel.to_ascii_lowercase().contains("microsoft") {
        return Ok(());
    }
    let config = fs::read_to_string("/etc/wsl.conf").unwrap_or_default();
    let handler = match fs::read_to_string("/proc/sys/fs/binfmt_misc/WSLInterop") {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("checking WSL Windows interop: {e}")),
    };
    if !windows_interop_safe(&kernel, &config, handler.as_deref()) {
        return Err("disable Windows executable interop with [interop] enabled=false in /etc/wsl.conf, then run wsl --shutdown from PowerShell and reopen Ubuntu; Windows processes cannot be confined by the Linux sandbox".into());
    }
    Ok(())
}

pub(super) fn command(
    workspace: &Path,
    policy: &Policy,
    program: &str,
    args: &[&str],
) -> Result<Command, String> {
    command_mode(workspace, policy, program, args, false)
}

/// Only a caller that first creates a fresh private PTY/session may retain that session.
/// Never share Dial's host terminal: that would reintroduce TIOCSTI input injection.
pub(super) fn command_mode(
    workspace: &Path,
    policy: &Policy,
    program: &str,
    args: &[&str],
    owned_terminal: bool,
) -> Result<Command, String> {
    if !Path::new(BWRAP).is_file() {
        return Err("bubblewrap is missing; install it with sudo apt install bubblewrap".into());
    }
    // A root WSL session has a different threat model; never silently run with host root rights.
    if unsafe { libc::geteuid() } == 0 {
        return Err("run Dial as your normal Linux/WSL user, not root or sudo".into());
    }
    check_windows_interop()?;
    let mut mounts = Mounts::new(workspace, policy)?;
    mounts.grant_program(program)?;
    let mut command = Command::new(BWRAP);
    command.arg("--die-with-parent");
    if !owned_terminal {
        command.arg("--new-session");
    }
    command.args([
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--cap-drop",
        "ALL",
        "--tmpfs",
        "/",
    ]);
    if !mounts.network {
        command.arg("--unshare-net");
    }
    for (source, target) in mounts.read {
        command.arg("--ro-bind").arg(source).arg(target);
    }
    command.args([
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--remount-ro",
        "/proc",
        "--remount-ro",
        "/dev",
    ]);
    for path in mounts.write {
        command.arg("--bind").arg(&path).arg(path);
    }
    for (path, directory) in mounts.masks {
        if directory {
            command.args(["--perms", "000", "--tmpfs"]).arg(&path);
            command.arg("--remount-ro").arg(path);
        } else {
            command.args(["--ro-bind", "/dev/null"]).arg(path);
        }
    }
    // Synthetic ancestors created by bwrap must not become writable escape paths.
    // Separate workspace/extra-write mounts stay writable when the root mount is sealed.
    command
        .args(["--remount-ro", "/", "--chdir"])
        .arg(workspace);
    command.arg("--").arg(program).args(args);
    Ok(command)
}

fn run(
    workspace: &Path,
    policy: &Policy,
    script: &str,
    args: &[&str],
) -> Result<(Option<i32>, String), String> {
    let mut arguments = vec!["-c", script, "_"];
    arguments.extend_from_slice(args);
    let mut command = command(workspace, policy, "/bin/bash", &arguments)?;
    command
        .current_dir(workspace)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    run_probe_command(command)
}

fn mechanism(network: bool) -> Result<(), String> {
    static PROBES: [LazyLock<Result<(), String>>; 2] = [
        LazyLock::new(|| self_test(false)),
        LazyLock::new(|| self_test(true)),
    ];
    PROBES[usize::from(network)].clone()
}

pub(super) fn probe() -> Sandbox {
    match mechanism(false) {
        Ok(()) => Sandbox::Available,
        Err(reason) => unavailable(reason),
    }
}

fn unavailable(reason: String) -> Sandbox {
    Sandbox::Unavailable(format!(
        "bubblewrap self-test failed: {reason}; Linux/WSL2 needs bubblewrap and working unprivileged user namespaces"
    ))
}

#[cfg(test)]
pub(super) fn probe_policy(workspace: &Path, policy: &Policy) -> Sandbox {
    let network = match policy {
        Policy::Strict { allow_network, .. } | Policy::Dev { allow_network, .. } => *allow_network,
    };
    let checked = mechanism(network).and_then(|()| {
        let (code, _) = run(workspace, policy, "exit 0", &[])?;
        if code == Some(0) {
            Ok(())
        } else {
            Err(format!("policy probe exited with {code:?}"))
        }
    });
    match checked {
        Ok(()) => Sandbox::Available,
        Err(reason) => unavailable(reason),
    }
}

fn self_test(network: bool) -> Result<(), String> {
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let base = temp.path().canonicalize().map_err(|e| e.to_string())?;
    let workspace = base.join("ws");
    let outside = base.join("outside");
    let extra = base.join("extra");
    for path in [&workspace, &outside, &extra] {
        fs::create_dir(path).map_err(|e| e.to_string())?;
    }
    fs::write(outside.join("secret"), "private").map_err(|e| e.to_string())?;
    std::os::unix::fs::symlink(outside.join("secret"), workspace.join("escape"))
        .map_err(|e| e.to_string())?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port()
        .to_string();
    let policy = Policy::Dev {
        deny_read: vec![outside.clone(), base.join("not-created-yet")],
        extra_write: vec![extra.clone()],
        allow_network: network,
    };
    let outside = outside.to_str().ok_or("probe path is not UTF-8")?;
    let extra = extra.to_str().ok_or("probe path is not UTF-8")?;
    let script = r#"
        echo ok > probe.txt && [ "$(cat probe.txt)" = ok ] || exit 10
        cat /etc/hosts >/dev/null || exit 11
        if cat "$1/secret" 2>/dev/null || cat escape 2>/dev/null; then exit 12; fi
        if echo bad > "$1/new" 2>/dev/null; then exit 13; fi
        if echo bad > ../escape.txt 2>/dev/null; then exit 14; fi
        echo ok > "$2/probe.txt" || exit 15
        if (exec 3<>/dev/tcp/127.0.0.1/"$3") 2>/dev/null; then connected=1; else connected=0; fi
        [ "$connected" = "$4" ] || exit 16
        echo ok
    "#;
    let (code, output) = run(
        &workspace,
        &policy,
        script,
        &[outside, extra, &port, if network { "1" } else { "0" }],
    )?;
    if code != Some(0) || output.trim() != "ok" {
        return Err(format!("enforcement probe exited with {code:?}: {output}"));
    }
    let private = workspace.join("state/private");
    fs::create_dir_all(&private).map_err(|e| e.to_string())?;
    fs::write(private.join("secret"), "private").map_err(|e| e.to_string())?;
    for policy in [
        Policy::Strict {
            deny_read: vec![private.clone()],
            extra_write: vec![],
            allow_network: network,
        },
        Policy::Dev {
            deny_read: vec![private.clone()],
            extra_write: vec![],
            allow_network: network,
        },
    ] {
        let (code, output) = run(
            &workspace,
            &policy,
            r#"
            echo ordinary > ordinary && [ "$(cat ordinary)" = ordinary ] || exit 20
            if cat state/private/secret 2>/dev/null; then exit 21; fi
            if echo bad > state/private/new 2>/dev/null; then exit 22; fi
            echo ok
        "#,
            &[],
        )?;
        if code != Some(0) || output.trim() != "ok" {
            return Err(format!(
                "nested private state probe exited with {code:?}: {output}"
            ));
        }
    }
    let strict = Policy::Strict {
        deny_read: Vec::new(),
        allow_network: false,
        extra_write: Vec::new(),
    };
    let (code, output) = run(
        &workspace,
        &strict,
        "if cat \"$1/probe.txt\" 2>/dev/null; then exit 12; fi; echo ok",
        &[extra],
    )?;
    if code != Some(0) || output.trim() != "ok" {
        return Err("strict policy exposed extra-write paths".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
