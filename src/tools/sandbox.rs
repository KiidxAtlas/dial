//! macOS `sandbox-exec` confinement for the model-facing `bash` tool.
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
//! ([`is_secret_env`]), with `TMPDIR` pointing at the scratch dir.
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
    /// Deny-by-default: the system read set plus the workspace (see [`profile`]).
    Strict { allow_network: bool },
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
        self.available()?;
        let profile = profile(workspace, allow_network)?;
        let mut command = Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(profile).arg(program).args(args);
        Ok(command)
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

    fn dev_command(
        &self,
        workspace: &Path,
        policy: &Policy,
        program: &str,
        args: &[&str],
        env: impl Iterator<Item = (OsString, OsString)>,
    ) -> Result<Command, String> {
        self.available()?;
        let (allow_network, deny_read, extra_write) = match policy {
            Policy::Strict { allow_network } => {
                return self.command(workspace, *allow_network, program, args);
            }
            Policy::Dev {
                deny_read,
                extra_write,
                allow_network,
            } => (*allow_network, deny_read, extra_write),
        };
        let profile = dev_profile(workspace, deny_read, extra_write, allow_network)?;
        let scratch = workspace.join(crate::tools::SCRATCH_DIR);
        std::fs::create_dir_all(&scratch)
            .map_err(|e| format!("creating scratch directory: {e}"))?;
        let mut command = Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(profile).arg(program).args(args);
        command
            .env_clear()
            .envs(scrubbed_env(env))
            .env("TMPDIR", &scratch);
        Ok(command)
    }

    /// Self-test `policy` for `workspace`, `detect`-style: `Available` only if
    /// the real profile compiles and runs a trivial command *and* the
    /// mechanism enforces what the policy promises. `Dev` runs a synthetic
    /// enforcement probe in a temp dir (reads outside allowed, denied reads
    /// via symlink and `..` refused, writes confined, `extra_write` honoured,
    /// loopback network gated by the flag); the probe result is cached per
    /// network setting. `Strict` reuses [`Sandbox::detect`].
    pub fn probe_policy(workspace: &Path, policy: &Policy) -> Sandbox {
        if let Err(reason) = platform_check() {
            return Sandbox::Unavailable(reason);
        }
        let (mechanism, real_profile) = match policy {
            Policy::Strict { allow_network } => {
                if let Sandbox::Unavailable(reason) = Sandbox::detect() {
                    return Sandbox::Unavailable(reason);
                }
                (Ok(()), profile(workspace, *allow_network))
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

/// Generate the sandbox profile for a canonical workspace root.
pub fn profile(workspace: &Path, allow_network: bool) -> Result<String, String> {
    if workspace.parent().is_none() {
        return Err("refusing to sandbox the filesystem root as a workspace".to_string());
    }
    let root = quote(workspace)?;
    let mut out = String::from(
        "(version 1)\n(deny default)\n\
         (allow process-exec)\n(allow process-fork)\n\
         (allow signal (target same-sandbox))\n\
         (allow sysctl-read)\n(allow mach-lookup)\n",
    );
    // Metadata: system paths, the workspace, and the literal ancestors of the
    // workspace (needed for path resolution and getcwd, nothing else).
    out.push_str(
        "(allow file-read-metadata (literal \"/\") (literal \"/opt\") (literal \"/private\")",
    );
    for ancestor in workspace.ancestors().skip(1) {
        if ancestor.parent().is_some() {
            out.push_str(&format!(" (literal {})", quote(ancestor)?));
        }
    }
    for path in SYSTEM_READ {
        out.push_str(&format!(" (subpath {})", quote(Path::new(path))?));
    }
    out.push_str(&format!(" (subpath {root}))\n"));
    // Data reads: "/" itself (dyld/bash probe it), system paths, then workspace.
    out.push_str("(allow file-read* (literal \"/\")");
    for path in SYSTEM_READ {
        out.push_str(&format!(" (subpath {})", quote(Path::new(path))?));
    }
    out.push_str(")\n(allow file-write*");
    for device in WRITABLE_DEVICES {
        out.push_str(&format!(" (literal {})", quote(Path::new(device))?));
    }
    out.push_str(")\n");
    out.push_str(&format!(
        "(allow file-read* file-write* (subpath {root}))\n"
    ));
    if allow_network {
        out.push_str("(allow network*)\n");
    }
    Ok(out)
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
fn resolve(path: &Path) -> Result<PathBuf, String> {
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
    for path in &denied {
        out.push_str(&format!(
            "(deny file-read* file-write* (subpath {}))\n",
            quote(path)?
        ));
    }
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

/// `env` minus secrets and non-UTF-8 entries; `PATH` is inherited (with a
/// system default when unset) so toolchains resolve, `HOME` is left as is.
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
    let (code, stdout) = run_probe(&profile(&workspace, false)?, &workspace, script, &[canary])?;
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
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawning {SANDBOX_EXEC}: {e}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() > Duration::from_secs(15) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("probe timed out".to_string());
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        std::io::Read::read_to_string(&mut pipe, &mut stdout).map_err(|e| e.to_string())?;
    }
    Ok((status.code(), stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_rejects_unsafe_paths() {
        for bad in ["/tmp/a\"b", "/tmp/a\nb", "/tmp/a\\b", "relative/path", "/"] {
            assert!(profile(Path::new(bad), false).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn profile_scopes_access_to_workspace() {
        let text = profile(Path::new("/private/tmp/my ws"), false).unwrap();
        assert!(text.contains("(deny default)"));
        assert!(text.contains("(allow file-read* file-write* (subpath \"/private/tmp/my ws\"))"));
        assert!(!text.contains("network"));
        assert!(!text.contains("/Users"));
        assert!(
            profile(Path::new("/private/tmp/ws"), true)
                .unwrap()
                .contains("(allow network*)")
        );
    }

    #[test]
    fn unavailable_sandbox_refuses_to_build_commands() {
        let sandbox = Sandbox::Unavailable("nope".to_string());
        assert!(sandbox.available().unwrap_err().contains("nope"));
        assert!(
            sandbox
                .command(Path::new("/private/tmp/ws"), false, "/bin/echo", &[])
                .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn detect_works_on_macos() {
        if !require_for_test() {
            return;
        }
        assert_eq!(Sandbox::detect(), Sandbox::Available);
    }

    // ---- Dev policy -------------------------------------------------------

    struct Fixture {
        _dir: tempfile::TempDir,
        ws: PathBuf,
        denied: PathBuf,
        extra: PathBuf,
        outside: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().canonicalize().unwrap();
            let make = |name: &str| {
                let path = base.join(name);
                std::fs::create_dir_all(&path).unwrap();
                path
            };
            let fx = Fixture {
                ws: make("ws"),
                denied: make("denied"),
                extra: make("extra"),
                outside: make("outside"),
                _dir: dir,
            };
            std::fs::write(fx.denied.join("canary"), "secret").unwrap();
            std::fs::write(fx.outside.join("readable"), "visible").unwrap();
            fx
        }

        fn policy(&self, allow_network: bool) -> Policy {
            Policy::Dev {
                deny_read: vec![self.denied.clone()],
                extra_write: vec![self.extra.clone()],
                allow_network,
            }
        }

        /// Run `script` in bash under the Dev policy; returns (exit code, stdout+stderr).
        fn run(&self, policy: &Policy, script: &str) -> (Option<i32>, String) {
            self.run_with_env(policy, script, std::env::vars_os())
        }

        fn run_with_env(
            &self,
            policy: &Policy,
            script: &str,
            env: impl Iterator<Item = (OsString, OsString)>,
        ) -> (Option<i32>, String) {
            let mut command = Sandbox::Available
                .dev_command(&self.ws, policy, "/bin/bash", &["-c", script], env)
                .unwrap();
            command.current_dir(&self.ws).stdin(Stdio::null());
            let out = command.output().unwrap();
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.code(), text)
        }
    }

    #[test]
    fn dev_profile_orders_denials_last_and_rejects_unsafe_paths() {
        let fx = Fixture::new();
        let text = dev_profile(&fx.ws, &[fx.denied.clone()], &[fx.extra.clone()], false).unwrap();
        let allow = text.find("(allow file-write*").unwrap();
        let deny = text.find("(deny file-read* file-write* (subpath").unwrap();
        assert!(allow < deny, "denials must come after the write allowances");
        assert!(text.contains("(deny network*)"));
        assert!(
            !dev_profile(&fx.ws, &[], &[], true)
                .unwrap()
                .contains("(deny network*)")
        );
        for bad in [
            "/tmp/a\"b",
            "/tmp/a\nb",
            "/tmp/a\\b",
            "/tmp/a\0b",
            "relative/path",
        ] {
            assert!(
                dev_profile(&fx.ws, &[PathBuf::from(bad)], &[], false).is_err(),
                "deny {bad:?}"
            );
            assert!(
                dev_profile(&fx.ws, &[], &[PathBuf::from(bad)], false).is_err(),
                "write {bad:?}"
            );
        }
        assert!(dev_profile(&fx.ws, &[], &[PathBuf::from("/")], false).is_err());
        assert!(dev_profile(Path::new("/"), &[], &[], false).is_err());
        // A workspace inside a denied path (or equal to it) could never work.
        let parent = fx.ws.parent().unwrap().to_path_buf();
        assert!(dev_profile(&fx.ws, &[parent], &[], false).is_err());
        assert!(dev_profile(&fx.ws, &[fx.ws.clone()], &[], false).is_err());
    }

    #[test]
    fn resolve_follows_symlinks_and_dotdot_and_keeps_missing_tails() {
        let fx = Fixture::new();
        let link = fx.ws.join("link");
        std::os::unix::fs::symlink(&fx.denied, &link).unwrap();
        assert_eq!(resolve(&link).unwrap(), fx.denied);
        assert_eq!(
            resolve(&fx.ws.join("..").join("denied")).unwrap(),
            fx.denied
        );
        assert_eq!(
            resolve(&link.join("not").join("yet")).unwrap(),
            fx.denied.join("not").join("yet")
        );
        // `..` after a missing component cannot be resolved lexically.
        assert!(resolve(&fx.ws.join("missing").join("..").join("denied")).is_err());
    }

    #[test]
    fn is_secret_env_flags_credentials_and_keeps_toolchain_vars() {
        for secret in [
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "HF_TOKEN",
            "OPENAI_API_KEY",
            "ANTHROPIC_BASE_URL",
            "AWS_REGION",
            "AWS_PROFILE",
            "MY_SECRET",
            "DB_PASSWORD",
            "STRIPE_KEY",
            "npm_config__authToken",
            "DIAL_HOME",
            "SSH_AUTH_SOCK",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "api_key",
        ] {
            assert!(is_secret_env(secret, "v"), "{secret}");
        }
        for plain in [
            "PATH",
            "HOME",
            "USER",
            "LANG",
            "TERM",
            "SHELL",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "EDITOR",
            "NODE_OPTIONS",
            "PYENV_ROOT",
            "XDG_CONFIG_HOME",
            "DEVELOPER_DIR",
            "HOMEBREW_PREFIX",
            "PWD",
        ] {
            assert!(!is_secret_env(plain, "/some/path"), "{plain}");
        }
        assert!(is_secret_env(
            "HTTPS_PROXY",
            "https://user:hunter2@proxy.example:8080/"
        ));
        assert!(!is_secret_env("HTTPS_PROXY", "https://proxy.example:8080/"));
        assert!(!is_secret_env("REMOTE", "git@github.com:a/b"));
    }

    #[test]
    fn unavailable_sandbox_refuses_dev_commands() {
        let sandbox = Sandbox::Unavailable("nope".to_string());
        let policy = Policy::Dev {
            deny_read: vec![],
            extra_write: vec![],
            allow_network: false,
        };
        assert!(
            sandbox
                .command_with_policy(Path::new("/private/tmp"), &policy, "/bin/echo", &[])
                .is_err()
        );
        let strict = Policy::Strict {
            allow_network: false,
        };
        assert!(
            sandbox
                .command_with_policy(Path::new("/private/tmp/ws"), &strict, "/bin/echo", &[])
                .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dev_reads_widely_but_not_denied_paths() {
        if !require_for_test() {
            return;
        }
        let fx = Fixture::new();
        let policy = fx.policy(false);
        let link_to_denied = fx.ws.join("..").join("denied-link");
        std::os::unix::fs::symlink(&fx.denied, &link_to_denied).unwrap();
        std::os::unix::fs::symlink(fx.denied.join("canary"), fx.ws.join("sneak")).unwrap();
        let script = format!(
            r#"/bin/cat /etc/hosts >/dev/null || exit 20
               [ "$(/bin/cat {outside}/readable)" = visible ] || exit 21
               /bin/ls {outside} >/dev/null || exit 22
               if /bin/cat {denied}/canary 2>/dev/null; then exit 30; fi
               if /bin/cat {denied}/../denied/canary 2>/dev/null; then exit 31; fi
               if /bin/cat {link}/canary 2>/dev/null; then exit 32; fi
               if /bin/cat sneak 2>/dev/null; then exit 33; fi
               if /bin/ls {denied} 2>/dev/null; then exit 34; fi
               if /bin/ln {denied}/canary hardlink 2>/dev/null; then exit 35; fi
               exit 0"#,
            outside = fx.outside.display(),
            denied = fx.denied.display(),
            link = link_to_denied.display(),
        );
        let (code, out) = fx.run(&policy, &script);
        assert_eq!(code, Some(0), "{out}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dev_denied_path_given_via_symlink_or_dotdot_is_enforced() {
        let fx = Fixture::new();
        let link = fx.ws.join("alias");
        std::os::unix::fs::symlink(&fx.denied, &link).unwrap();
        for spelled in [link, fx.ws.join("..").join("denied")] {
            let policy = Policy::Dev {
                deny_read: vec![spelled.clone()],
                extra_write: vec![],
                allow_network: false,
            };
            let (code, out) = fx.run(&policy, &format!("/bin/cat {}/canary", fx.denied.display()));
            assert_ne!(code, Some(0), "{spelled:?}: {out}");
            assert!(
                out.contains("Operation not permitted"),
                "{spelled:?}: {out}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dev_confines_writes() {
        if !require_for_test() {
            return;
        }
        let fx = Fixture::new();
        let policy = fx.policy(false);
        let script = format!(
            r#"echo a > in-ws.txt || exit 10
               mkdir sub && echo b > sub/f || exit 11
               echo c > {extra}/f || exit 12
               t=$(/usr/bin/mktemp "$TMPDIR/x.XXXXXX") || exit 13
               case "$t" in {ws}/.tmp/*) ;; *) exit 14;; esac
               echo d > /dev/null || exit 15
               echo e > /dev/stderr || exit 16
               echo f 2>&1 > /dev/stdout || exit 17
               if echo x > {outside}/f 2>/dev/null; then exit 20; fi
               if /bin/mkdir {outside}/d 2>/dev/null; then exit 21; fi
               if /bin/rm {outside}/readable 2>/dev/null; then exit 22; fi
               if echo x > $HOME/.dial-sandbox-test 2>/dev/null; then exit 23; fi
               if echo x > /private/tmp/dial-sandbox-test 2>/dev/null; then exit 24; fi
               if /bin/ln -s {extra}/f {outside}/l 2>/dev/null; then exit 25; fi
               if echo x > {denied}/new 2>/dev/null; then exit 26; fi
               exit 0"#,
            extra = fx.extra.display(),
            ws = fx.ws.display(),
            outside = fx.outside.display(),
            denied = fx.denied.display(),
        );
        let (code, out) = fx.run(&policy, &script);
        assert_eq!(code, Some(0), "{out}");
        assert_eq!(
            std::fs::read_to_string(fx.ws.join("in-ws.txt")).unwrap(),
            "a\n"
        );
        assert_eq!(std::fs::read_to_string(fx.extra.join("f")).unwrap(), "c\n");
        assert!(!fx.outside.join("f").exists());
        assert!(!fx.denied.join("new").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dev_denial_beats_extra_write_and_workspace() {
        if !require_for_test() {
            return;
        }
        let fx = Fixture::new();
        // A denied dir nested inside the workspace and one inside an extra_write dir.
        let in_ws = fx.ws.join("private");
        let in_extra = fx.extra.join("private");
        for dir in [&in_ws, &in_extra] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("canary"), "secret").unwrap();
        }
        let policy = Policy::Dev {
            deny_read: vec![in_ws.clone(), in_extra.clone()],
            extra_write: vec![fx.extra.clone()],
            allow_network: false,
        };
        let script = format!(
            r#"echo ok > fine.txt || exit 10
               echo ok > {extra}/fine.txt || exit 11
               for d in {ws}/private {extra}/private; do
                 if /bin/cat $d/canary 2>/dev/null; then exit 20; fi
                 if echo x > $d/new 2>/dev/null; then exit 21; fi
               done
               exit 0"#,
            extra = fx.extra.display(),
            ws = fx.ws.display(),
        );
        let (code, out) = fx.run(&policy, &script);
        assert_eq!(code, Some(0), "{out}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dev_network_follows_flag() {
        if !require_for_test() {
            return;
        }
        let fx = Fixture::new();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = format!("(exec 3<>/dev/tcp/127.0.0.1/{port})");
        let (blocked, out) = fx.run(&fx.policy(false), &script);
        assert_ne!(blocked, Some(0), "network must be blocked: {out}");
        let (allowed, out) = fx.run(&fx.policy(true), &script);
        assert_eq!(allowed, Some(0), "network must be allowed: {out}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dev_scrubs_secret_environment() {
        if !require_for_test() {
            return;
        }
        let fx = Fixture::new();
        let env: Vec<(OsString, OsString)> = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/Users/someone"),
            ("KEEP_ME", "1"),
            ("TMPDIR", "/var/should/be/replaced"),
            ("GITHUB_TOKEN", "ghp_x"),
            ("HF_TOKEN", "hf_x"),
            ("OPENAI_API_KEY", "sk-x"),
            ("ANTHROPIC_API_KEY", "sk-ant-x"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("DB_PASSWORD", "pw"),
            ("PROXY_URL", "http://u:hunter2@host/"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let (code, out) = fx.run_with_env(&fx.policy(false), "/usr/bin/env", env.into_iter());
        assert_eq!(code, Some(0), "{out}");
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.contains(&"KEEP_ME=1"), "{out}");
        assert!(lines.contains(&"HOME=/Users/someone"), "{out}");
        assert!(lines.contains(&"PATH=/usr/bin:/bin"), "{out}");
        let tmp = format!("TMPDIR={}", fx.ws.join(crate::tools::SCRATCH_DIR).display());
        assert!(lines.contains(&tmp.as_str()), "{out}");
        for leaked in [
            "ghp_x",
            "hf_x",
            "sk-x",
            "sk-ant-x",
            "aws",
            "hunter2",
            "DB_PASSWORD",
            "should/be/replaced",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
        // An environment without PATH still gets a usable one.
        let (code, out) = fx.run_with_env(&fx.policy(false), "echo $PATH", std::iter::empty());
        assert_eq!(code, Some(0), "{out}");
        assert!(out.contains("/usr/bin"), "{out}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dev_runs_toolchains() {
        if !require_for_test() {
            return;
        }
        let fx = Fixture::new();
        let script = r#"
            set -e
            bash -c 'echo nested-bash' >/dev/null
            python3 -c 'import json,sys; print(json.dumps([1]))' >/dev/null
            git init -q repo
            cd repo
            echo hi > f.txt
            git add f.txt
            git -c user.name=t -c user.email=t@example.invalid commit -q -m first
            [ "$(git log --format=%s)" = first ]
            cd ..
            for tool in rustc cargo node npm; do
              if command -v "$tool" >/dev/null; then "$tool" --version >/dev/null; fi
            done
            printf 'fn main(){}' > m.rs && rustc m.rs -o m 2>/dev/null || true
        "#;
        let (code, out) = fx.run(&fx.policy(false), script);
        assert_eq!(code, Some(0), "{out}");
        assert!(fx.ws.join("repo/.git").is_dir());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dev_timeout_kills_the_whole_process_group() {
        if !require_for_test() {
            return;
        }
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        let fx = Fixture::new();
        let marker = fx.ws.join("grandchild-ran");
        let script = format!("(sleep 30; echo late > {}) & wait", marker.display());
        let mut command = Sandbox::Available
            .dev_command(
                &fx.ws,
                &fx.policy(false),
                "/bin/bash",
                &["-c", &script],
                std::env::vars_os(),
            )
            .unwrap();
        command
            .current_dir(&fx.ws)
            .stdin(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            child.try_wait().unwrap().is_none(),
            "command should still be running"
        );
        let pgid = i32::try_from(child.id()).unwrap();
        assert_eq!(unsafe { libc::killpg(pgid, libc::SIGKILL) }, 0);
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(!marker.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn probe_policy_reports_enforceable_policies_and_rejects_broken_ones() {
        if !require_for_test() {
            return;
        }
        let fx = Fixture::new();
        for allow_network in [false, true] {
            assert_eq!(
                Sandbox::probe_policy(&fx.ws, &fx.policy(allow_network)),
                Sandbox::Available,
                "dev network={allow_network}"
            );
            assert_eq!(
                Sandbox::probe_policy(&fx.ws, &Policy::Strict { allow_network }),
                Sandbox::Available,
                "strict network={allow_network}"
            );
        }
        // A policy that cannot be applied to this workspace fails closed.
        let broken = Policy::Dev {
            deny_read: vec![fx.ws.parent().unwrap().to_path_buf()],
            extra_write: vec![],
            allow_network: false,
        };
        match Sandbox::probe_policy(&fx.ws, &broken) {
            Sandbox::Unavailable(reason) => {
                assert!(reason.contains("inside a denied path"), "{reason}")
            }
            Sandbox::Available => panic!("broken policy reported available"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn strict_policy_through_command_with_policy_matches_command() {
        let fx = Fixture::new();
        let target = fx.outside.join("readable");
        let mut command = Sandbox::Available
            .command_with_policy(
                &fx.ws,
                &Policy::Strict {
                    allow_network: false,
                },
                "/bin/cat",
                &[target.to_str().unwrap()],
            )
            .unwrap();
        let out = command.current_dir(&fx.ws).output().unwrap();
        assert!(
            !out.status.success(),
            "strict policy must not read outside the workspace"
        );
    }
}
