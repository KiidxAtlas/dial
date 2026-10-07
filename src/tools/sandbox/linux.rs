//! Linux/WSL2 confinement using bubblewrap's user, mount, PID, IPC and network namespaces.
//!
//! Unlike macOS Dev mode, Linux Dev reads are bounded to the system runtime, the workspace
//! and explicit extra-write directories. No host home, /run or /tmp is exposed wholesale.
//! A denied path overlapping any exposed tree is rejected rather than silently ignored.
//! This deliberately stricter policy avoids host IPC sockets and protects absent secret paths
//! without an unsafe allow-by-default mount. There is never an unsandboxed fallback.

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
}

impl Mounts {
    fn new(workspace: &Path, policy: &Policy) -> Result<Self, String> {
        let workspace = directory(workspace)?;
        let (network, denied, extra) = match policy {
            Policy::Strict { allow_network } => (*allow_network, &[][..], &[][..]),
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
        // WSL's resolv.conf can point outside /etc (e.g. /mnt/wsl/resolv.conf).
        // Expose that one read-only file, never its containing Windows mount.
        if let Ok(dns) = fs::canonicalize("/etc/resolv.conf") {
            if !dns.starts_with("/etc") && dns.is_file() {
                read.push((dns.clone(), dns));
            }
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
                || read
                    .iter()
                    .any(|(source, target)| overlaps(path, source) || overlaps(path, target))
            {
                return Err(format!(
                    "Linux sandbox cannot make a system/runtime tree writable: {}",
                    path.display()
                ));
            }
        }
        for path in denied {
            let denied = resolve(path)?;
            if write.iter().any(|path| overlaps(path, &denied))
                || read
                    .iter()
                    .any(|(source, target)| overlaps(source, &denied) || overlaps(target, &denied))
                || ["/proc", "/dev"]
                    .iter()
                    .any(|root| overlaps(Path::new(root), &denied))
            {
                return Err(format!(
                    "Linux sandbox refuses overlapping allowed and denied paths: {}; keep secrets outside the workspace and extra_write directories",
                    denied.display()
                ));
            }
        }
        Ok(Self {
            read,
            write,
            network,
        })
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
        } else if in_interop {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("enabled") {
                    settings.push(value.trim().eq_ignore_ascii_case("false"));
                }
            }
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
    if !Path::new(BWRAP).is_file() {
        return Err("bubblewrap is missing; install it with sudo apt install bubblewrap".into());
    }
    // A root WSL session has a different threat model; never silently run with host root rights.
    if unsafe { libc::geteuid() } == 0 {
        return Err("run Dial as your normal Linux/WSL user, not root or sudo".into());
    }
    check_windows_interop()?;
    let mounts = Mounts::new(workspace, policy)?;
    let mut command = Command::new(BWRAP);
    command.args([
        "--die-with-parent",
        "--new-session",
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

pub(super) fn probe_policy(workspace: &Path, policy: &Policy) -> Sandbox {
    let network = match policy {
        Policy::Strict { allow_network } | Policy::Dev { allow_network, .. } => *allow_network,
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
    let strict = Policy::Strict {
        allow_network: false,
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
mod tests {
    use super::*;

    #[test]
    fn linux_policy_rejects_root_missing_and_secret_overlaps() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path().canonicalize().unwrap();
        let strict = Policy::Strict {
            allow_network: false,
        };
        assert!(Mounts::new(Path::new("/"), &strict).is_err());
        assert!(Mounts::new(&ws.join("missing"), &strict).is_err());
        for denied in [&ws, &ws.join(".env"), Path::new("/etc/hosts")] {
            let policy = Policy::Dev {
                deny_read: vec![denied.to_owned()],
                extra_write: vec![],
                allow_network: false,
            };
            assert!(Mounts::new(&ws, &policy).is_err(), "{denied:?}");
        }
        let policy = Policy::Dev {
            deny_read: vec![],
            extra_write: vec![PathBuf::from("/usr")],
            allow_network: false,
        };
        assert!(Mounts::new(&ws, &policy).is_err());
    }

    #[test]
    fn wsl_requires_disabled_config_and_current_interpreter() {
        let kernel = "6.6.87.2-microsoft-standard-WSL2";
        let disabled = "[interop]\nenabled=false\nappendWindowsPath=false\n";
        assert!(windows_interop_safe("generic-linux", "", Some("enabled")));
        assert!(windows_interop_safe(kernel, disabled, None));
        assert!(windows_interop_safe(kernel, disabled, Some("disabled\n")));
        assert!(!windows_interop_safe(
            kernel,
            disabled,
            Some("enabled\nflags: PF\n")
        ));
        assert!(!windows_interop_safe(
            kernel,
            "[interop]\nenabled=true",
            None
        ));
        assert!(!windows_interop_safe(kernel, "", None));
        assert!(!windows_interop_safe(
            kernel,
            "[interop]\nenabled=false\nenabled=true",
            None
        ));
        assert!(!windows_interop_safe(
            kernel,
            "[interop]\nenabled=false\n[interop]\nenabled=false",
            None
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_enforces_filesystem_and_both_network_modes() {
        // Required, not skipped: a release runner without a working sandbox must fail.
        mechanism(false).unwrap();
        mechanism(true).unwrap();
    }
}
