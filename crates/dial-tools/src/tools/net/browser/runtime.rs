//! Private, pinned runtime for the built-in browser tool.
//!
//! Playwright, Readability and (when no Chromium is configured) a Playwright-managed
//! Chromium live in an owner-only cache directory keyed by a digest of the embedded
//! manifest. Dependencies are installed with `bun install --frozen-lockfile
//! --ignore-scripts`, so nothing is fetched into the workspace and no install scripts run.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const PACKAGE: &str = include_str!("../../../../../../runtimes/browser/package.json");
const LOCK: &str = include_str!("../../../../../../runtimes/browser/bun.lock");
/// Upper bound for dependency installation.
const INSTALL_CAP: Duration = Duration::from_secs(300);
/// Upper bound for the Chromium download, which is large.
const CHROMIUM_CAP: Duration = Duration::from_secs(900);
const POLL: Duration = Duration::from_millis(50);

/// Locations of the provisioned browser runtime.
#[derive(Debug, Clone)]
pub(super) struct Runtime {
    pub(super) root: PathBuf,
    /// Playwright entry (`node_modules/playwright-core/index.js`).
    pub(super) entry: PathBuf,
    /// Playwright CLI, used to install Chromium into `browsers`.
    pub(super) cli: PathBuf,
    /// Mozilla Readability source, injected into pages for `readable` extraction.
    pub(super) readability: PathBuf,
    /// `PLAYWRIGHT_BROWSERS_PATH` for the private Chromium.
    pub(super) browsers: PathBuf,
}

/// Provision (or reuse) the pinned runtime. Install and download are bounded by their
/// caps and abort promptly when `interrupt` is set.
pub(super) fn provision(interrupt: &AtomicBool, allow_install: bool) -> Result<Runtime, String> {
    let root = cache_root()?.join(digest_dir());
    if !allow_install {
        return cached();
    }
    provision_at(&root, interrupt, || {
        hosts_reachable(&["registry.npmjs.org:443"])
    })
}

/// Inspect existing files only: never contact a service, require JS, install or launch Chromium.
pub(super) fn cached() -> Result<Runtime, String> {
    cached_at(&cache_root()?.join(digest_dir()))
}

fn cached_at(root: &Path) -> Result<Runtime, String> {
    let runtime = layout(root);
    if !dependencies_cached(&runtime) {
        return Err("browser prerequisite unavailable: pinned JavaScript runtime is not cached; benchmark installation is disabled".into());
    }
    Ok(runtime)
}

// Keep cache selection and network probing injectable without changing process-wide environment.
fn provision_at(
    root: &Path,
    interrupt: &AtomicBool,
    reachable: impl FnOnce() -> bool,
) -> Result<Runtime, String> {
    let runtime = layout(root);
    ensure_fetchable(dependencies_cached(&runtime), reachable)?;
    std::fs::create_dir_all(root).map_err(|e| format!("creating private browser runtime: {e}"))?;
    crate::platform::fs::set_owner_only(root)
        .map_err(|e| format!("restricting private browser runtime permissions: {e}"))?;
    for (name, body) in [("package.json", PACKAGE), ("bun.lock", LOCK)] {
        let path = root.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(body) {
            crate::config::settings_store::write_private(&path, body.as_bytes())
                .map_err(|e| format!("writing private browser runtime: {e}"))?;
        }
        crate::platform::fs::set_owner_only(&path)
            .map_err(|e| format!("restricting private browser runtime file: {e}"))?;
    }
    if !dependencies_cached(&runtime) {
        let mut command = Command::new(bun());
        command
            .current_dir(root)
            .args(["install", "--frozen-lockfile", "--ignore-scripts"]);
        run_bounded(
            command,
            INSTALL_CAP,
            interrupt,
            "browser dependency installation",
        )?;
        if !dependencies_cached(&runtime) {
            return Err("browser dependency installation produced an incomplete runtime".into());
        }
    }
    Ok(runtime)
}

/// Download the private Chromium if its executable is missing. `node` is the trusted Node binary.
pub(super) fn install_chromium(
    node: &Path,
    runtime: &Runtime,
    interrupt: &AtomicBool,
) -> Result<(), String> {
    if !hosts_reachable(&["cdn.playwright.dev:443"]) {
        return Err("Chromium is not cached and its download host (cdn.playwright.dev) is unreachable; connect to the network once, configure browser.chromium_executable, or attach via CDP".into());
    }
    let mut command = Command::new(node);
    command
        .arg(&runtime.cli)
        .args(["install", "chromium"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("PLAYWRIGHT_BROWSERS_PATH", &runtime.browsers)
        .current_dir(&runtime.root);
    std::fs::create_dir_all(&runtime.browsers)
        .map_err(|e| format!("creating private Chromium directory: {e}"))?;
    crate::platform::fs::set_owner_only(&runtime.browsers)
        .map_err(|e| format!("restricting private Chromium directory: {e}"))?;
    run_bounded(command, CHROMIUM_CAP, interrupt, "Chromium download")
}

/// Cache directory: `<cache>/dial/browser-runtime/<digest>`. The digest changes with the manifest.
fn layout(root: &Path) -> Runtime {
    let package = root.join("node_modules");
    Runtime {
        root: root.to_path_buf(),
        entry: package.join("playwright-core").join("index.js"),
        cli: package.join("playwright-core").join("cli.js"),
        readability: package
            .join("@mozilla")
            .join("readability")
            .join("Readability.js"),
        browsers: root.join("browsers"),
    }
}

fn digest_dir() -> String {
    let mut hasher = Sha256::new();
    hasher.update(PACKAGE.as_bytes());
    hasher.update([0]);
    hasher.update(LOCK.as_bytes());
    crate::hex(&hasher.finalize()[..8])
}
fn dependencies_cached(runtime: &Runtime) -> bool {
    runtime.entry.is_file() && runtime.readability.is_file()
}

/// True when the private runtime and a Chromium build are already cached, so no network is needed.
#[cfg(test)]
pub(super) fn is_cached() -> bool {
    let Ok(root) = cache_root().map(|base| base.join(digest_dir())) else {
        return false;
    };
    let runtime = layout(&root);
    let has_chromium = std::fs::read_dir(&runtime.browsers).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("chromium-"))
    });
    dependencies_cached(&runtime) && has_chromium
}

/// Refuse to fetch uncached JavaScript dependencies offline. Chromium availability is checked
/// separately, only when startup actually needs to download a private browser.
fn ensure_fetchable(cached: bool, reachable: impl FnOnce() -> bool) -> Result<(), String> {
    if cached || reachable() {
        return Ok(());
    }
    Err("browser JavaScript dependencies are not cached and their download host (registry.npmjs.org) is unreachable; connect to the network once to provision them".into())
}

/// Whether the hosts that provisioning downloads from accept a TCP connection promptly.
#[cfg(test)]
pub(super) fn network_reachable() -> bool {
    hosts_reachable(&["registry.npmjs.org:443", "cdn.playwright.dev:443"])
}

fn hosts_reachable(hosts: &[&str]) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    hosts.iter().all(|host| {
        host.to_socket_addrs()
            .ok()
            .and_then(|mut addresses| addresses.next())
            .is_some_and(|address| {
                TcpStream::connect_timeout(&address, Duration::from_secs(3)).is_ok()
            })
    })
}

fn cache_root() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or("cannot determine private browser runtime cache directory")?;
    Ok(base.join("dial").join("browser-runtime"))
}

fn bun() -> PathBuf {
    std::env::var_os("DIAL_BUN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "bun.exe" } else { "bun" }))
}

/// Run a provisioning command in its own process group, bounded by `cap` and `interrupt`.
fn run_bounded(
    mut command: Command,
    cap: Duration,
    interrupt: &AtomicBool,
    what: &str,
) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = crate::platform::process::spawn_group(&mut command)
        .map_err(|e| format!("starting {what}: {e}"))?;
    let deadline = Instant::now() + cap;
    loop {
        if interrupt.load(Ordering::SeqCst) {
            reap(&mut child);
            return Err(format!("{what} cancelled"));
        }
        let polled = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                reap(&mut child);
                return Err(format!("checking {what}: {error}"));
            }
        };
        if let Some(status) = polled {
            crate::platform::process::forget_group(child.id());
            return if status.success() {
                Ok(())
            } else {
                Err(format!(
                    "{what} failed (exit {status}); check network access and retry"
                ))
            };
        }
        if Instant::now() >= deadline {
            reap(&mut child);
            return Err(format!("{what} timed out"));
        }
        std::thread::sleep(POLL);
    }
}

fn reap(child: &mut Child) {
    let pid = child.id();
    let _ = crate::platform::process::kill_group(pid);
    let _ = child.wait();
    crate::platform::process::forget_group(pid);
}

#[cfg(test)]
mod tests;
