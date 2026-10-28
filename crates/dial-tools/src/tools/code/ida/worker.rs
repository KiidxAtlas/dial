//! Supervisor for one `idalib` worker process: JSON-lines RPC with deadlines, interrupts, and teardown.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::platform::process::{kill_group, spawn_group, wait_exited};

const WORKER_SOURCE: &str = include_str!("worker.py");
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a worker has to answer an interrupt before its process group is killed.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
/// How long a worker has to exit after a successful `close` before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(25);
const STDERR_TAIL: usize = 4096;

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A successful RPC reply. `stopped` carries the reason when the request was interrupted by
/// timeout or cancellation; the worker may still have produced a partial result.
pub(super) struct Call {
    pub(super) result: Value,
    pub(super) stopped: Option<String>,
}

struct Stop {
    at: Instant,
    reason: String,
}

enum Recv {
    Frame(Value),
    Idle,
    Closed,
}

pub(super) struct Worker {
    child: Child,
    stdin: ChildStdin,
    frames: mpsc::Receiver<String>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    next_id: u64,
}

impl Worker {
    /// Spawn the worker and wait for its `ready` frame. `python` is the operator-configured
    /// interpreter; `idadir` (when non-empty) is exported as `IDADIR` for idalib.
    pub(super) fn start(python: &str, idadir: &str, cancel: &AtomicBool) -> Result<Self, String> {
        let mut command = Command::new(python);
        command
            .args(["-u", "-c", WORKER_SOURCE])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !idadir.is_empty() {
            command.env("IDADIR", idadir);
        }
        let mut child = spawn_group(&mut command).map_err(|error| {
            format!("cannot start Python interpreter `{python}`: {error}; install Python 3 or set ida.python")
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            kill_child(&mut child);
            return Err("could not attach to the IDA worker's stdio".to_owned());
        };

        let (tx, frames) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let tail = Arc::clone(&stderr_tail);
        thread::spawn(move || {
            let mut reader = stderr;
            let mut chunk = [0u8; 1024];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut buf = lock(&tail);
                buf.extend(&chunk[..n]);
                let excess = buf.len().saturating_sub(STDERR_TAIL);
                buf.drain(..excess);
            }
        });

        let mut worker = Worker {
            child,
            stdin,
            frames,
            stderr: stderr_tail,
            next_id: 1,
        };
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let first = loop {
            match worker.recv(POLL)? {
                Recv::Frame(frame) => break frame,
                Recv::Closed => return Err(worker.exit_error("starting")),
                Recv::Idle => {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("IDA startup cancelled".to_owned());
                    }
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "IDA runtime did not start within {}s",
                            STARTUP_TIMEOUT.as_secs()
                        ));
                    }
                }
            }
        };
        match first.get("event").and_then(Value::as_str) {
            Some("ready") => Ok(worker),
            Some("error") => Err(runtime_error(python, &first)),
            _ => Err("IDA worker sent an unexpected startup frame".to_owned()),
        }
    }

    /// Send one request and wait for its response. On timeout or cancellation the worker is
    /// interrupted; if it does not answer within the grace period its process group is killed.
    pub(super) fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<Call, String> {
        let id = self.next_id;
        self.next_id += 1;
        if self
            .write(&json!({"id": id, "method": method, "params": params}))
            .is_err()
        {
            return Err(self.exit_error(method));
        }
        let deadline = Instant::now() + timeout;
        let mut stop: Option<Stop> = None;
        loop {
            match self.recv(POLL)? {
                Recv::Frame(frame) => {
                    if frame.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    return finish(method, frame, stop.map(|s| s.reason));
                }
                Recv::Closed => return Err(self.exit_error(method)),
                Recv::Idle => {}
            }
            let now = Instant::now();
            if let Some(s) = &stop {
                if now.duration_since(s.at) >= INTERRUPT_GRACE {
                    let reason = s.reason.clone();
                    self.kill();
                    return Err(format!(
                        "{method} {reason}; the worker did not stop within {}s and was killed, so unsaved database changes are lost",
                        INTERRUPT_GRACE.as_secs()
                    ));
                }
            } else {
                let reason = if cancel.load(Ordering::Relaxed) {
                    Some("cancelled".to_owned())
                } else if now >= deadline {
                    Some(format!("timed out after {}s", timeout.as_secs()))
                } else {
                    None
                };
                if let Some(reason) = reason {
                    if self.write(&json!({"interrupt": id})).is_err() {
                        return Err(self.exit_error(method));
                    }
                    stop = Some(Stop { at: now, reason });
                }
            }
        }
    }

    /// True while the worker process has not exited.
    pub(super) fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait briefly for a worker that already acknowledged `close` to exit; kill it otherwise.
    pub(super) fn shutdown(&mut self) {
        if !matches!(wait_exited(&self.child, SHUTDOWN_GRACE), Ok(true)) {
            self.kill();
        }
    }

    pub(super) fn kill(&mut self) {
        kill_child(&mut self.child);
    }

    fn write(&mut self, frame: &Value) -> std::io::Result<()> {
        let mut line = frame.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()
    }

    fn recv(&mut self, timeout: Duration) -> Result<Recv, String> {
        match self.frames.recv_timeout(timeout) {
            Ok(line) => match serde_json::from_str::<Value>(&line) {
                Ok(frame) => Ok(Recv::Frame(frame)),
                Err(error) => {
                    self.kill();
                    Err(format!("IDA worker sent malformed output: {error}"))
                }
            },
            Err(RecvTimeoutError::Timeout) => Ok(Recv::Idle),
            Err(RecvTimeoutError::Disconnected) => Ok(Recv::Closed),
        }
    }

    /// Reap the exited worker and describe how it died, with the tail of its stderr.
    fn exit_error(&mut self, phase: &str) -> String {
        let status = match self.child.wait() {
            Ok(status) => status.to_string(),
            Err(error) => error.to_string(),
        };
        let bytes: Vec<u8> = lock(&self.stderr).iter().copied().collect();
        let tail = String::from_utf8_lossy(&bytes);
        let tail = tail.trim();
        if tail.is_empty() {
            format!("IDA worker exited while {phase} ({status})")
        } else {
            format!("IDA worker exited while {phase} ({status}): {tail}")
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        kill_child(&mut self.child);
    }
}

fn kill_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        let _ = kill_group(child.id());
    }
    let _ = child.wait();
}

fn finish(method: &str, frame: Value, stopped: Option<String>) -> Result<Call, String> {
    if frame.get("ok").and_then(Value::as_bool) == Some(true) {
        let result = frame.get("result").cloned().unwrap_or(Value::Null);
        return Ok(Call { result, stopped });
    }
    let error = frame.get("error");
    let kind = error
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("error");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match stopped {
        Some(reason) => Err(format!(
            "{method} {reason}; the worker stopped the request ({kind}: {message})"
        )),
        None => Err(format!("{kind}: {message}")),
    }
}

fn runtime_error(python: &str, frame: &Value) -> String {
    let error = frame.get("error");
    let kind = error
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("error");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    format!(
        "IDA runtime is not usable ({kind}: {message}). Install IDA Pro with idalib, set ida.idadir to its install directory (or export IDADIR), and check that `{python} -c 'import idapro'` succeeds"
    )
}
