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
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const LOCK_FILE: &str = "lock";
const SERVER_FILE: &str = "server.json";
const LEASES: &str = "leases";
/// Output of the server Dial started, for diagnosing a failed start.
pub const LOG_FILE: &str = "serve.log";
const POLL: Duration = Duration::from_millis(200);

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
    fn reachable(&self) -> bool;
    /// Launches the server and returns its identity.
    fn spawn(&self, dir: &Path) -> Result<ProcId>;
}

struct RealHost {
    host: String,
    port: u16,
    program: Option<PathBuf>,
}

impl Host for RealHost {
    fn reachable(&self) -> bool {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(1)))
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
    let (host, port) = local_address(base_url)
        .with_context(|| format!("Dial starts Ollama only on this machine, not at {base_url}"))?;
    let dir = state_dir(user_dir, &host, port);
    let me = ProcId::current().context("could not read this process's identity")?;
    let host = RealHost {
        host,
        port,
        program: program.map(Path::to_path_buf),
    };
    let ready = ensure_with(&dir, me, &host, wait)?;
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

fn lock(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir.join(LEASES)).with_context(|| format!("creating {}", dir.display()))?;
    let file = platform_fs::open_owner_only(
        OpenOptions::new().create(true).write(true),
        &dir.join(LOCK_FILE),
    )
    .context("opening the Ollama server lock")?;
    file.lock().context("locking the Ollama server state")?;
    Ok(file)
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

pub(crate) fn ensure_with(
    dir: &Path,
    me: ProcId,
    host: &dyn Host,
    wait: Duration,
) -> Result<Ready> {
    let _lock = lock(dir)?;
    fs::write(lease_path(dir, me), serde_json::to_vec(&me)?)
        .context("recording the Ollama server lease")?;
    if host.reachable() {
        return Ok(Ready::Running);
    }
    let server = match read_server(dir).filter(ProcId::alive) {
        // Another Dial process started it and it is still coming up: wait for it, never spawn twice.
        Some(server) => server,
        None => {
            let server = host.spawn(dir)?;
            fs::write(dir.join(SERVER_FILE), serde_json::to_vec(&server)?)
                .context("recording the Ollama server")?;
            server
        }
    };
    let deadline = Instant::now() + wait;
    loop {
        if host.reachable() {
            return Ok(Ready::Started);
        }
        ensure!(
            server.alive(),
            "`ollama serve` exited during startup; see {}",
            dir.join(LOG_FILE).display()
        );
        if Instant::now() >= deadline {
            return Ok(Ready::Starting);
        }
        std::thread::sleep(POLL);
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
