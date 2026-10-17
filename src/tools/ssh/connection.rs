//! Shared OpenSSH connections: ControlMaster multiplexing under Dial's user directory, strictly
//! non-interactive (`BatchMode=yes`, no terminal), plus the cached remote OS/shell probe that
//! gates `ssh://` transfers and Windows compatibility wrapping.
//!
//! The control sockets live in `<DIAL_HOME>/ssh/cm`, which the bash sandbox can neither read nor
//! connect to, so sandboxed commands cannot ride an authenticated master. When that path would
//! overflow `sun_path`, a per-user `/tmp/dial-ssh-<digest>` directory is used instead and must be
//! owner-private (0700, owned by us, not a symlink) on every use.

use super::Settings;
use super::process::{self, Finished};
use super::url::{check_destination_part, sh_quote};
use serde::{Deserialize, Serialize};
#[cfg(any(unix, test))]
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::sync::Once;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
#[cfg(unix)]
static ONCE: Once = Once::new();

/// Socket file name; `%C` is OpenSSH's 40-hex digest of the connection. The sandbox matches the
/// `dial-cm-` prefix to refuse connections to these sockets.
pub const SOCKET_PREFIX: &str = "dial-cm-";
/// Bytes `dial-cm-%C` expands to.
const SOCKET_NAME_BYTES: usize = SOCKET_PREFIX.len() + 40;
/// `muxserver_listen` binds `<path>.<16 random chars>` before renaming it into place.
const MUX_TEMP_SUFFIX_BYTES: usize = 1 + 16;
/// Pre-command helpers (check/start/probe) never wait longer than this.
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
/// A verified master is trusted this long before `ssh -O check` runs again.
const VERIFY_TTL: Duration = Duration::from_secs(30);
const HOST_INFO_VERSION: u32 = 1;
const HOST_PROBE_MARKER: &str = "DIAL_HOST_PROBE=";
const TRANSFER_PROBE_MARKER: &str = "DIAL_TRANSFER_OK|";

/// A host to connect to. `name` keys the caches; `host` is what OpenSSH receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub key: Option<PathBuf>,
    pub compat: Option<bool>,
}

impl Target {
    /// An OpenSSH destination resolved by OpenSSH itself (an alias or hostname).
    pub fn opaque(name: &str) -> Self {
        Target {
            name: name.to_owned(),
            host: name.to_owned(),
            user: None,
            port: None,
            key: None,
            compat: None,
        }
    }

    /// `[user@]host`, refusing anything OpenSSH would parse as an option.
    pub fn destination(&self) -> Result<String, String> {
        check_destination_part("host", &self.host)?;
        match &self.user {
            Some(user) => {
                check_destination_part("username", user)?;
                Ok(format!("{user}@{}", self.host))
            }
            None => Ok(self.host.clone()),
        }
    }

    fn fingerprint(&self) -> String {
        format!(
            "{}@{}:{}\0{}",
            self.user.as_deref().unwrap_or(""),
            self.host,
            self.port.map(|p| p.to_string()).unwrap_or_default(),
            self.key
                .as_deref()
                .map(|k| k.display().to_string())
                .unwrap_or_default()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Windows,
    Linux,
    Macos,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Cmd,
    Powershell,
    Bash,
    Zsh,
    Sh,
    Unknown,
}

/// A POSIX shell verified to run Dial's transfer snippets (or a Windows compat layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PosixShell {
    Sh,
    Bash,
    Zsh,
}

impl PosixShell {
    pub fn name(self) -> &'static str {
        match self {
            PosixShell::Sh => "sh",
            PosixShell::Bash => "bash",
            PosixShell::Zsh => "zsh",
        }
    }

    /// `<shell> -c '<command>'`: parse under this shell, not the remote login shell (which may
    /// be fish, csh or cmd and cannot read `if [ … ]; then`).
    pub fn wrap(self, command: &str) -> String {
        format!("{} -c {}", self.name(), sh_quote(command))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub version: u32,
    pub os: Os,
    pub shell: Shell,
    #[serde(default)]
    pub transfer_shell: Option<PosixShell>,
    #[serde(default)]
    pub compat_shell: Option<PosixShell>,
    #[serde(default)]
    pub compat_enabled: bool,
}

impl HostInfo {
    /// `linux/bash`, `windows/cmd`, … for host listings.
    pub fn label(&self) -> String {
        let os = match self.os {
            Os::Windows => "windows",
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Unknown => "unknown",
        };
        let shell = match (self.os, self.compat_enabled, self.compat_shell) {
            (Os::Windows, true, Some(compat)) => compat.name(),
            _ => match self.shell {
                Shell::Cmd => "cmd",
                Shell::Powershell => "powershell",
                Shell::Bash => "bash",
                Shell::Zsh => "zsh",
                Shell::Sh => "sh",
                Shell::Unknown => "unknown",
            },
        };
        format!("{os}/{shell}")
    }
}

#[derive(Default)]
struct State {
    active: HashMap<String, Target>,
    verified: HashMap<String, (Instant, String)>,
    info: HashMap<String, HostInfo>,
    locks: HashMap<String, Arc<Mutex<()>>>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// ControlMaster needs Unix-domain sockets; Windows OpenSSH has none.
pub fn supports_control_master() -> bool {
    cfg!(unix)
}

/// Whether `dir/dial-cm-<40 hex>` plus OpenSSH's temporary bind suffix fits `sun_path`
/// (104 bytes on macOS, 108 elsewhere; OpenSSH rejects lengths that reach the limit).
pub fn control_path_fits(dir: &Path, macos: bool) -> bool {
    let limit = if macos { 104 } else { 108 };
    dir.as_os_str().len() + 1 + SOCKET_NAME_BYTES + MUX_TEMP_SUFFIX_BYTES < limit
}

/// Short per-user fallback for control directories that overflow `sun_path`.
#[cfg(any(unix, test))]
pub fn fallback_control_dir(canonical: &Path, uid: u32) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(uid.to_string());
    hash.update([0]);
    hash.update(canonical.as_os_str().as_encoded_bytes());
    let digest = format!("{:x}", hash.finalize());
    PathBuf::from("/tmp").join(format!("dial-ssh-{}", &digest[..20]))
}

/// The chosen control directory and whether it is the shared-`/tmp` fallback.
pub fn control_dir(settings: &Settings) -> (PathBuf, bool) {
    let canonical = settings.user_dir.join("ssh").join("cm");
    if control_path_fits(&canonical, cfg!(target_os = "macos")) {
        return (canonical, false);
    }
    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        (fallback_control_dir(&canonical, uid), true)
    }
    #[cfg(not(unix))]
    {
        (canonical, false)
    }
}

/// Create the control directory (0700) and verify it is ours before any socket goes there.
pub fn ensure_control_dir(settings: &Settings) -> Result<PathBuf, String> {
    let (dir, shared) = control_dir(settings);
    create_private_dir(&dir)
        .map_err(|e| format!("creating SSH control directory {}: {e}", dir.display()))?;
    if shared {
        check_owner_private(&dir)?;
    }
    Ok(dir)
}

/// `mkdir -p` with 0700 on the leaf, then tighten an existing leaf to 0700.
pub(super) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let meta = fs::symlink_metadata(dir)?;
        if meta.is_dir() && !meta.file_type().is_symlink() && meta.permissions().mode() & 0o077 != 0
        {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// A directory in a shared location must be a real directory we own with mode 0700.
fn check_owner_private(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta =
            fs::symlink_metadata(dir).map_err(|e| format!("inspecting {}: {e}", dir.display()))?;
        let uid = unsafe { libc::getuid() };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(format!(
                "SSH control directory {} is not a real directory; remove it",
                dir.display()
            ));
        }
        if meta.uid() != uid {
            return Err(format!(
                "SSH control directory {} is owned by another user; remove it",
                dir.display()
            ));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(format!(
                "SSH control directory {} is accessible to other users; chmod 700 it",
                dir.display()
            ));
        }
    }
    let _ = dir;
    Ok(())
}

/// A `-o Key=value` value: `%` escaped for OpenSSH tokens when `tokens`, quoted when it has spaces.
pub(super) fn option_value(value: &str, tokens: bool) -> String {
    let value = if tokens {
        value.replace('%', "%%")
    } else {
        value.to_owned()
    };
    if value.contains(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value
    }
}

/// `ControlPath=<dir>/dial-cm-%C`.
pub fn control_path_option(dir: &Path) -> String {
    let path = format!(
        "{}/{SOCKET_PREFIX}",
        option_value(&dir.display().to_string(), true).trim_matches('"')
    );
    let full = format!("{path}%C");
    if full.contains(char::is_whitespace) {
        format!("ControlPath=\"{full}\"")
    } else {
        format!("ControlPath={full}")
    }
}

/// Options every ssh invocation shares. `-n` keeps stdin closed unless the caller streams data.
pub fn common_args(
    target: &Target,
    settings: &Settings,
    control: Option<&Path>,
    allow_stdin: bool,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if !allow_stdin {
        args.push("-n".into());
    }
    if let Some(dir) = control {
        args.extend([
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            control_path_option(dir),
            "-o".into(),
            format!("ControlPersist={}", settings.config.control_persist_secs),
        ]);
    }
    args.extend(no_prompt_options(settings));
    if let Some(port) = target.port {
        args.extend(["-p".into(), port.to_string()]);
    }
    if let Some(key) = &target.key {
        args.extend(["-i".into(), key.display().to_string()]);
    }
    args
}

/// Options that make OpenSSH fail instead of prompting or hanging.
pub(super) fn no_prompt_options(settings: &Settings) -> Vec<String> {
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!(
            "StrictHostKeyChecking={}",
            settings.config.host_key_checking.as_option()
        ),
        "-o".into(),
        format!(
            "ConnectTimeout={}",
            settings.config.connect_timeout_secs.max(1)
        ),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ]
}

/// The complete argv (after `ssh`) that runs `command` on `target` over the shared master.
pub fn remote_args(
    target: &Target,
    settings: &Settings,
    command: &str,
    allow_stdin: bool,
) -> Result<Vec<String>, String> {
    validate_key(target.key.as_deref())?;
    let control = control_for(settings)?;
    let mut args = common_args(target, settings, control.as_deref(), allow_stdin);
    args.push(target.destination()?);
    args.push(command.to_owned());
    Ok(args)
}

fn control_for(settings: &Settings) -> Result<Option<PathBuf>, String> {
    if supports_control_master() {
        ensure_control_dir(settings).map(Some)
    } else {
        Ok(None)
    }
}

/// `ssh` with an environment that cannot fall back to graphical password prompts.
pub fn ssh_command(args: &[String]) -> Command {
    let mut command = Command::new("ssh");
    command.args(args).env("SSH_ASKPASS_REQUIRE", "never");
    command
}

fn validate_key(key: Option<&Path>) -> Result<(), String> {
    let Some(key) = key else { return Ok(()) };
    let meta = fs::metadata(key).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("SSH key not found: {}", key.display()),
        _ => format!("SSH key {}: {e}", key.display()),
    })?;
    if !meta.is_file() {
        return Err(format!("SSH key is not a file: {}", key.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "SSH key permissions must be 600 or stricter: {}",
                key.display()
            ));
        }
    }
    Ok(())
}

pub(super) fn ssh_available() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("ssh").is_file())
}
fn run_helper(args: Vec<String>, cancel: &AtomicBool) -> Result<Finished, String> {
    process::run(
        ssh_command(&args),
        None,
        HELPER_TIMEOUT,
        256 * 1024,
        cancel,
        &mut |_, _| {},
    )
}

fn host_lock(name: &str) -> Arc<Mutex<()>> {
    state().locks.entry(name.to_owned()).or_default().clone()
}

/// Make sure a master connection to `target` is up (starting one if needed).
pub fn ensure_connection(
    target: &Target,
    settings: &Settings,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let lock = host_lock(&target.name);
    let _serial = lock.lock().unwrap_or_else(|e| e.into_inner());
    let fingerprint = target.fingerprint();
    {
        let state = state();
        if let Some((at, print)) = state.verified.get(&target.name)
            && at.elapsed() < VERIFY_TTL
            && *print == fingerprint
            && state.active.contains_key(&target.name)
        {
            drop(state);
            // The socket directory can be replaced after verification; re-check it every time.
            control_for(settings)?;
            return Ok(());
        }
    }
    if !ssh_available() {
        return Err("ssh binary not found on PATH; install OpenSSH client".to_owned());
    }
    let control = control_for(settings)?;
    validate_key(target.key.as_deref())?;
    register_cleanup();
    let destination = target.destination()?;
    if let Some(dir) = control.as_deref() {
        let mut check = vec!["-O".to_owned(), "check".to_owned()];
        check.extend(common_args(target, settings, Some(dir), false));
        check.push(destination.clone());
        let alive = run_helper(check, cancel)?;
        if alive.cancelled {
            return Err("ssh: cancelled".to_owned());
        }
        if !alive.success() {
            let mut start = vec!["-M".to_owned(), "-N".to_owned(), "-f".to_owned()];
            start.extend(common_args(target, settings, Some(dir), false));
            start.push(destination.clone());
            let started = run_helper(start, cancel)?;
            if !started.success() {
                return Err(format!(
                    "{}\nHint: connections are non-interactive (BatchMode): key or agent authentication must work without prompts. Run `ssh {destination}` once in a terminal to check access{}.",
                    started.failure(
                        &format!("failed to connect to {destination}"),
                        HELPER_TIMEOUT
                    ),
                    if settings.config.host_key_checking == super::HostKeyChecking::Yes {
                        " and accept the host key"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    let mut state = state();
    state.active.insert(target.name.clone(), target.clone());
    state
        .verified
        .insert(target.name.clone(), (Instant::now(), fingerprint));
    Ok(())
}

/// Run `command` on the remote login shell with the shared master; capture up to `cap` stdout bytes.
pub fn capture(
    target: &Target,
    settings: &Settings,
    command: &str,
    timeout: Duration,
    cap: usize,
    cancel: &AtomicBool,
) -> Result<Finished, String> {
    let args = remote_args(target, settings, command, false)?;
    process::run(
        ssh_command(&args),
        None,
        timeout,
        cap,
        cancel,
        &mut |_, _| {},
    )
}

/// Cached OS/shell information, probing (and connecting) only when nothing usable is cached.
pub fn host_info(
    target: &Target,
    settings: &Settings,
    cancel: &AtomicBool,
) -> Result<HostInfo, String> {
    if let Some(info) = cached_info(target, settings)
        && !should_refresh(target, &info)
    {
        return Ok(info);
    }
    ensure_connection(target, settings, cancel)?;
    let info = probe(target, settings, cancel)?;
    state().info.insert(target.name.clone(), info.clone());
    persist_info(settings, &target.name, &info);
    Ok(info)
}

/// Host info from memory or disk without touching the network.
pub fn cached_info(target: &Target, settings: &Settings) -> Option<HostInfo> {
    if let Some(info) = state().info.get(&target.name) {
        return Some(apply_compat(target, info.clone()));
    }
    let text = fs::read_to_string(info_path(settings, &target.name)).ok()?;
    let info: HostInfo = serde_json::from_str(&text).ok()?;
    let info = apply_compat(target, info);
    state().info.insert(target.name.clone(), info.clone());
    Some(info)
}

fn info_path(settings: &Settings, name: &str) -> PathBuf {
    settings
        .user_dir
        .join("ssh")
        .join("hosts")
        .join(format!("{}.json", sanitize_host_name(name)))
}

fn sanitize_host_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn persist_info(settings: &Settings, name: &str, info: &HostInfo) {
    let path = info_path(settings, name);
    let Some(dir) = path.parent() else { return };
    if create_private_dir(dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(info) {
        // Synced, uniquely named temp file and rename: concurrent sessions probing the same host
        // never interleave writes into one shared temp path.
        let _ = crate::storage::settings_store::write_private(&path, text.as_bytes());
    }
}

fn forget_info(settings: &Settings, name: &str) {
    state().info.remove(name);
    let _ = fs::remove_file(info_path(settings, name));
}

/// Re-probe stale or ambiguous classifications instead of trusting them.
pub fn should_refresh(target: &Target, info: &HostInfo) -> bool {
    info.version != HOST_INFO_VERSION
        || info.os == Os::Unknown
        || (info.os != Os::Windows && info.compat_enabled)
        || (info.os == Os::Windows && info.compat_enabled && info.compat_shell.is_none())
        || (target.compat == Some(true) && info.os == Os::Windows && info.compat_shell.is_none())
        || (info.os != Os::Windows && info.transfer_shell.is_none())
}

/// Windows compat (run commands under the detected bash/sh) unless the host disables it.
pub fn apply_compat(target: &Target, mut info: HostInfo) -> HostInfo {
    if info.compat_shell.is_none() && info.os == Os::Windows {
        info.compat_shell = match info.shell {
            Shell::Bash => Some(PosixShell::Bash),
            Shell::Sh => Some(PosixShell::Sh),
            _ => None,
        };
    }
    info.compat_enabled =
        target.compat != Some(false) && info.os == Os::Windows && info.compat_shell.is_some();
    info
}

/// Classify a login-shell path or name. fish/csh/tcsh are not POSIX and stay unknown.
pub fn parse_shell(value: &str) -> Shell {
    let lower = value.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return Shell::Unknown;
    }
    if lower.contains("bash") {
        return Shell::Bash;
    }
    if lower.contains("zsh") {
        return Shell::Zsh;
    }
    if lower.contains("pwsh") || lower.contains("powershell") {
        return Shell::Powershell;
    }
    if lower.contains("cmd.exe") || lower == "cmd" {
        return Shell::Cmd;
    }
    let base = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    match base {
        "sh" | "dash" | "ash" | "ksh" | "mksh" => Shell::Sh,
        _ => Shell::Unknown,
    }
}

/// Classify `uname -s` output.
pub fn os_from_uname(value: &str) -> Option<Os> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("darwin") {
        Some(Os::Macos)
    } else if lower.contains("linux") || lower.contains("gnu") {
        Some(Os::Linux)
    } else if ["mingw", "msys", "cygwin", "windows"]
        .iter()
        .any(|w| lower.contains(w))
    {
        Some(Os::Windows)
    } else {
        None
    }
}

/// The payload after the first line starting with `marker`, so login banners and noisy dotfiles
/// cannot corrupt classification. stdout first, then stderr (some dotfiles swap them).
pub fn extract_probe_payload(stdout: &str, stderr: &str, marker: &str) -> Option<String> {
    [stdout, stderr]
        .into_iter()
        .flat_map(str::lines)
        .map(str::trim)
        .find_map(|line| line.strip_prefix(marker).map(str::to_owned))
}

/// Everything after `marker` anywhere in stdout, else stderr.
pub fn find_probe_marker(stdout: &str, stderr: &str, marker: &str) -> Option<String> {
    [stdout, stderr].into_iter().find_map(|blob| {
        blob.find(marker)
            .map(|i| blob[i + marker.len()..].to_owned())
    })
}

/// OS, login shell and whether bash is present, from `$OSTYPE|$SHELL|$BASH_VERSION`.
pub fn classify_probe(payload: &str) -> (Os, Shell, bool) {
    let mut parts = payload.split('|');
    let os_raw = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let shell_raw = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let bash_version = parts.next().unwrap_or("").trim();
    let payload_lower = payload.to_ascii_lowercase();
    let unexpanded = ["$OSTYPE", "$SHELL", "$BASH_VERSION"]
        .iter()
        .any(|v| payload.contains(v));
    let windows = ["windows", "msys", "cygwin", "mingw"]
        .iter()
        .any(|w| os_raw.contains(w))
        || payload_lower.contains("windows_nt")
        || payload_lower.contains("comspec")
        || shell_raw.contains("cmd")
        || shell_raw.contains("powershell")
        || unexpanded
        || payload.contains("%OS%");
    let os = if windows {
        Os::Windows
    } else if os_raw.contains("darwin") {
        Os::Macos
    } else if os_raw.contains("linux") || os_raw.contains("gnu") {
        Os::Linux
    } else {
        Os::Unknown
    };
    let mut shell = parse_shell(&shell_raw);
    if shell == Shell::Unknown && os == Os::Windows && shell_raw.is_empty() {
        shell = Shell::Cmd;
    }
    let has_bash = !unexpanded && (!bash_version.is_empty() || shell == Shell::Bash);
    (os, shell, has_bash)
}

fn probe(target: &Target, settings: &Settings, cancel: &AtomicBool) -> Result<HostInfo, String> {
    let command = format!(
        "echo \"{HOST_PROBE_MARKER}$OSTYPE|$SHELL|$BASH_VERSION\" 2>/dev/null || echo \"{HOST_PROBE_MARKER}%OS%|%COMSPEC%|\""
    );
    let result = capture(
        target,
        settings,
        &command,
        HELPER_TIMEOUT,
        64 * 1024,
        cancel,
    )?;
    if result.cancelled {
        return Err("ssh: cancelled".to_owned());
    }
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    let Some(payload) = extract_probe_payload(&stdout, &stderr, HOST_PROBE_MARKER) else {
        if !result.success() && result.code() == Some(255) {
            return Err(result.failure(&format!("probing {}", target.name), HELPER_TIMEOUT));
        }
        let (transfer, uname) = probe_transfer_shell(target, settings, cancel)?;
        return Ok(apply_compat(
            target,
            HostInfo {
                version: HOST_INFO_VERSION,
                os: transfer
                    .and_then(|_| os_from_uname(&uname))
                    .unwrap_or(Os::Unknown),
                shell: Shell::Unknown,
                transfer_shell: transfer,
                compat_shell: None,
                compat_enabled: false,
            },
        ));
    };
    let (mut os, shell, has_bash) = classify_probe(&payload);
    let mut transfer_shell = None;
    if os != Os::Windows {
        let (shell, uname) = probe_transfer_shell(target, settings, cancel)?;
        transfer_shell = shell;
        if transfer_shell.is_some() && os == Os::Unknown {
            os = os_from_uname(&uname).unwrap_or(os);
        }
    }
    let mut compat_shell = None;
    if os == Os::Windows && target.compat != Some(false) {
        for (candidate, marker) in [
            (PosixShell::Bash, "DIAL_BASH_OK"),
            (PosixShell::Sh, "DIAL_SH_OK"),
        ] {
            let check = capture(
                target,
                settings,
                &format!("{} -lc \"echo {marker}\"", candidate.name()),
                HELPER_TIMEOUT,
                64 * 1024,
                cancel,
            )?;
            if check.success() && String::from_utf8_lossy(&check.stdout).contains(marker) {
                compat_shell = Some(candidate);
                break;
            }
        }
    } else if os == Os::Windows && has_bash {
        compat_shell = Some(PosixShell::Bash);
    } else if os == Os::Windows && shell == Shell::Sh {
        compat_shell = Some(PosixShell::Sh);
    }
    Ok(apply_compat(
        target,
        HostInfo {
            version: HOST_INFO_VERSION,
            os,
            shell,
            transfer_shell,
            compat_shell,
            compat_enabled: false,
        },
    ))
}

/// The first of sh/bash/zsh that round-trips a marker, and the `uname -s` it printed.
fn probe_transfer_shell(
    target: &Target,
    settings: &Settings,
    cancel: &AtomicBool,
) -> Result<(Option<PosixShell>, String), String> {
    for candidate in [PosixShell::Sh, PosixShell::Bash, PosixShell::Zsh] {
        let command = format!(
            "{} -lc 'printf \"{TRANSFER_PROBE_MARKER}\"; uname -s 2>/dev/null || true'",
            candidate.name()
        );
        let probe = capture(
            target,
            settings,
            &command,
            HELPER_TIMEOUT,
            64 * 1024,
            cancel,
        )?;
        if probe.cancelled {
            return Err("ssh: cancelled".to_owned());
        }
        if !probe.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&probe.stdout);
        let stderr = String::from_utf8_lossy(&probe.stderr);
        if let Some(tail) = find_probe_marker(&stdout, &stderr, TRANSFER_PROBE_MARKER) {
            return Ok((Some(candidate), tail.trim().to_owned()));
        }
    }
    Ok((None, String::new()))
}

/// Stop the master for `name` (if this process knows it) and forget cached host metadata.
pub fn close(settings: &Settings, target: &Target) -> Result<bool, String> {
    let known = {
        let mut state = state();
        state.verified.remove(&target.name);
        state.active.remove(&target.name)
    };
    forget_info(settings, &target.name);
    let target = known.unwrap_or_else(|| target.clone());
    if !supports_control_master() {
        return Ok(false);
    }
    let dir = ensure_control_dir(settings)?;
    let mut args = vec!["-O".to_owned(), "exit".to_owned()];
    args.extend(common_args(&target, settings, Some(&dir), false));
    args.push(target.destination()?);
    let done = process::run(
        ssh_command(&args),
        None,
        Duration::from_secs(5),
        4096,
        &AtomicBool::new(false),
        &mut |_, _| {},
    )?;
    Ok(done.success())
}

/// Masters this process opened (or verified), by name.
pub fn active() -> Vec<String> {
    let mut names: Vec<String> = state().active.keys().cloned().collect();
    names.sort();
    names
}

/// Close every master this process opened. Runs at exit (Unix `atexit`); never blocks on a held lock.
#[cfg(unix)]
pub fn close_all(settings: &Settings) {
    let targets: Vec<Target> = match STATE.try_lock() {
        Ok(mut state) => {
            state.verified.clear();
            state.active.drain().map(|(_, target)| target).collect()
        }
        Err(_) => return,
    };
    let Ok(dir) = ensure_control_dir(settings) else {
        return;
    };
    for target in targets {
        let Ok(destination) = target.destination() else {
            continue;
        };
        let mut args = vec!["-O".to_owned(), "exit".to_owned()];
        args.extend(common_args(&target, settings, Some(&dir), false));
        args.push(destination);
        let _ = process::run(
            ssh_command(&args),
            None,
            Duration::from_secs(3),
            1024,
            &AtomicBool::new(false),
            &mut |_, _| {},
        );
    }
}

#[cfg(unix)]
fn register_cleanup() {
    extern "C" fn close_at_exit() {
        let _ = std::panic::catch_unwind(|| {
            let settings = Settings {
                user_dir: super::user_dir().unwrap_or_else(|_| PathBuf::from(".dial")),
                config: super::SshConfig::default(),
            };
            close_all(&settings)
        });
    }
    ONCE.call_once(|| unsafe {
        libc::atexit(close_at_exit);
    });
}

#[cfg(not(unix))]
fn register_cleanup() {}

#[cfg(test)]
mod tests;
