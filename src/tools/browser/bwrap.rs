//! Linux containment for the browser driver: bubblewrap with no workspace mount.
//!
//! The driver never reads workspace files (uploads travel as base64 from Rust),
//! so the workspace is deliberately absent from the mount namespace. Only the
//! system read roots, the Node/Playwright/Chromium install roots, and the
//! per-session state directory are visible.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const BWRAP: &str = "/usr/bin/bwrap";
const SYSTEM_READ: [&str; 6] = ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"];

/// Mount and network plan for one browser driver process.
pub(super) struct Plan<'a> {
    pub(super) read_only: &'a [PathBuf],
    pub(super) writable: &'a [PathBuf],
    pub(super) network: bool,
    pub(super) cwd: &'a Path,
}

/// System directories that exist on this host and must be visible read-only.
pub(super) fn system_read_roots() -> Vec<PathBuf> {
    SYSTEM_READ
        .iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}

/// Pure argument vector for `bwrap`, ending with `-- program args...`.
pub(super) fn args(plan: &Plan<'_>, program: &Path, program_args: &[OsString]) -> Vec<OsString> {
    let mut out: Vec<OsString> = [
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
    ]
    .iter()
    .map(OsString::from)
    .collect();
    if !plan.network {
        out.push("--unshare-net".into());
    }
    for path in plan.read_only {
        out.push("--ro-bind".into());
        out.push(path.as_os_str().to_owned());
        out.push(path.as_os_str().to_owned());
    }
    for path in plan.writable {
        out.push("--bind".into());
        out.push(path.as_os_str().to_owned());
        out.push(path.as_os_str().to_owned());
    }
    for item in [
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/dev/shm",
        "--remount-ro",
        "/",
        "--chdir",
    ] {
        out.push(item.into());
    }
    out.push(plan.cwd.as_os_str().to_owned());
    out.push("--".into());
    out.push(program.as_os_str().to_owned());
    out.extend(program_args.iter().cloned());
    out
}

/// Build the `bwrap` command. Fails closed when containment cannot be guaranteed.
pub(super) fn command(
    plan: &Plan<'_>,
    program: &Path,
    program_args: &[OsString],
) -> Result<Command, String> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return Err("browser containment refuses to run as root".into());
    }
    if !Path::new(BWRAP).is_file() {
        return Err(format!(
            "browser containment requires {BWRAP}; install bubblewrap"
        ));
    }
    let mut command = Command::new(BWRAP);
    command.args(args(plan, program, program_args));
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[OsString]) -> Vec<String> {
        values
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn no_network_unshares_net_and_program_follows_separator() {
        let read = [PathBuf::from("/usr")];
        let write = [PathBuf::from("/state/browser")];
        let plan = Plan {
            read_only: &read,
            writable: &write,
            network: false,
            cwd: Path::new("/state/browser"),
        };
        let out = strings(&args(
            &plan,
            Path::new("/usr/bin/node"),
            &[OsString::from("-e")],
        ));
        assert!(out.iter().any(|a| a == "--unshare-net"));
        let sep = out.iter().position(|a| a == "--").expect("separator");
        assert_eq!(&out[sep + 1..], ["/usr/bin/node", "-e"]);
        assert!(
            out.windows(3)
                .any(|w| w == ["--bind", "/state/browser", "/state/browser"])
        );
        assert!(out.windows(3).any(|w| w == ["--ro-bind", "/usr", "/usr"]));
    }

    #[test]
    fn network_mode_keeps_net_namespace_shared_and_never_mounts_workspace() {
        let plan = Plan {
            read_only: &[],
            writable: &[],
            network: true,
            cwd: Path::new("/state/browser"),
        };
        let out = strings(&args(&plan, Path::new("/usr/bin/node"), &[]));
        assert!(!out.iter().any(|a| a == "--unshare-net"));
        assert!(out.iter().any(|a| a == "--die-with-parent"));
        assert!(out.iter().any(|a| a == "--cap-drop"));
        assert!(!out.iter().any(|a| a.contains("workspace")));
    }
}
