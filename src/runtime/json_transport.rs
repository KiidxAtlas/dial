//! Bounded JSON-lines child transport shared by persistent execution adapters.
//! Callers supply launch policy, cancellation checks and authorized callback handlers.
//! This module has no knowledge of tools, preludes, workspace permissions or sandbox policy.
use super::process::ProcessGroup;
use serde_json::{Value, json};
#[cfg(unix)]
use std::io::Write;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicI32, Ordering},
};
use std::time::{Duration, Instant};

pub(crate) const MAX_FRAME: usize = 12 * 1024 * 1024;
/// Maximum response-drain grace after the caller's interrupt check trips.
pub(crate) const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
pub(crate) fn execution_deadline(timeout: Duration) -> Option<Instant> {
    (!timeout.is_zero()).then(|| Instant::now() + timeout)
}

#[cfg(windows)]
type ProcessStdin = crate::platform::io::QueuedWriter<ChildStdin>;
#[cfg(unix)]
type ProcessStdin = ChildStdin;
/// Bounded JSON-lines protocol. Pipes are nonblocking so neither output nor input can defeat deadlines.
pub(crate) struct JsonProcess {
    group: ProcessGroup,
    input: ProcessStdin,
    output: ChildStdout,
    errors: ChildStderr,
    pending: Vec<u8>,
    stderr: Vec<u8>,
    stderr_dropped: usize,
    pid: Arc<AtomicI32>,
    sequence: u64,
    callback_sequence: u64,
    /// A request frame was fully written and its reply has not been read yet.
    in_flight: bool,
}
impl JsonProcess {
    pub(crate) fn start(mut command: Command, pid: Arc<AtomicI32>) -> Result<Self, String> {
        let child = crate::platform::process::spawn_group(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .map_err(|e| e.to_string())?;
        // Owned from spawn onward: any later error terminates and reaps the group.
        let mut group = ProcessGroup::new(child);
        let input = group.child.stdin.take().ok_or("child stdin unavailable")?;
        let output = group
            .child
            .stdout
            .take()
            .ok_or("child stdout unavailable")?;
        let errors = group
            .child
            .stderr
            .take()
            .ok_or("child stderr unavailable")?;
        crate::platform::io::set_nonblocking(&input).map_err(|e| e.to_string())?;
        crate::platform::io::set_nonblocking(&output).map_err(|e| e.to_string())?;
        crate::platform::io::set_nonblocking(&errors).map_err(|e| e.to_string())?;
        #[cfg(windows)]
        let input = crate::platform::io::QueuedWriter::new(input, 64 * 1024);
        pid.store(group.child.id() as i32, Ordering::Release);
        Ok(Self {
            group,
            input,
            output,
            errors,
            pending: Vec::new(),
            stderr: Vec::new(),
            stderr_dropped: 0,
            pid,
            sequence: 0,
            callback_sequence: 0,
            in_flight: false,
        })
    }
    pub(crate) fn alive(&mut self) -> bool {
        self.group.exited().is_ok_and(|exited| !exited)
    }
    fn write_frame(
        &mut self,
        value: &Value,
        end: Option<Instant>,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_FRAME {
            return Err("request frame too large".into());
        }
        bytes.push(b'\n');
        let mut offset = 0;
        while offset < bytes.len() {
            check()?;
            self.drain_stderr();
            if end.is_some_and(|end| Instant::now() >= end) {
                return Err("execution deadline exceeded; namespace/session discarded".into());
            }
            match self.write_input(&bytes[offset..]) {
                Ok(0) => return Err("child stdin closed".into()),
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.poll_input(),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        loop {
            check()?;
            self.drain_stderr();
            if end.is_some_and(|end| Instant::now() >= end) {
                return Err("execution deadline exceeded; namespace/session discarded".into());
            }
            if self.input_drained().map_err(|error| error.to_string())? {
                break;
            }
        }
        Ok(())
    }
    fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        #[cfg(windows)]
        {
            self.input.try_write(bytes)
        }
        #[cfg(unix)]
        {
            self.input.write(bytes)
        }
    }
    fn input_drained(&self) -> std::io::Result<bool> {
        #[cfg(windows)]
        {
            self.input.wait_until_drained(Duration::from_millis(10))
        }
        #[cfg(unix)]
        {
            Ok(true)
        }
    }
    fn poll_input(&self) {
        #[cfg(windows)]
        {
            let _ = self.input.wait_until_drained(Duration::from_millis(10));
        }
        #[cfg(unix)]
        {
            let _ = crate::platform::io::wait_writable(&self.input, Duration::from_millis(10));
        }
    }
    fn poll_output(&self) {
        let _ = crate::platform::io::wait_readable(&self.output, Duration::from_millis(10));
    }
    fn drain_stderr(&mut self) {
        let mut buffer = [0u8; 16384];
        for _ in 0..8 {
            match crate::platform::io::read_nonblocking(&mut self.errors, &mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let remaining = (16 * 1024usize).saturating_sub(self.stderr.len());
                    self.stderr.extend_from_slice(&buffer[..n.min(remaining)]);
                    self.stderr_dropped = self
                        .stderr_dropped
                        .saturating_add(n.saturating_sub(remaining));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }
    pub(crate) fn request(
        &mut self,
        value: Value,
        timeout: Duration,
        callback: impl FnMut(&Value) -> Result<Value, String>,
    ) -> Result<Value, String> {
        self.request_guarded(
            value,
            execution_deadline(timeout),
            || Ok(()),
            usize::MAX,
            callback,
        )
    }
    /// Interrupt-aware request: a tripped check (cancel, deadline, interrupt) first gives the driver
    /// [`INTERRUPT_GRACE`] to unwind the cell after SIGINT, then the caller discards the session.
    pub(crate) fn request_guarded(
        &mut self,
        value: Value,
        end: Option<Instant>,
        check: impl Fn() -> Result<(), String>,
        max_callbacks: usize,
        callback: impl FnMut(&Value) -> Result<Value, String>,
    ) -> Result<Value, String> {
        let tripped = std::cell::Cell::new(false);
        let watched = || check().inspect_err(|_| tripped.set(true));
        let result = self.exchange(value, end, &watched, max_callbacks, callback);
        if tripped.get() && self.in_flight {
            self.settle_interrupted();
        }
        self.in_flight = false;
        result
    }
    fn settle_interrupted(&mut self) {
        let deadline = Instant::now() + INTERRUPT_GRACE;
        let mut buffer = [0u8; 16384];
        let mut complete = self.pending.contains(&b'\n');
        while !complete && Instant::now() < deadline && self.alive() {
            self.drain_stderr();
            match crate::platform::io::read_nonblocking(&mut self.output, &mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if self.pending.len() + n > MAX_FRAME {
                        break;
                    }
                    self.pending.extend_from_slice(&buffer[..n]);
                    complete = buffer[..n].contains(&b'\n');
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.poll_output(),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }
    fn exchange(
        &mut self,
        mut value: Value,
        end: Option<Instant>,
        check: &impl Fn() -> Result<(), String>,
        max_callbacks: usize,
        mut callback: impl FnMut(&Value) -> Result<Value, String>,
    ) -> Result<Value, String> {
        check()?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("operation sequence exhausted")?;
        value["id"] = json!(self.sequence);
        self.write_frame(&value, end, &check)?;
        self.in_flight = true;
        let mut callbacks = 0usize;
        let mut scanned = 0;
        loop {
            check()?;
            self.drain_stderr();
            let mut buffer = [0u8; 16384];
            if end.is_some_and(|end| Instant::now() >= end) {
                return Err("execution deadline exceeded; namespace/session discarded".into());
            }
            if let Some(index) = self.pending[scanned..]
                .iter()
                .position(|b| *b == b'\n')
                .map(|index| scanned + index)
            {
                let line = self.pending.drain(..=index).collect::<Vec<_>>();
                scanned = 0;
                let reply: Value = serde_json::from_slice(&line)
                    .map_err(|e| format!("invalid adapter response: {e}"))?;
                if reply.get("callback").is_some() {
                    callbacks += 1;
                    if callbacks > max_callbacks {
                        return Err("operation callback limit exceeded".into());
                    }
                    let operation = reply.get("operation_id").and_then(Value::as_u64);
                    let identity = reply.get("callback_id").and_then(Value::as_u64);
                    let outcome = if operation != Some(self.sequence) {
                        Err("callback belongs to an expired operation".into())
                    } else if identity.is_none_or(|id| id <= self.callback_sequence) {
                        Err("callback identity is malformed or repeated".into())
                    } else {
                        self.callback_sequence = identity.unwrap();
                        check()?;
                        callback(&reply)
                    };
                    // A deadline/cancel while approval or a nested tool was pending must never resume code.
                    check()?;
                    if end.is_some_and(|end| Instant::now() >= end) {
                        return Err(
                            "execution deadline exceeded; namespace/session discarded".into()
                        );
                    }
                    let response = match outcome {
                        Ok(value) => {
                            json!({"callback_id":reply["callback_id"],"operation_id":self.sequence,"result":value})
                        }
                        Err(error) => {
                            json!({"callback_id":reply["callback_id"],"operation_id":self.sequence,"error":error})
                        }
                    };
                    self.write_frame(&response, end, &check)?;
                    continue;
                }
                if reply.get("id").and_then(Value::as_u64) != Some(self.sequence) {
                    return Err("adapter response sequence mismatch".into());
                }
                if let Some(error) = reply.get("error").and_then(Value::as_str) {
                    return Err(error.into());
                }
                let mut result = reply.get("result").cloned().unwrap_or(Value::Null);
                if !self.stderr.is_empty() {
                    if let Some(object) = result.as_object_mut() {
                        object.insert(
                            "process_output".into(),
                            json!(String::from_utf8_lossy(&self.stderr)),
                        );
                        if self.stderr_dropped > 0 {
                            object.insert(
                                "process_output_truncated_bytes".into(),
                                json!(self.stderr_dropped),
                            );
                        }
                    }
                    self.stderr.clear();
                    self.stderr_dropped = 0;
                }
                check()?;
                if end.is_some_and(|end| Instant::now() >= end) {
                    return Err("execution deadline exceeded; namespace/session discarded".into());
                }
                return Ok(result);
            }
            // Previously inspected bytes cannot acquire a newline. Rescanning a growing
            // unterminated frame makes rejection quadratic and can starve the deadline.
            scanned = self.pending.len();
            match crate::platform::io::read_nonblocking(&mut self.output, &mut buffer) {
                Ok(0) => {
                    return Err(format!(
                        "execution process exited: {}",
                        String::from_utf8_lossy(&self.stderr)
                    ));
                }
                Ok(n) => {
                    if self.pending.len() + n > MAX_FRAME {
                        return Err("adapter output exceeded frame limit".into());
                    }
                    self.pending.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.poll_output(),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}
impl Drop for JsonProcess {
    fn drop(&mut self) {
        self.group.terminate();
        self.pid.store(0, Ordering::Release);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fake(source: &str) -> JsonProcess {
        let mut command = Command::new("python3");
        command.args(["-u", "-c", source]);
        JsonProcess::start(command, Arc::new(AtomicI32::new(0))).unwrap()
    }

    #[test]
    fn json_transport_round_trip_and_owner_drop() {
        let mut process = fake(
            "import sys,json\nfor line in sys.stdin:\n r=json.loads(line);print(json.dumps({'id':r['id'],'result':{'value':r['value']}}),flush=True)",
        );
        let pid = process.pid.clone();
        assert_eq!(
            process
                .request(
                    json!({"value":42}),
                    Duration::from_secs(2),
                    |_| unreachable!()
                )
                .unwrap()["value"],
            42
        );
        drop(process);
        assert_eq!(pid.load(Ordering::Acquire), 0);
    }

    #[test]
    fn json_transport_stalled_writes_observe_deadline_and_cancel() {
        for cancelled in [false, true] {
            let mut process = fake("import time; time.sleep(30)");
            let start = Instant::now();
            let error = process
                .request_guarded(
                    json!({"value":"x".repeat(1024 * 1024)}),
                    Some(start + Duration::from_millis(200)),
                    || {
                        if cancelled && start.elapsed() >= Duration::from_millis(50) {
                            Err("cancelled".into())
                        } else {
                            Ok(())
                        }
                    },
                    0,
                    |_| unreachable!(),
                )
                .unwrap_err();
            assert!(
                error.contains(if cancelled { "cancelled" } else { "deadline" }),
                "{error}"
            );
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn json_transport_rejects_unterminated_oversized_frame() {
        let mut process = fake(
            "import sys,time; sys.stdin.readline(); sys.stdout.write('x' * (13 * 1024 * 1024)); sys.stdout.flush(); time.sleep(30)",
        );
        let error = process
            .request(json!({}), Duration::from_secs(5), |_| unreachable!())
            .unwrap_err();
        assert!(error.contains("frame limit"), "{error}");
    }

    #[test]
    fn json_transport_stale_callback_never_reaches_controller() {
        let mut process = fake(
            "import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'callback':'tool','operation_id':0,'callback_id':1}),flush=True)\na=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':a}),flush=True)",
        );
        let result = process
            .request(json!({}), Duration::from_secs(2), |_| {
                panic!("stale callback dispatched")
            })
            .unwrap();
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("expired operation")
        );
    }
}
