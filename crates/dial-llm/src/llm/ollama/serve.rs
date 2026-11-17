//! Starts a local Ollama server on demand and stops it once no Dial process needs it.
//!
//! There is one server per address. Callers hold a lock file while they probe and spawn, so
//! concurrent Dial processes reuse a running server, or wait for one that another Dial process is
//! starting, instead of launching a second. Each Dial process that relies on the server records a
//! lease named after its process identity. [`shutdown`], which runs when Dial exits, drops this
//! process's leases and stops a server only when Dial started it and no live Dial process still
//! holds a lease. A server the user started (`ollama serve`, the Ollama app, `brew services`) is
//! reused and never stopped. A Dial process that dies without `shutdown` leaves a dead lease; the
//! next release prunes it.
//!
//! Ollama itself loads a model on the first request and unloads it after its idle time
//! (`keep_alive`, five minutes by default), so only the server is managed here.

use crate::platform::{fs as platform_fs, process, procinfo::ProcId};
use anyhow::{Context, Result, bail, ensure};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const LOCK_FILE: &str = "lock";
const SERVER_FILE: &str = "server.json";
const LEASES: &str = "leases";
/// Output of the server Dial started, for diagnosing a failed start.
pub const LOG_FILE: &str = "serve.log";
/// Interval between readiness probes while a started server comes up.
const POLL: Duration = Duration::from_millis(200);
/// Interval between attempts to take the state lock while another process holds it.
const LOCK_POLL: Duration = Duration::from_millis(20);
/// Longest a single readiness probe may take, so a probe never outlives the caller's deadline by much.
const PROBE: Duration = Duration::from_secs(1);
/// Shortest probe timeout. A probe that starts just before the deadline may run this long past it:
/// the bound on how far an [`ensure_until`] call can overrun its deadline, besides the lock poll.
pub const MIN_PROBE: Duration = Duration::from_millis(100);

/// State directories this process holds a lease in, released by [`shutdown`].
static HELD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// How [`ensure`] found the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ready {
    /// It already answered; Dial did not start it in this call.
    Running,
    /// It was started (by this call or another Dial process) and now answers.
    Started,
    /// A server Dial started is still coming up after the wait.
    Starting,
}

/// An Ollama start interrupted by its caller's cancellation. It is not a failed start: nothing is
/// recorded as broken, so the next caller may try again at once.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Ollama start was cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// The loopback `host:port` of an `http://` Ollama base URL, or `None` for any other address.
/// Only a server on this machine is started.
pub fn local_address(base_url: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(base_url.trim()).ok()?;
    if parsed.scheme() != "http" {
        return None;
    }
    let host = parsed.host_str()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = bare == "localhost"
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    loopback.then(|| (host.to_owned(), parsed.port().unwrap_or(11434)))
}

/// Where the lock, server record and leases for one address live.
pub fn state_dir(user_dir: &Path, host: &str, port: u16) -> PathBuf {
    let host: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    user_dir
        .join("state")
        .join("ollama-serve")
        .join(format!("{host}_{port}"))
}

/// What the lifecycle needs from the outside world; tests substitute it.
pub(crate) trait Host {
    /// Whether the server answers within `timeout`.
    fn reachable(&self, timeout: Duration) -> bool;
    /// Launches the server and returns its identity.
    fn spawn(&self, dir: &Path) -> Result<ProcId>;
}

struct RealHost {
    host: String,
    port: u16,
    program: Option<PathBuf>,
}

impl Host for RealHost {
    fn reachable(&self, timeout: Duration) -> bool {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build()
            .into();
        agent
            .get(format!("http://{}:{}/api/version", self.host, self.port))
            .call()
            .is_ok_and(|response| response.status().is_success())
    }

    fn spawn(&self, dir: &Path) -> Result<ProcId> {
        let program = match &self.program {
            Some(program) => program.clone(),
            None => find_program().context(
                "Ollama is not installed or not on PATH; install it (https://ollama.com/download) or start your embedding server",
            )?,
        };
        let log = platform_fs::open_owner_only(
            OpenOptions::new().create(true).write(true).truncate(true),
            &dir.join(LOG_FILE),
        )
        .context("opening the Ollama server log")?;
        let mut command = Command::new(&program);
        command
            .arg("serve")
            .env("OLLAMA_HOST", format!("{}:{}", self.host, self.port))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        let child = process::spawn_group(&mut command)
            .with_context(|| format!("starting {} serve", program.display()))?;
        // The child is not waited on: it outlives this call and is stopped by `release`.
        ProcId::of(child.id() as i32).context("`ollama serve` exited immediately")
    }
}

/// `ollama` on PATH, then the usual install locations a GUI-launched PATH can miss.
fn find_program() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "ollama.exe"
    } else {
        "ollama"
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Makes sure an Ollama server answers at `base_url`, a loopback `http://` address, starting
/// `ollama serve` (`program`, or the one on PATH) when nothing listens there. Waits up to `wait`
/// for a started server. This process holds a lease until [`shutdown`].
pub fn ensure(
    user_dir: &Path,
    base_url: &str,
    program: Option<&Path>,
    wait: Duration,
) -> Result<Ready> {
    ensure_until(user_dir, base_url, program, Instant::now() + wait, None)
}

/// [`ensure`] bounded by an absolute `deadline` and an optional `cancel` flag. Every nested step
/// (waiting for the state lock, probing, spawning, polling for readiness) honours both. A deadline
/// that has passed before the state lock is taken returns [`Ready::Starting`] without taking the
/// lock, recording a lease or spawning; a deadline passing later, while the server is still coming
/// up, also returns `Starting`. A set `cancel` returns [`Cancelled`] at the next check, and a
/// cancelled or failed call drops its own lease, so it does not keep the server up. The call can
/// finish up to one readiness probe ([`MIN_PROBE`] at least) after `deadline`.
pub fn ensure_until(
    user_dir: &Path,
    base_url: &str,
    program: Option<&Path>,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<Ready> {
    let (host, port) = local_address(base_url)
        .with_context(|| format!("Dial starts Ollama only on this machine, not at {base_url}"))?;
    let dir = state_dir(user_dir, &host, port);
    let me = ProcId::current().context("could not read this process's identity")?;
    let host = RealHost {
        host,
        port,
        program: program.map(Path::to_path_buf),
    };
    let ready = ensure_with(&dir, me, &host, deadline, cancel)?;
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if !held.contains(&dir) {
        held.push(dir);
    }
    Ok(ready)
}

/// Releases this process's leases, stopping each server Dial started that no live Dial process
/// still uses. Run once when Dial exits; errors are ignored because exit must not fail on them.
pub fn shutdown() {
    let dirs = std::mem::take(&mut *HELD.lock().unwrap_or_else(|e| e.into_inner()));
    let Some(me) = ProcId::current() else {
        return;
    };
    for dir in dirs {
        let _ = release(&dir, me);
    }
}

fn open_lock(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir.join(LEASES)).with_context(|| format!("creating {}", dir.display()))?;
    platform_fs::open_owner_only(
        OpenOptions::new().create(true).write(true),
        &dir.join(LOCK_FILE),
    )
    .context("opening the Ollama server lock")
}

/// Takes the state lock, waiting unboundedly. Used only where no caller deadline applies.
fn lock(dir: &Path) -> Result<File> {
    let file = open_lock(dir)?;
    file.lock().context("locking the Ollama server state")?;
    Ok(file)
}

/// Takes the state lock before `deadline`, or `Ok(None)` when another process still holds it
/// then. A set `cancel` stops the wait with [`Cancelled`].
fn lock_until(dir: &Path, deadline: Instant, cancel: Option<&AtomicBool>) -> Result<Option<File>> {
    let mut file = None;
    loop {
        // Cancellation and the deadline are checked before every attempt, so neither a pre-set
        // flag nor an expired deadline ever takes the lock.
        check_cancel(cancel)?;
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let opened = match file.take() {
            Some(opened) => opened,
            None => open_lock(dir)?,
        };
        match opened.try_lock() {
            Ok(()) => return Ok(Some(opened)),
            Err(TryLockError::WouldBlock) => file = Some(opened),
            Err(TryLockError::Error(error)) => {
                return Err(error).context("locking the Ollama server state");
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(LOCK_POLL.min(deadline - now));
    }
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err(anyhow::Error::new(Cancelled));
    }
    Ok(())
}

/// How long one readiness probe may run before `deadline`.
fn probe_timeout(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .clamp(MIN_PROBE, PROBE)
}

fn lease_path(dir: &Path, client: ProcId) -> PathBuf {
    dir.join(LEASES)
        .join(format!("{}-{}", client.pid, client.start))
}

fn read_server(dir: &Path) -> Option<ProcId> {
    serde_json::from_slice(&fs::read(dir.join(SERVER_FILE)).ok()?).ok()
}

/// Live lease holders after removing dead ones.
fn live_leases(dir: &Path) -> Result<Vec<ProcId>> {
    let mut live = Vec::new();
    for entry in fs::read_dir(dir.join(LEASES))? {
        let path = entry?.path();
        let holder = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ProcId>(&bytes).ok());
        match holder {
            Some(holder) if holder.alive() => live.push(holder),
            _ => {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(live)
}

/// The lifecycle behind [`ensure_until`]. The state lock covers only the decision and its records
/// (lease, server record, at most one spawn); readiness is polled without it so other clients are
/// never queued behind a slow start.
pub(crate) fn ensure_with(
    dir: &Path,
    me: ProcId,
    host: &dyn Host,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<Ready> {
    let result = claim_and_wait(dir, me, host, deadline, cancel);
    if result.is_err() {
        // A caller that fails or is cancelled does not keep the server alive for itself.
        let _ = fs::remove_file(lease_path(dir, me));
    }
    result
}

fn claim_and_wait(
    dir: &Path,
    me: ProcId,
    host: &dyn Host,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<Ready> {
    let Some(lock) = lock_until(dir, deadline, cancel)? else {
        // Another process holds the state and may be starting the server, or the deadline passed
        // first: either way this call does not wait past its deadline.
        return Ok(Ready::Starting);
    };
    fs::write(lease_path(dir, me), serde_json::to_vec(&me)?)
        .context("recording the Ollama server lease")?;
    if host.reachable(probe_timeout(deadline)) {
        return Ok(Ready::Running);
    }
    check_cancel(cancel)?;
    let server = match read_server(dir).filter(ProcId::alive) {
        // Another Dial process started it and it is still coming up: wait for it, never spawn twice.
        Some(server) => server,
        // A spawn is not started once the deadline has passed; the lease stays for a later call.
        None if Instant::now() >= deadline => return Ok(Ready::Starting),
        None => {
            let server = host.spawn(dir)?;
            fs::write(dir.join(SERVER_FILE), serde_json::to_vec(&server)?)
                .context("recording the Ollama server")?;
            server
        }
    };
    drop(lock);
    loop {
        if host.reachable(probe_timeout(deadline)) {
            return Ok(Ready::Started);
        }
        ensure!(
            server.alive(),
            "`ollama serve` exited during startup; see {}",
            dir.join(LOG_FILE).display()
        );
        check_cancel(cancel)?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(Ready::Starting);
        }
        std::thread::sleep(POLL.min(deadline - now));
    }
}

/// Drops `client`'s lease and stops the server Dial started once no live lease remains.
/// Returns whether a server was stopped.
pub(crate) fn release(dir: &Path, client: ProcId) -> Result<bool> {
    let _lock = lock(dir)?;
    let _ = fs::remove_file(lease_path(dir, client));
    if !live_leases(dir)?.is_empty() {
        return Ok(false);
    }
    let Some(server) = read_server(dir) else {
        return Ok(false);
    };
    let _ = fs::remove_file(dir.join(SERVER_FILE));
    if !server.same_process() {
        return Ok(false);
    }
    let Ok(pid) = u32::try_from(server.pid) else {
        bail!("invalid Ollama server pid {}", server.pid);
    };
    process::terminate_group(pid).context("stopping the Ollama server Dial started")?;
    Ok(true)
}

#[cfg(test)]
mod tests;
