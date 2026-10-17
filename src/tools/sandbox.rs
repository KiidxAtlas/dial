//! OS confinement for model-facing commands: macOS `sandbox-exec`, Linux/WSL2 `bubblewrap`.
//!
//! Linux/WSL2 uses a bounded filesystem view even in Dev mode; see `sandbox/linux.rs`.
//! The profiles and residual limits below describe the unchanged macOS backend.
//!
//! The profile is deny-by-default. Reads are limited to the system locations
//! needed to run bash, coreutils, Homebrew python and git; reads and writes are
//! limited to the workspace; the network is denied unless the caller opts in.
//! There is no unsandboxed fallback: when the sandbox cannot be established or
//! fails its enforcement self-test, [`Sandbox::available`] returns an error and
//! callers must refuse to run commands.
//!
//! Residual limits (see also the tools module docs):
//! * `sandbox-exec` is deprecated by Apple, though it still ships and enforces.
//! * World-readable system paths stay readable: `/usr`, `/bin`, `/sbin`,
//!   `/System`, `/Library`, `/private/etc`, `/dev`, `/opt/homebrew`.
//! * `mach-lookup` and `sysctl-read` are allowed wholesale (needed by dyld,
//!   libc, DNS and most tools); they expose host metadata and system services.
//! * Metadata (existence, size, mtime) is hidden for most of the filesystem but
//!   not for the workspace's ancestor directories or the allowed system paths.
//! * Timing and CPU/memory exhaustion are not constrained.
//!
//! # Dev policy ([`Policy::Dev`])
//!
//! Allow-by-default so toolchains (git, cargo, npm, python, node, rustc) work,
//! then, in this order (the last matching SBPL rule wins):
//! 1. `(deny file-write*)`, then `(allow file-write* …)` for the workspace
//!    (which holds the `.tmp` scratch TMPDIR), `extra_write`, a few devices
//!    (`/dev/null`, `/dev/zero`, `/dev/tty`, `/dev/ptmx`, `/dev/ttysN`,
//!    `/dev/dtracehelper`) and `/dev/fd` (for `> /dev/stderr`);
//! 2. `(deny network*)` unless allowed;
//! 3. `(deny network-outbound (remote unix-socket …))` for the launchd
//!    ssh-agent listener and `docker.sock`;
//! 4. `(deny mach-lookup …)` for the pasteboard and keychain daemons;
//! 5. `(deny file-read* file-write* (subpath …))` per `deny_read` path, last so
//!    it also beats the workspace and `extra_write`. Paths are resolved
//!    (symlinks, `..`) before use because the kernel matches resolved paths;
//!    a symlink inside the workspace to a denied path is therefore refused.
//!
//! The Dev environment is rebuilt from the caller's, minus likely secrets
//! ([`is_secret_env`]), with `TMPDIR` pointing at the scratch dir. Toolchain
//! caches (`UV_CACHE_DIR`, `PIP_CACHE_DIR`, `npm_config_cache`, `GOCACHE`,
//! `XDG_CACHE_HOME`) that are not already writable point at a dial-owned
//! sandbox cache (`~/Library/Caches/dial/sandbox`, `~/.cache/dial/sandbox` on
//! Linux), which is added to the write set; the user's own caches stay
//! read-only. Strict commands are unaffected.
//!
//! Dev residual limits:
//! * Other same-user processes' environment and arguments remain readable
//!   (`sysctl kern.procargs2`); no SBPL rule found on this macOS version
//!   blocks it, so never keep secrets in the harness process environment.
//! * Descriptors opened before the sandbox starts stay usable; the unsandboxed
//!   parent's inherited fds are the caller's responsibility.
//! * Most Mach services stay reachable: AppleEvents/LaunchServices (`osascript`,
//!   `open`) can drive other apps that run unsandboxed. Denying them hangs
//!   `osascript` and risks breaking CoreFoundation tools, so they are not denied.
//! * Anything the workspace holds is trusted later by unsandboxed tools:
//!   `.git/hooks`, `.git/config`, package scripts, build scripts.
//! * `mktemp` without a template and `mktemp -t` use the per-user
//!   `/var/folders` dir (not `$TMPDIR`) and fail; pass it via `extra_write`
//!   if scripts need it.
//! * Denied paths that do not exist yet are matched by their resolved
//!   parent plus the name, but a denied *symlink leaf* only protects its
//!   target; `readlink` on the link itself still works.
//! * Hard links: creating one to a denied file is refused (needs read access),
//!   but a hard link made before the sandbox started is a distinct path.

#[cfg(any(target_os = "linux", all(test, unix)))]
mod linux;

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// System locations a command may read (never write) to start up and run.
const SYSTEM_READ: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/private/etc",
    "/dev",
    "/opt/homebrew",
];

/// Devices that commands routinely write to (`> /dev/null`, dyld tracing).
const WRITABLE_DEVICES: &[&str] = &["/dev/null", "/dev/dtracehelper"];

/// What a sandboxed command may touch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Deny-by-default: the system read set plus the workspace and `extra_write`, the
    /// attached workspace roots that get the same access as the workspace (see [`profile`]).
    /// `deny_read` excludes protected state from reads and writes, overriding those grants.
    Strict {
        deny_read: Vec<PathBuf>,
        allow_network: bool,
        extra_write: Vec<PathBuf>,
    },
    /// Developer-friendly: allow-by-default, then writes are limited to the
    /// workspace, its scratch dir, a few devices and `extra_write`; reads *and
    /// writes* under every `deny_read` path are refused (the rules come last,
    /// so they also win over `extra_write` and the workspace). See
    /// [`dev_profile`].
    Dev {
        deny_read: Vec<PathBuf>,
        extra_write: Vec<PathBuf>,
        allow_network: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sandbox {
    Available,
    Unavailable(String),
}

impl Sandbox {
    /// Probe the host once per process. The probe runs a real command under a
    /// real profile and verifies that workspace writes work *and* that a file
    /// outside the workspace is unreadable, so a sandbox that silently fails
    /// to enforce is reported as unavailable.
    pub fn detect() -> Sandbox {
        static DETECTED: LazyLock<Sandbox> = LazyLock::new(probe);
        DETECTED.clone()
    }

    pub fn available(&self) -> Result<(), String> {
        match self {
            Sandbox::Available => Ok(()),
            Sandbox::Unavailable(reason) => {
                let nested = if already_sandboxed() {
                    " (this process already runs inside a sandbox, which macOS cannot nest)"
                } else {
                    ""
                };
                Err(format!(
                    "bash is disabled because no sandbox is available: {reason}{nested}"
                ))
            }
        }
    }

    /// Build a command that runs `program args…` inside the sandbox. The caller
    /// sets cwd, environment and stdio. `workspace` must be canonical.
    pub fn command(
        &self,
        workspace: &Path,
        allow_network: bool,
        program: &str,
        args: &[&str],
    ) -> Result<Command, String> {
        let policy = Policy::Strict {
            deny_read: Vec::new(),
            allow_network,
            extra_write: Vec::new(),
        };
        self.command_mode(workspace, &policy, program, args, None)
    }

    fn command_mode(
        &self,
        workspace: &Path,
        policy: &Policy,
        program: &str,
        args: &[&str],
        owned_terminal: Option<i32>,
    ) -> Result<Command, String> {
        self.available()?;
        #[cfg(target_os = "linux")]
        return linux::command_mode(workspace, policy, program, args, owned_terminal.is_some());
        #[cfg(windows)]
        {
            let _ = (workspace, policy, program, args, owned_terminal);
            Err(
                "Windows command isolation is unavailable; refusing to run a sandboxed command"
                    .into(),
            )
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let Policy::Strict {
                deny_read,
                allow_network,
                extra_write,
            } = policy
            else {
                return Err("command_mode requires a Strict policy on this platform".into());
            };
            let mut profile = strict_profile(workspace, deny_read, extra_write, *allow_network)?;
            profile.push_str(&program_rule(program)?);
            if let Some(fd) = owned_terminal {
                profile.push_str(&owned_terminal_rule(fd)?);
            }
            append_denials(&mut profile, deny_read)?;
            let mut command = Command::new(SANDBOX_EXEC);
            command.arg("-p").arg(profile).arg(program).args(args);
            Ok(command)
        }
    }

    /// Like [`Sandbox::command`] but for an arbitrary [`Policy`].
    ///
    /// `Strict` behaves exactly like `command` (the caller sets the
    /// environment). `Dev` additionally owns the environment: it is cleared
    /// and rebuilt from this process's environment minus obvious secrets
    /// (see [`is_secret_env`]), with `TMPDIR` set to the workspace scratch
    /// directory (created here). The caller MUST NOT call `env_clear` on a Dev
    /// command; it may add variables and sets cwd and stdio.
    pub fn command_with_policy(
        &self,
        workspace: &Path,
        policy: &Policy,
        program: &str,
        args: &[&str],
    ) -> Result<Command, String> {
        self.dev_command(workspace, policy, program, args, std::env::vars_os())
    }

    /// Retain only the caller's freshly allocated private controlling PTY, never a host TTY.
    /// All filesystem, network, PID namespace and die-with-parent restrictions are unchanged.
    #[cfg(not(windows))]
    pub(super) fn command_with_owned_terminal(
        &self,
        workspace: &Path,
        policy: &Policy,
        program: &str,
        args: &[&str],
        terminal_fd: i32,
    ) -> Result<Command, String> {
        self.dev_command_mode(
            workspace,
            policy,
            program,
            args,
            std::env::vars_os(),
            Some(terminal_fd),
        )
    }

    fn dev_command(
        &self,
        workspace: &Path,
        policy: &Policy,
        program: &str,
        args: &[&str],
        env: impl Iterator<Item = (OsString, OsString)>,
    ) -> Result<Command, String> {
        self.dev_command_mode(workspace, policy, program, args, env, None)
    }

    fn dev_command_mode(
        &self,
        workspace: &Path,
        policy: &Policy,
        program: &str,
        args: &[&str],
        env: impl Iterator<Item = (OsString, OsString)>,
        owned_terminal: Option<i32>,
    ) -> Result<Command, String> {
        self.available()?;
        let (allow_network, deny_read, extra_write) = match policy {
            Policy::Strict { .. } => {
                return self.command_mode(workspace, policy, program, args, owned_terminal);
            }
            Policy::Dev {
                deny_read,
                extra_write,
                allow_network,
            } => (*allow_network, deny_read, extra_write.clone()),
        };
        #[cfg(not(windows))]
        let mut extra_write = extra_write;
        // Toolchains (uv, pip, npm, go) refuse to start when their home-directory cache is
        // read-only; give them a dial-owned cache instead of widening writes to the real one.
        #[cfg(not(windows))]
        let cache = toolchain_cache_root().filter(|root| std::fs::create_dir_all(root).is_ok());
        #[cfg(not(windows))]
        if let Some(root) = &cache
            && !extra_write.iter().any(|path| root.starts_with(path))
        {
            extra_write.push(root.clone());
        }
        #[cfg(target_os = "linux")]
        let policy = &Policy::Dev {
            deny_read: deny_read.clone(),
            extra_write: extra_write.clone(),
            allow_network,
        };
        let extra_write = &extra_write;
        #[cfg(windows)]
        {
            let _ = (allow_network, deny_read, extra_write, env);
            Err("Windows filesystem/network isolation is unavailable; refusing to run a sandboxed command".into())
        }
        #[cfg(not(windows))]
        {
            #[cfg(not(target_os = "linux"))]
            let mut profile = dev_profile(workspace, deny_read, extra_write, allow_network)?;
            #[cfg(not(target_os = "linux"))]
            profile.push_str(&program_rule(program)?);
            #[cfg(target_os = "linux")]
            let _ = (allow_network, deny_read, extra_write);
            let scratch = workspace.join(crate::workspace_copy::SCRATCH_DIR);
            std::fs::create_dir_all(&scratch)
                .map_err(|e| format!("creating scratch directory: {e}"))?;
            #[cfg(target_os = "linux")]
            let mut command =
                linux::command_mode(workspace, policy, program, args, owned_terminal.is_some())?;
            #[cfg(not(target_os = "linux"))]
            let mut command = {
                if let Some(fd) = owned_terminal {
                    profile.push_str(&owned_terminal_rule(fd)?);
                }
                append_denials(&mut profile, deny_read)?;
                let mut command = Command::new(SANDBOX_EXEC);
                command.arg("-p").arg(profile).arg(program).args(args);
                command
            };
            let mut env = scrubbed_env(env);
            if let Some(root) = &cache {
                let mut writable = vec![workspace.to_path_buf()];
                writable.extend(extra_write.iter().cloned());
                redirect_toolchain_caches(&mut env, root, &writable);
            }
            command.env_clear().envs(env).env("TMPDIR", &scratch);
            Ok(command)
        }
    }

    /// Self-test `policy` for `workspace`, `detect`-style: `Available` only if
    /// the real profile compiles and runs a trivial command *and* the
    /// mechanism enforces what the policy promises. `Dev` runs a synthetic
    /// enforcement probe in a temp dir (reads outside allowed, denied reads
    /// via symlink and `..` refused, writes confined, `extra_write` honoured,
    /// loopback network gated by the flag); the probe result is cached per
    /// network setting. `Strict` reuses [`Sandbox::detect`].
    #[cfg(test)]
    pub fn probe_policy(workspace: &Path, policy: &Policy) -> Sandbox {
        #[cfg(windows)]
        return Sandbox::Unavailable(
            "Windows command isolation is unavailable; sandboxed execution is disabled".to_string(),
        );
        #[cfg(target_os = "linux")]
        return linux::probe_policy(workspace, policy);
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            if let Err(reason) = platform_check() {
                return Sandbox::Unavailable(reason);
            }
            let (mechanism, real_profile) = match policy {
                Policy::Strict {
                    deny_read,
                    allow_network,
                    extra_write,
                } => {
                    if let Sandbox::Unavailable(reason) = Sandbox::detect() {
                        return Sandbox::Unavailable(reason);
                    }
                    (
                        Ok(()),
                        strict_profile(workspace, deny_read, extra_write, *allow_network),
                    )
                }
                Policy::Dev {
                    deny_read,
                    extra_write,
                    allow_network,
                } => {
                    static DEV_PROBES: [LazyLock<Result<(), String>>; 2] = [
                        LazyLock::new(|| dev_self_test(false)),
                        LazyLock::new(|| dev_self_test(true)),
                    ];
                    (
                        DEV_PROBES[usize::from(*allow_network)].clone(),
                        dev_profile(workspace, deny_read, extra_write, *allow_network),
                    )
                }
            };
            if let Err(reason) = mechanism {
                return Sandbox::Unavailable(format!("sandbox-exec self-test failed: {reason}"));
            }
            // The real profile must at least be valid and let a command start.
            let started = real_profile.and_then(|text| {
                let (code, _) = run_probe(&text, workspace, "exit 0", &[])?;
                if code == Some(0) {
                    Ok(())
                } else {
                    Err(format!("probe exited with {code:?}"))
                }
            });
            match started {
                Ok(()) => Sandbox::Available,
                Err(reason) => Sandbox::Unavailable(format!("policy cannot be enforced: {reason}")),
            }
        }
    }
}

/// Whether this process itself runs under a macOS sandbox (e.g. a dial `bash` command running dial's
/// own tests). `sandbox-exec` cannot be nested, so sandboxed tools are unavailable there.
#[cfg(target_os = "macos")]
pub fn already_sandboxed() -> bool {
    unsafe extern "C" {
        fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            filter: libc::c_int,
            ...
        ) -> libc::c_int;
    }
    // SAFETY: a null operation with SANDBOX_FILTER_NONE (0) only asks whether the process is sandboxed.
    unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) == 1 }
}
#[cfg(not(target_os = "macos"))]
pub fn already_sandboxed() -> bool {
    false
}

/// For tests that exercise the real sandbox: true when it works; false (skip, with a note) only when
/// this process is already sandboxed and nesting is impossible; otherwise the missing sandbox fails the test.
#[cfg(test)]
pub(crate) fn require_for_test() -> bool {
    match Sandbox::detect().available() {
        Ok(()) => true,
        Err(_) if already_sandboxed() => {
            eprintln!("skipping: already inside a sandbox, which macOS cannot nest");
            false
        }
        Err(reason) => panic!("sandbox-exec must work on this Mac: {reason}"),
    }
}

/// Grants the sandboxed program read and execute access to its own file, plus metadata for each
/// ancestor so the path resolves. Bare program names are resolved by the profile's system rules.
#[cfg(all(unix, not(target_os = "linux")))]
fn program_rule(program: &str) -> Result<String, String> {
    let path = Path::new(program);
    if !path.is_absolute() {
        return Ok(String::new());
    }
    let resolved = std::fs::canonicalize(path)
        .map_err(|e| format!("resolving sandboxed program {program}: {e}"))?;
    let mut out = format!(
        "(allow file-read* file-map-executable (literal {}))\n",
        quote(&resolved)?
    );
    let mut ancestors = Vec::new();
    for ancestor in resolved.ancestors().skip(1) {
        if ancestor.parent().is_none() {
            break;
        }
        ancestors.push(format!("(literal {})", quote(ancestor)?));
    }
    if !ancestors.is_empty() {
        out.push_str(&format!(
            "(allow file-read-metadata {})\n",
            ancestors.join(" ")
        ));
    }
    Ok(out)
}

fn quote(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))?;
    if !path.is_absolute() {
        return Err(format!("sandbox path must be absolute: {text}"));
    }
    if text
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return Err(format!(
            "sandbox path contains a quote, backslash or control character: {text:?}"
        ));
    }
    Ok(format!("\"{text}\""))
}

/// Grant data I/O only to the private PTY allocated by background, not other host terminals.
#[cfg(not(any(target_os = "linux", windows)))]
fn owned_terminal_rule(fd: i32) -> Result<String, String> {
    let mut name = [0u8; 1024];
    let result = unsafe { libc::ttyname_r(fd, name.as_mut_ptr().cast(), name.len()) };
    if result != 0 {
        return Err(format!(
            "resolving private PTY: {}",
            std::io::Error::from_raw_os_error(result)
        ));
    }
    let length = name
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("PTY path is too long")?;
    let path = std::str::from_utf8(&name[..length]).map_err(|_| "PTY path is not UTF-8")?;
    let suffix = path
        .strip_prefix("/dev/ttys")
        .ok_or("expected an owned macOS PTY device")?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected an owned macOS PTY device".into());
    }
    // /dev/tty resolves only to the private controlling terminal installed before sandbox exec.
    Ok(format!(
        "(allow file-read-data file-write-data (literal {} \"/dev/tty\"))\n",
        quote(Path::new(path))?
    ))
}

/// Generate the sandbox profile for a canonical workspace root plus the attached
/// `extra_write` roots, which get the same metadata, read and write access.
pub fn profile(
    workspace: &Path,
    extra_write: &[PathBuf],
    allow_network: bool,
) -> Result<String, String> {
    let roots: Vec<&Path> = std::iter::once(workspace)
        .chain(extra_write.iter().map(PathBuf::as_path))
        .collect();
    for root in &roots {
        if root.parent().is_none() {
            return Err("refusing to sandbox the filesystem root as a workspace".to_string());
        }
    }
    let mut out = String::from(
        "(version 1)\n(deny default)\n\
         (allow process-exec)\n(allow process-fork)\n\
         (allow signal (target same-sandbox))\n\
         (allow sysctl-read)\n(allow mach-lookup)\n",
    );
    // Metadata: system paths, every root, and the literal ancestors of each root
    // (needed for path resolution and getcwd, nothing else).
    out.push_str(
        "(allow file-read-metadata (literal \"/\") (literal \"/opt\") (literal \"/private\")",
    );
    for root in &roots {
        for ancestor in root.ancestors().skip(1) {
            if ancestor.parent().is_some() {
                out.push_str(&format!(" (literal {})", quote(ancestor)?));
            }
        }
    }
    for path in SYSTEM_READ {
        out.push_str(&format!(" (subpath {})", quote(Path::new(path))?));
    }
    for root in &roots {
        out.push_str(&format!(" (subpath {})", quote(root)?));
    }
    out.push_str(")\n");
    // Data reads: "/" itself (dyld/bash probe it), system paths, then the roots.
    out.push_str("(allow file-read* (literal \"/\")");
    for path in SYSTEM_READ {
        out.push_str(&format!(" (subpath {})", quote(Path::new(path))?));
    }
    out.push_str(")\n(allow file-write*");
    for device in WRITABLE_DEVICES {
        out.push_str(&format!(" (literal {})", quote(Path::new(device))?));
    }
    out.push_str(")\n");
    for root in &roots {
        out.push_str(&format!(
            "(allow file-read* file-write* (subpath {}))\n",
            quote(root)?
        ));
    }
    if allow_network {
        out.push_str("(allow network*)\n");
    }
    Ok(out)
}

/// Strict profile with protected state excluded even when nested below a writable root.
pub fn strict_profile(
    workspace: &Path,
    deny_read: &[PathBuf],
    extra_write: &[PathBuf],
    allow_network: bool,
) -> Result<String, String> {
    let workspace = resolve(workspace)?;
    for denied in deny_read {
        let denied = resolve(denied)?;
        if workspace.starts_with(&denied) {
            return Err(format!(
                "the workspace {} is inside a denied path {}",
                workspace.display(),
                denied.display()
            ));
        }
    }
    let mut out = profile(&workspace, extra_write, allow_network)?;
    append_denials(&mut out, deny_read)?;
    Ok(out)
}

/// Shared by both policies. Protect the path and prevent renaming/removing an ancestor
/// to move its contents outside the path-based denial. Ordinary sibling writes remain allowed.
fn append_denials(out: &mut String, deny_read: &[PathBuf]) -> Result<(), String> {
    for path in deny_read {
        let path = resolve(path)?;
        for ancestor in path.ancestors().skip(1) {
            out.push_str(&format!(
                "(deny file-write-unlink (literal {}))\n",
                quote(ancestor)?
            ));
        }
        out.push_str(&format!(
            "(deny file-read* file-write* (subpath {}))\n",
            quote(&path)?
        ));
    }
    Ok(())
}

/// Devices commands may write to under the Dev policy.
const DEV_WRITABLE_DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/tty",
    "/dev/dtracehelper",
    "/dev/ptmx",
];

/// Mach services denied under the Dev policy: clipboard and keychain access
/// (the rest of mach-lookup stays open; dyld, DNS and toolchains need it).
const DEV_DENIED_MACH: &[&str] = &[
    "com.apple.pasteboard.1",
    "com.apple.SecurityServer",
    "com.apple.securityd",
];

/// Resolve `path` to the real path the kernel will match against: symlinks and
/// `..` are resolved through the longest existing prefix; a not-yet-existing
/// tail is appended verbatim (it may not contain `..`). This is what makes a
/// rule given via a symlinked or `..` path enforceable, since sandbox rules
/// match resolved paths.
pub(crate) fn resolve(path: &Path) -> Result<PathBuf, String> {
    quote(path)?; // absolute, UTF-8, no quote/backslash/control characters
    let mut existing = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    let mut base = loop {
        match existing.canonicalize() {
            Ok(real) => break real,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| format!("cannot resolve sandbox path {}", path.display()))?
                    .to_owned();
                tail.push(name);
                existing.pop();
            }
            Err(e) => return Err(format!("resolving sandbox path {}: {e}", path.display())),
        }
    };
    base.extend(tail.iter().rev());
    quote(&base)?;
    Ok(base)
}

/// Generate the Dev sandbox profile. `workspace` must exist; `deny_read` and
/// `extra_write` entries are absolute paths (symlinks/`..` allowed; they are
/// resolved). Rule order matters — the last matching rule wins — so the
/// profile is: allow everything, deny all writes, re-allow the write set,
/// then deny the protected paths last.
pub fn dev_profile(
    workspace: &Path,
    deny_read: &[PathBuf],
    extra_write: &[PathBuf],
    allow_network: bool,
) -> Result<String, String> {
    let workspace = resolve(workspace)?;
    if workspace.parent().is_none() {
        return Err("refusing to sandbox the filesystem root as a workspace".to_string());
    }
    let denied = deny_read
        .iter()
        .map(|p| resolve(p))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(hit) = denied.iter().find(|d| workspace.starts_with(d)) {
        return Err(format!(
            "the workspace {} is inside a denied path {}",
            workspace.display(),
            hit.display()
        ));
    }
    let mut writable = vec![workspace];
    for path in extra_write {
        let real = resolve(path)?;
        if real.parent().is_none() {
            return Err("refusing to make the filesystem root writable".to_string());
        }
        writable.push(real);
    }

    let mut out =
        String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*");
    for path in &writable {
        out.push_str(&format!(" (subpath {})", quote(path)?));
    }
    for device in DEV_WRITABLE_DEVICES {
        out.push_str(&format!(" (literal {})", quote(Path::new(device))?));
    }
    // `/dev/fd/N` (behind /dev/stdout, /dev/stderr) reopens a descriptor the
    // process already holds; it grants nothing the descriptor didn't.
    out.push_str(" (subpath \"/dev/fd\") (regex #\"^/dev/ttys[0-9]+$\"))\n");
    if !allow_network {
        out.push_str("(deny network*)\n");
    }
    // Credential and escape channels that stay reachable even with network on:
    // the ssh-agent listener and the Docker daemon socket.
    out.push_str(
        "(deny network-outbound (remote unix-socket (path-regex \
         #\"^/private/(var/run|tmp)/com\\.apple\\.launchd\\.[^/]+/Listeners$\" \
         #\"(^|/)docker\\.sock$\")))\n",
    );
    out.push_str("(deny mach-lookup");
    for service in DEV_DENIED_MACH {
        out.push_str(&format!(" (global-name \"{service}\")"));
    }
    out.push_str(")\n");
    // Last, so they override the workspace and extra_write allowances.
    append_denials(&mut out, &denied)?;
    Ok(out)
}

/// True for environment variables that look like they carry credentials:
/// well-known secret prefixes, secret-ish name fragments or suffixes, and any
/// value holding URL userinfo (`scheme://user:password@host`).
pub fn is_secret_env(name: &str, value: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "AWS_",
        "AZURE_",
        "GCP_",
        "GCLOUD_",
        "GOOGLE_",
        "GITHUB_",
        "GITLAB_",
        "GH_",
        "ANTHROPIC_",
        "OPENAI_",
        "OPENROUTER_",
        "GEMINI_",
        "MISTRAL_",
        "GROQ_",
        "COHERE_",
        "HF_",
        "HUGGINGFACE",
        "DIAL_",
        "SSH_",
        "GPG_",
        "DOCKER_",
        "KUBE",
    ];
    const FRAGMENTS: &[&str] = &[
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "PRIVATE_KEY",
        "API_KEY",
        "APIKEY",
        "ACCESS_KEY",
    ];
    const SUFFIXES: &[&str] = &["_KEY", "_PASS", "_AUTH", "_CREDS", "_COOKIE"];
    let upper = name.to_ascii_uppercase();
    if PREFIXES.iter().any(|p| upper.starts_with(p))
        || FRAGMENTS.iter().any(|f| upper.contains(f))
        || SUFFIXES.iter().any(|s| upper.ends_with(s))
    {
        return true;
    }
    value.split_once("://").is_some_and(|(_, rest)| {
        let authority = rest.split('/').next().unwrap_or("");
        authority
            .rsplit_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
    })
}

/// Cache variables a Dev command gets pointed at [`toolchain_cache_root`] (the empty
/// subdirectory means the root itself), unless already set to a writable location.
#[cfg(not(windows))]
const TOOLCHAIN_CACHES: &[(&str, &str)] = &[
    ("XDG_CACHE_HOME", ""),
    ("UV_CACHE_DIR", "uv"),
    ("PIP_CACHE_DIR", "pip"),
    ("npm_config_cache", "npm"),
    ("GOCACHE", "go-build"),
];

/// A dial-owned cache that only sandboxed Dev commands use, so a cache poisoned inside the
/// sandbox never reaches the user's unsandboxed toolchains. Outside the dial state and user
/// dirs (which Dev denies) and outside the workspace (so it is not indexed or committed).
#[cfg(not(windows))]
fn toolchain_cache_root() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if !home.is_absolute() || home.parent().is_none() {
        return None;
    }
    // Not `$XDG_CACHE_HOME`: on Linux it could name a system tree bubblewrap refuses to make writable.
    #[cfg(target_os = "macos")]
    let base = home.join("Library/Caches");
    #[cfg(not(target_os = "macos"))]
    let base = home.join(".cache");
    Some(base.join("dial").join("sandbox"))
}

/// Point each cache variable in [`TOOLCHAIN_CACHES`] below `root` unless its current value
/// is already inside one of the `writable` roots.
#[cfg(not(windows))]
fn redirect_toolchain_caches(
    env: &mut Vec<(OsString, OsString)>,
    root: &Path,
    writable: &[PathBuf],
) {
    for (name, sub) in TOOLCHAIN_CACHES {
        let current = env.iter().position(|(key, _)| key == name);
        if let Some(index) = current {
            let value = Path::new(&env[index].1);
            if value.is_absolute() && writable.iter().any(|root| value.starts_with(root)) {
                continue;
            }
            env.remove(index);
        }
        let path = if sub.is_empty() {
            root.to_path_buf()
        } else {
            root.join(sub)
        };
        env.push(((*name).into(), path.into_os_string()));
    }
}

/// `env` minus secrets and non-UTF-8 entries; `PATH` is inherited (with a
/// system default when unset) so toolchains resolve, `HOME` is left as is.
#[cfg(not(windows))]
fn scrubbed_env(env: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let mut kept: Vec<(OsString, OsString)> = env
        .filter(|(name, value)| match (name.to_str(), value.to_str()) {
            (Some(name), Some(value)) => name != "TMPDIR" && !is_secret_env(name, value),
            _ => false,
        })
        .collect();
    if !kept.iter().any(|(name, _)| name == "PATH") {
        kept.push(("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()));
    }
    kept
}

#[cfg(all(test, not(any(target_os = "linux", windows))))]
fn platform_check() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("sandbox-exec exists only on macOS".to_string());
    }
    if !Path::new(SANDBOX_EXEC).exists() {
        return Err(format!("{SANDBOX_EXEC} not found"));
    }
    Ok(())
}

/// Enforcement probe for the Dev policy mechanism (synthetic paths).
#[cfg(all(test, not(any(target_os = "linux", windows))))]
fn dev_self_test(allow_network: bool) -> Result<(), String> {
    let scratch = tempfile::tempdir().map_err(|e| format!("creating scratch dir: {e}"))?;
    let base = scratch
        .path()
        .canonicalize()
        .map_err(|e| format!("canonicalizing scratch dir: {e}"))?;
    let io = |e: std::io::Error| e.to_string();
    let (workspace, extra, outside) = (base.join("ws"), base.join("extra"), base.join("outside"));
    let (denied_a, denied_b) = (base.join("denied-a"), base.join("denied-b"));
    for dir in [&workspace, &extra, &outside, &denied_a, &denied_b] {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let (canary_a, canary_b) = (denied_a.join("canary"), denied_b.join("canary"));
    std::fs::write(&canary_a, "secret").map_err(io)?;
    std::fs::write(&canary_b, "secret").map_err(io)?;
    // One denied dir is named through a symlink, the other through `..`.
    let link = base.join("link-to-denied-a");
    std::os::unix::fs::symlink(&denied_a, &link).map_err(io)?;
    std::os::unix::fs::symlink(&canary_a, workspace.join("sneak")).map_err(io)?;
    let policy_profile = dev_profile(
        &workspace,
        &[link, workspace.join("..").join("denied-b")],
        std::slice::from_ref(&extra),
        allow_network,
    )?;

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(io)?;
    let port = listener.local_addr().map_err(io)?.port().to_string();
    let text = |p: &Path| {
        p.to_str()
            .map(str::to_owned)
            .ok_or("scratch path is not UTF-8")
    };
    let args = [
        text(&canary_a)?,
        text(&canary_b)?,
        text(&outside)?,
        text(&extra)?,
        port,
        if allow_network { "1" } else { "0" }.to_string(),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let script = r#"
        /bin/cat /etc/hosts >/dev/null 2>&1 || exit 20
        echo ok > probe.txt 2>/dev/null || exit 10
        if /bin/cat "$1" >/dev/null 2>&1; then exit 12; fi
        if /bin/cat "$2" >/dev/null 2>&1; then exit 13; fi
        if /bin/cat sneak >/dev/null 2>&1; then exit 14; fi
        if echo x > "$3/f" 2>/dev/null; then exit 15; fi
        echo ok > "$4/f" 2>/dev/null || exit 16
        if (exec 3<>/dev/tcp/127.0.0.1/"$5") 2>/dev/null; then connected=1; else connected=0; fi
        [ "$connected" = "$6" ] || exit 17
        echo ok
    "#;
    let (code, stdout) = run_probe(&policy_profile, &workspace, script, &args)?;
    match code {
        Some(0) if stdout.trim() == "ok" => Ok(()),
        Some(20) => Err("reads outside the workspace are blocked".to_string()),
        Some(10) => Err("workspace is not writable".to_string()),
        Some(12 | 13) => Err("sandbox did not block reads of a denied path".to_string()),
        Some(14) => Err("sandbox did not block reads of a denied path via a symlink".to_string()),
        Some(15) => Err("sandbox did not block writes outside the workspace".to_string()),
        Some(16) => Err("extra_write path is not writable".to_string()),
        Some(17) => Err("sandbox did not enforce the network setting".to_string()),
        other => Err(format!("probe exited with {other:?}")),
    }
}

#[cfg(target_os = "linux")]
fn probe() -> Sandbox {
    linux::probe()
}

#[cfg(not(target_os = "linux"))]
fn probe() -> Sandbox {
    if !cfg!(target_os = "macos") {
        return Sandbox::Unavailable("sandbox-exec exists only on macOS".to_string());
    }
    if !Path::new(SANDBOX_EXEC).exists() {
        return Sandbox::Unavailable(format!("{SANDBOX_EXEC} not found"));
    }
    match self_test() {
        Ok(()) => Sandbox::Available,
        Err(reason) => Sandbox::Unavailable(format!("sandbox-exec self-test failed: {reason}")),
    }
}

fn self_test() -> Result<(), String> {
    let scratch = tempfile::tempdir().map_err(|e| format!("creating scratch dir: {e}"))?;
    let base = scratch
        .path()
        .canonicalize()
        .map_err(|e| format!("canonicalizing scratch dir: {e}"))?;
    let workspace = base.join("ws");
    let outside = base.join("outside");
    std::fs::create_dir_all(&workspace).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&outside).map_err(|e| e.to_string())?;
    let canary = outside.join("canary");
    std::fs::write(&canary, "secret").map_err(|e| e.to_string())?;
    let canary = canary.to_str().ok_or("scratch path is not UTF-8")?;

    // Writes inside must work and the canary must be unreadable. Exit 0 and
    // stdout "ok" only if both hold.
    let script = r#"
        echo ok > probe.txt || exit 10
        [ "$(/bin/cat probe.txt)" = ok ] || exit 11
        if /bin/cat "$1" >/dev/null 2>&1; then exit 12; fi
        echo ok
    "#;
    let (code, stdout) = run_probe(
        &profile(&workspace, &[], false)?,
        &workspace,
        script,
        &[canary],
    )?;
    match code {
        Some(0) if stdout.trim() == "ok" => Ok(()),
        Some(12) => Err("sandbox did not block reads outside the workspace".to_string()),
        other => Err(format!("probe exited with {other:?}")),
    }
}

/// Run `/bin/bash -c script _ args…` under `profile` in `cwd` with a minimal
/// environment, bounded to 15 seconds. Returns the exit code and stdout.
fn run_probe(
    profile: &str,
    cwd: &Path,
    script: &str,
    args: &[&str],
) -> Result<(Option<i32>, String), String> {
    let mut command = Command::new(SANDBOX_EXEC);
    command
        .arg("-p")
        .arg(profile)
        .args(["/bin/bash", "-c", script, "_"])
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    run_probe_command(command)
}

/// Run a fixed enforcement probe, never a model-supplied script.
fn run_probe_command(mut command: Command) -> Result<(Option<i32>, String), String> {
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawning sandbox enforcement probe: {e}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
            Ok(None) if started.elapsed() > Duration::from_secs(15) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("probe timed out".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        std::io::Read::read_to_string(&mut pipe, &mut stdout).map_err(|e| e.to_string())?;
    }
    Ok((status.code(), stdout))
}

#[cfg(all(test, unix))]
mod tests;
