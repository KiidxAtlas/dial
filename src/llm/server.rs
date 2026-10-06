//! Supervision of a persistent `python -m mlx_lm.server` (mlx-lm 0.32.0).
//!
//! The server listens on loopback only, runs in its own process group, and is
//! killed (group and all) on `stop()` or drop. Model and adapter are fixed at
//! start: `mlx_lm.server` can switch them per request (`model` / `adapters`
//! body fields, loaded on demand from any path), but dial never uses that and
//! restarts the server instead, via [`ensure`].
//!
//! Residual limits: any local process, including a tool the agent runs, can
//! reach the loopback port and send requests, which can make the server load
//! files the process can read. Nothing here authenticates the port.

use super::{self as llm, Chat, SERVER_MODEL};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;
use std::io::{self, Read};
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Stderr kept in memory for diagnostics; older output is discarded.
const STDERR_KEEP_BYTES: usize = 8192;
/// Portion of the stderr tail quoted in start-up errors.
const ERROR_TAIL_BYTES: usize = 2048;
/// Port races are retried; model-load failures are not.
const PORT_ATTEMPTS: usize = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Share of physical memory the server's prompt (KV prefix) cache may hold. mlx_lm keeps up to ten
/// caches with no byte limit by default; a few long-context caches next to the weights push unified
/// memory into swap, which costs far more than the occasional re-prefill an evicted cache causes.
const PROMPT_CACHE_MEMORY_DIVISOR: u64 = 4;

/// The `--prompt-cache-bytes` budget, or `None` when physical memory cannot be read.
fn prompt_cache_bytes() -> Option<u64> {
    // SAFETY: sysconf only reads static system configuration.
    let (pages, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let total = u64::try_from(pages)
        .ok()?
        .checked_mul(u64::try_from(page_size).ok()?)?;
    (total > 0).then(|| total / PROMPT_CACHE_MEMORY_DIVISOR)
}

#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    eof: bool,
}

/// Drain the child's stderr into a bounded in-memory tail. A pipe that nobody
/// reads would eventually block the server; a file would grow without bound.
/// At `--log-level WARNING` the server writes request lines and warnings only,
/// never prompt text.
fn collect_stderr(mut stderr: impl Read + Send + 'static) -> Arc<Mutex<Tail>> {
    let tail = Arc::new(Mutex::new(Tail::default()));
    let shared = Arc::clone(&tail);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            let count = match stderr.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut tail = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tail.bytes.extend_from_slice(&buffer[..count]);
            if tail.bytes.len() > STDERR_KEEP_BYTES {
                let excess = tail.bytes.len() - STDERR_KEEP_BYTES;
                tail.bytes.drain(..excess);
            }
        }
        shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .eof = true;
    });
    tail
}

fn tail_text(tail: &Mutex<Tail>, max_bytes: usize) -> String {
    let tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let start = tail.bytes.len().saturating_sub(max_bytes);
    String::from_utf8_lossy(&tail.bytes[start..])
        .trim()
        .to_string()
}

/// Wait briefly for the reader thread to see EOF so a dead child's final
/// words are included in the error.
fn settle_stderr(tail: &Mutex<Tail>) {
    for _ in 0..50 {
        if tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .eof
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The child and every process it started share one process group.
struct Supervised {
    child: Child,
    stopped: bool,
}

impl Supervised {
    /// Exit status is only observed (WNOWAIT), not consumed, so the group id
    /// cannot be recycled before `stop` signals the group.
    fn exited(&self) -> io::Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    fn stop(&mut self) {
        if !self.stopped {
            unsafe { libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL) };
            let _ = self.child.wait();
            self.stopped = true;
        }
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct ModelServer {
    process: Supervised,
    base_url: String,
    model: String,
    adapter: Option<PathBuf>,
    steering: Option<crate::steering::Specification>,
}

enum Failure {
    /// Another process took the port between probing and binding.
    PortTaken(anyhow::Error),
    Fatal(anyhow::Error),
}

impl ModelServer {
    /// Start a server for the resolved local model directory `model`, with an
    /// optional LoRA adapter directory, and wait until it can generate.
    pub fn start(
        python: &Path,
        model: &str,
        adapter: Option<&Path>,
        startup_timeout_secs: u64,
    ) -> Result<ModelServer> {
        Self::start_controlled(python, model, adapter, startup_timeout_secs, None)
    }

    fn start_controlled(
        python: &Path,
        model: &str,
        adapter: Option<&Path>,
        startup_timeout_secs: u64,
        interrupt: Option<Arc<AtomicBool>>,
    ) -> Result<ModelServer> {
        Self::start_profile(
            python,
            model,
            adapter,
            startup_timeout_secs,
            interrupt,
            None,
        )
    }

    fn start_profile(
        python: &Path,
        model: &str,
        adapter: Option<&Path>,
        startup_timeout_secs: u64,
        interrupt: Option<Arc<AtomicBool>>,
        steering: Option<&crate::steering::Specification>,
    ) -> Result<ModelServer> {
        ensure!(python.is_absolute(), "python path must be absolute");
        ensure!(!model.is_empty(), "model must not be empty");
        ensure!(
            startup_timeout_secs > 0,
            "server startup timeout must be positive"
        );
        let deadline = Instant::now() + Duration::from_secs(startup_timeout_secs);
        let mut last_race = None;
        for _ in 0..PORT_ATTEMPTS {
            let port = free_port()?;
            match launch(
                python,
                model,
                adapter,
                port,
                deadline,
                interrupt.clone(),
                steering,
            ) {
                Ok(server) => return Ok(server),
                Err(Failure::PortTaken(error)) => last_race = Some(error),
                Err(Failure::Fatal(error)) => return Err(error),
            }
        }
        Err(last_race.unwrap_or_else(|| anyhow!("no server start attempt was made"))).context(
            format!("model server could not claim a port after {PORT_ATTEMPTS} attempts"),
        )
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether this server serves exactly `model` with exactly `adapter`.
    pub fn matches(&self, model: &str, adapter: Option<&Path>) -> bool {
        self.model == model && self.adapter.as_deref() == adapter
    }

    /// False once the server process has exited (crash or external kill).
    pub fn is_running(&self) -> bool {
        !self.process.stopped && matches!(self.process.exited(), Ok(false))
    }

    /// Kill the whole process group and reap the server. Idempotent.
    pub fn stop(&mut self) {
        self.process.stop();
    }
}

/// Keep `slot` holding a running server for `model`/`adapter`, restarting it
/// when either changed or the process died. The old server is stopped first so
/// two copies of the weights are never resident at once; if the restart fails
/// the slot is left empty.
pub fn ensure<'a>(
    slot: &'a mut Option<ModelServer>,
    python: &Path,
    model: &str,
    adapter: Option<&Path>,
    startup_timeout_secs: u64,
) -> Result<&'a ModelServer> {
    ensure_controlled(slot, python, model, adapter, startup_timeout_secs, None)
}

pub fn ensure_controlled<'a>(
    slot: &'a mut Option<ModelServer>,
    python: &Path,
    model: &str,
    adapter: Option<&Path>,
    startup_timeout_secs: u64,
    interrupt: Option<Arc<AtomicBool>>,
) -> Result<&'a ModelServer> {
    ensure_profile_controlled(
        slot,
        python,
        model,
        adapter,
        startup_timeout_secs,
        interrupt,
        None,
    )
}

pub fn ensure_profile_controlled<'a>(
    slot: &'a mut Option<ModelServer>,
    python: &Path,
    model: &str,
    adapter: Option<&Path>,
    startup_timeout_secs: u64,
    interrupt: Option<Arc<AtomicBool>>,
    steering: Option<&crate::steering::Specification>,
) -> Result<&'a ModelServer> {
    let reusable = slot.as_ref().is_some_and(|server| {
        server.matches(model, adapter)
            && server.steering.as_ref() == steering
            && server.is_running()
    });
    if !reusable {
        drop(slot.take());
        *slot = Some(ModelServer::start_profile(
            python,
            model,
            adapter,
            startup_timeout_secs,
            interrupt,
            steering,
        )?);
    }
    slot.as_ref().context("model server slot is empty")
}

/// Ask the OS for a free loopback port. The listener is dropped before the
/// server binds, so a race is possible; `launch` reports it as `PortTaken`.
fn free_port() -> Result<u16> {
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("finding a free loopback port")?;
    Ok(listener
        .local_addr()
        .context("reading the chosen port")?
        .port())
}

fn launch(
    python: &Path,
    model: &str,
    adapter: Option<&Path>,
    port: u16,
    deadline: Instant,
    interrupt: Option<Arc<AtomicBool>>,
    steering: Option<&crate::steering::Specification>,
) -> std::result::Result<ModelServer, Failure> {
    let mut command = Command::new(python);
    // `--opt=value` keeps a value that starts with `-` from being read as a flag.
    command
        .arg("-c")
        .arg(format!("import sys, types\n_m = types.ModuleType('dial_steering')\nexec({}, _m.__dict__)\nsys.modules['dial_steering'] = _m\n{}", serde_json::to_string(include_str!("../../python/dial_steering.py")).unwrap(), include_str!("../../python/dial_server.py")))
        .arg(format!("--model={model}"))
        .arg("--host=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--log-level=WARNING");
    if let Some(bytes) = prompt_cache_bytes() {
        command.arg(format!("--prompt-cache-bytes={bytes}"));
    }
    if let Some(adapter) = adapter {
        let mut flag = std::ffi::OsString::from("--adapter-path=");
        flag.push(adapter);
        command.arg(flag);
    }
    let mut child = command
        .env(
            "DIAL_STEERING_SPEC",
            steering
                .map(|s| serde_json::to_string(s).unwrap())
                .unwrap_or_default(),
        )
        .env("HF_HUB_OFFLINE", "1")
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("starting mlx_lm.server")
        .map_err(Failure::Fatal)?;
    let stderr = child.stderr.take();
    let mut process = Supervised {
        child,
        stopped: false,
    };
    let tail =
        collect_stderr(stderr.ok_or_else(|| Failure::Fatal(anyhow!("missing server stderr")))?);
    let base_url = format!("http://127.0.0.1:{port}");

    let outcome = wait_ready(&mut process, &base_url, &tail, deadline, interrupt);
    match outcome {
        Ok(()) => Ok(ModelServer {
            process,
            base_url,
            model: model.to_string(),
            adapter: adapter.map(Path::to_path_buf),
            steering: steering.cloned(),
        }),
        Err(error) => {
            process.stop();
            settle_stderr(&tail);
            let text = tail_text(&tail, ERROR_TAIL_BYTES);
            let message = if text.is_empty() {
                format!("{error:#} (server wrote nothing to stderr)")
            } else {
                format!("{error:#}; server stderr (last {ERROR_TAIL_BYTES} bytes):\n{text}")
            };
            if text.contains("Address already in use") {
                Err(Failure::PortTaken(anyhow!(message)))
            } else {
                Err(Failure::Fatal(anyhow!(message)))
            }
        }
    }
}

fn wait_ready(
    process: &mut Supervised,
    base_url: &str,
    tail: &Mutex<Tail>,
    deadline: Instant,
    interrupt: Option<Arc<AtomicBool>>,
) -> Result<()> {
    let models_url = format!("{base_url}/v1/models");
    loop {
        if interrupt
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            return Err(llm::Interrupted.into());
        }
        ensure!(
            !process.exited().context("observing server exit")?,
            "model server exited during startup"
        );
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "model server not ready before the startup timeout"
        );
        let up = llm::agent(2)
            .get(&models_url)
            .call()
            .is_ok_and(|response| response.status().as_u16() == 200);
        if up {
            break;
        }
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
    // The HTTP port opens before the model finishes loading, and a bad
    // model/adapter only fails on first use. One 1-token request blocks until
    // the weights are loaded and proves generation works.
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_secs()
        .max(1);
    let warmup = Chat {
        messages: vec![json!({"role": "user", "content": "ping"})],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: 1,
        seed: None,
    };
    if let Err(error) = llm::chat_at_controlled(
        &llm::completions_url(base_url),
        None,
        SERVER_MODEL,
        &warmup,
        remaining,
        interrupt.clone(),
        None,
        &mut |_| true,
    ) {
        if interrupt
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            return Err(llm::Interrupted.into());
        }
        // A crash during load surfaces as a dropped connection; prefer saying so.
        for _ in 0..20 {
            if process.exited().unwrap_or(false) {
                settle_stderr(tail);
                bail!("model server exited while loading the model");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        return Err(error.context("model server warm-up request failed"));
    }
    ensure!(
        !process.exited().context("observing server exit")?,
        "model server exited right after becoming ready (is the port owned by another process?)"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// Stands in for `python -m mlx_lm.server`. The `--model` value selects a
    /// behaviour: `exit-early`, `never-ready`, `bad-model`, or a path that
    /// receives `{"pid", "child", "argv"}` once the server is listening.
    const FAKE_SERVER: &str = r#"#!/usr/bin/env python3
import http.server, json, os, subprocess, sys
opts = dict(a.split("=", 1) for a in sys.argv[1:] if a.startswith("--") and "=" in a)
mode = opts["--model"]
if mode == "exit-early":
    sys.stderr.write("fatal: cannot load model\n")
    sys.exit(3)
if mode == "never-ready":
    import time
    time.sleep(300)
helper = subprocess.Popen(["sleep", "300"])
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, code, payload):
        data = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        self.reply(200, {"object": "list", "data": []})
    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        if mode == "bad-model":
            self.reply(500, {"error": "adapter config is broken"})
        else:
            self.reply(200, {"choices": [{"finish_reason": "length", "message": {"role": "assistant", "content": "pong"}}],
                             "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
server = http.server.ThreadingHTTPServer((opts["--host"], int(opts["--port"])), Handler)
if mode != "bad-model":
    open(mode, "w").write(json.dumps({"pid": os.getpid(), "child": helper.pid, "argv": sys.argv[1:]}))
server.serve_forever()
"#;

    pub(crate) fn fake_python(dir: &Path) -> PathBuf {
        let path = dir.join("fake-python");
        fs::write(&path, FAKE_SERVER).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn alive(pid: i64) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    fn wait_dead(pid: i64) -> bool {
        for _ in 0..200 {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn info(path: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn steering_profile_change_restarts_server_and_clears_cache() {
        let root = tempfile::tempdir().unwrap();
        let python = fake_python(root.path());
        let marker = root.path().join("info.json");
        let model = marker.to_str().unwrap();
        let mut slot = None;
        let mut spec = crate::steering::Specification {
            path: root.path().join("vector.json"),
            id: "a".repeat(64),
            strength: 0.5,
            fingerprint: "first".into(),
        };
        ensure_profile_controlled(&mut slot, &python, model, None, 30, None, Some(&spec)).unwrap();
        let first = info(&marker)["pid"].as_i64().unwrap();
        ensure_profile_controlled(&mut slot, &python, model, None, 30, None, Some(&spec)).unwrap();
        assert_eq!(first, info(&marker)["pid"].as_i64().unwrap());
        spec.strength = 1.;
        ensure_profile_controlled(&mut slot, &python, model, None, 30, None, Some(&spec)).unwrap();
        let second = info(&marker)["pid"].as_i64().unwrap();
        assert_ne!(first, second);
        assert!(wait_dead(first));
        ensure_profile_controlled(&mut slot, &python, model, None, 30, None, None).unwrap();
        assert_ne!(second, info(&marker)["pid"].as_i64().unwrap());
        assert!(wait_dead(second));
    }

    #[test]
    fn ready_then_drop_kills_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let marker = dir.path().join("info.json");
        let model = marker.to_str().unwrap();
        let adapter = dir.path().join("adapter");
        let server = ModelServer::start(&python, model, Some(&adapter), 30).unwrap();
        assert!(server.base_url().starts_with("http://127.0.0.1:"));
        assert!(server.matches(model, Some(&adapter)));
        assert!(!server.matches(model, None));
        assert!(!server.matches("other", Some(&adapter)));
        assert!(server.is_running());
        let started = info(&marker);
        let (pid, helper) = (
            started["pid"].as_i64().unwrap(),
            started["child"].as_i64().unwrap(),
        );
        assert!(alive(pid) && alive(helper));
        let budget = started["argv"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a.as_str()?.strip_prefix("--prompt-cache-bytes="))
            .next()
            .expect("the KV prefix cache must be bounded")
            .parse::<u64>()
            .unwrap();
        assert!(budget > 0, "{budget}");
        drop(server);
        assert!(wait_dead(pid), "server survived drop");
        assert!(wait_dead(helper), "server's child survived drop");
    }

    #[test]
    fn explicit_stop_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let marker = dir.path().join("info.json");
        let mut server = ModelServer::start(&python, marker.to_str().unwrap(), None, 30).unwrap();
        let pid = info(&marker)["pid"].as_i64().unwrap();
        server.stop();
        assert!(!server.is_running());
        assert!(!alive(pid));
        server.stop();
    }

    #[test]
    fn early_exit_reports_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let error = ModelServer::start(&python, "exit-early", None, 30)
            .err()
            .expect("must fail")
            .to_string();
        assert!(error.contains("exited during startup"), "{error}");
        assert!(error.contains("fatal: cannot load model"), "{error}");
    }

    #[test]
    fn cancelled_startup_releases_the_server_before_its_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut slot = None;
        let started = Instant::now();
        std::thread::scope(|scope| {
            let flag = cancel.clone();
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                flag.store(true, Ordering::SeqCst);
            });
            let error =
                ensure_controlled(&mut slot, &python, "never-ready", None, 30, Some(cancel))
                    .err()
                    .expect("startup must be cancelled");
            assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
        });
        assert!(slot.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn startup_timeout_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let started = Instant::now();
        let error = ModelServer::start(&python, "never-ready", None, 1)
            .err()
            .expect("must fail")
            .to_string();
        assert!(error.contains("startup timeout"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn failed_warmup_is_a_start_failure() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let error = ModelServer::start(&python, "bad-model", None, 30)
            .err()
            .expect("must fail")
            .to_string();
        assert!(error.contains("warm-up"), "{error}");
        assert!(error.contains("HTTP 500"), "{error}");
    }

    #[test]
    fn ensure_reuses_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let first = dir.path().join("a.json");
        let second = dir.path().join("b.json");
        let mut slot = None;
        let url = ensure(&mut slot, &python, first.to_str().unwrap(), None, 30)
            .unwrap()
            .base_url()
            .to_string();
        let pid = info(&first)["pid"].as_i64().unwrap();
        assert_eq!(
            ensure(&mut slot, &python, first.to_str().unwrap(), None, 30)
                .unwrap()
                .base_url(),
            url
        );
        // A different adapter forces a restart and tears the old process down.
        let adapter = dir.path().join("adapter");
        ensure(
            &mut slot,
            &python,
            first.to_str().unwrap(),
            Some(&adapter),
            30,
        )
        .unwrap();
        assert!(wait_dead(pid), "old server not stopped on swap");
        let pid = info(&first)["pid"].as_i64().unwrap();
        // A crashed server is replaced even when the configuration is unchanged.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        for _ in 0..200 {
            if !slot.as_ref().unwrap().is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ensure(
            &mut slot,
            &python,
            first.to_str().unwrap(),
            Some(&adapter),
            30,
        )
        .unwrap();
        assert!(slot.as_ref().unwrap().is_running());
        assert_ne!(info(&first)["pid"].as_i64().unwrap(), pid);
        ensure(&mut slot, &python, second.to_str().unwrap(), None, 30).unwrap();
        assert!(
            slot.as_ref()
                .unwrap()
                .matches(second.to_str().unwrap(), None)
        );
    }

    #[test]
    fn free_ports_are_loopback_and_usable() {
        let port = free_port().unwrap();
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok());
    }

    #[test]
    fn stderr_tail_is_bounded() {
        let noise = vec![b'x'; STDERR_KEEP_BYTES * 3];
        let tail = collect_stderr(io::Cursor::new(noise));
        settle_stderr(&tail);
        assert_eq!(
            tail.lock().unwrap_or_else(|p| p.into_inner()).bytes.len(),
            STDERR_KEEP_BYTES
        );
        assert_eq!(tail_text(&tail, ERROR_TAIL_BYTES).len(), ERROR_TAIL_BYTES);
    }

    // ---- real mlx_lm.server (run with `cargo test --lib server -- --ignored --nocapture`) ----

    fn real_python() -> PathBuf {
        std::env::var_os("DIAL_TEST_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("python/.venv/bin/python")
            })
    }

    fn snapshot(repo_dir: &str) -> String {
        let home = std::env::var("HOME").unwrap();
        let snapshots = Path::new(&home)
            .join(".cache/huggingface/hub")
            .join(repo_dir)
            .join("snapshots");
        let entry = fs::read_dir(&snapshots)
            .unwrap_or_else(|e| panic!("{}: {e}", snapshots.display()))
            .next()
            .expect("no snapshot")
            .unwrap();
        entry.path().to_str().unwrap().to_string()
    }

    #[test]
    #[ignore = "starts the real mlx_lm.server; needs the SmolLM2 snapshot"]
    fn real_server_chat() {
        let model = snapshot("models--mlx-community--SmolLM2-135M-Instruct-8bit");
        let server = ModelServer::start(&real_python(), &model, None, 180).unwrap();
        let request = Chat {
            messages: vec![json!({"role": "user", "content": "Say hello in five words."})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 24,
            seed: Some(1),
        };
        let reply = llm::chat(server.base_url(), SERVER_MODEL, &request, 120).unwrap();
        println!("REAL REPLY: {reply:?}");
        assert!(!reply.text.is_empty());
        assert!(reply.completion_tokens.is_some());
    }

    #[test]
    #[ignore = "starts the real mlx_lm.server; set DIAL_TOOL_MODEL_DIR to a tool-capable snapshot"]
    fn real_server_tool_calls() {
        let model = std::env::var("DIAL_TOOL_MODEL_DIR").expect("DIAL_TOOL_MODEL_DIR");
        let server = ModelServer::start(&real_python(), &model, None, 180).unwrap();
        let tool = json!({"type": "function", "function": {
            "name": "read_file",
            "description": "Read a text file from the workspace",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }});
        let request = Chat {
            messages: vec![json!({"role": "user", "content": "Please read the file notes.txt"})],
            tools: vec![tool],
            temperature: 0.0,
            max_tokens: 128,
            seed: Some(1),
        };
        let reply = llm::chat(server.base_url(), SERVER_MODEL, &request, 120).unwrap();
        println!("REAL TOOL REPLY: {reply:?}");
        assert!(
            !reply.tool_calls.is_empty(),
            "model produced no parsed tool call"
        );
        assert_eq!(reply.tool_calls[0].name, "read_file");
        assert!(!reply.tool_calls[0].malformed_arguments);
        // Replay the turn with a tool result to prove the history shape is accepted.
        let mut followup = request.clone();
        followup.messages.push(llm::assistant_message(&reply));
        followup.messages.push(llm::tool_message(
            &reply.tool_calls[0].id,
            "hello from notes",
        ));
        let second = llm::chat(server.base_url(), SERVER_MODEL, &followup, 120).unwrap();
        println!("REAL FOLLOWUP: {second:?}");
    }
}
