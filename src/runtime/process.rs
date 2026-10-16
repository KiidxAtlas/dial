//! Bounded operator subprocesses. Callers own environment and effect policy; this module owns
//! pipes, deadlines, cancellation and process-group cleanup, including unwinding callbacks.
use std::{
    io::{self, Write},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Kills the whole process group on drop so nothing outlives its owning operation.
pub(crate) struct ProcessGroup {
    pub(crate) child: Child,
    terminated: bool,
}
impl ProcessGroup {
    pub(crate) fn new(child: Child) -> Self {
        Self {
            child,
            terminated: false,
        }
    }
    pub(crate) fn terminate(&mut self) {
        if !self.terminated {
            let _ = crate::platform::process::terminate_group(self.child.id());
            let _ = crate::platform::process::wait_exited(&self.child, Duration::from_millis(100));
            let _ = crate::platform::process::kill_group(self.child.id());
            self.terminated = true;
        }
    }
    /// Transfers ownership without killing or forgetting the process group.
    pub(crate) fn release(mut self) -> Child {
        self.terminated = true;
        let group = std::mem::ManuallyDrop::new(self);
        unsafe { std::ptr::read(&group.child) }
    }
    /// The exited leader is not reaped, keeping its PID reserved until cleanup.
    pub(crate) fn exited(&self) -> io::Result<bool> {
        crate::platform::process::exited(&self.child)
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.wait();
        crate::platform::process::forget_group(self.child.id());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub timeout: Duration,
    pub stdout_cap: usize,
    pub stderr_cap: usize,
    /// Diagnostics normally retain their tail; protocol hooks can explicitly retain the prefix.
    pub stderr_tail: bool,
    /// Observational hooks may exit deliberately without consuming their event payload.
    pub allow_unconsumed_stdin: bool,
    /// SSH connection masters may survive a successful leader; ordinary commands must not.
    pub retain_descendants: bool,
    /// Detach from the controlling terminal; SSH must never wait for a /dev/tty prompt.
    pub detached: bool,
    pub fail_on_output_limit: bool,
    /// Unknown external commands are conservatively effectful; fixed read operations opt out.
    pub may_mutate: bool,
}
impl Policy {
    pub fn bounded(timeout: Duration, stdout_cap: usize) -> Self {
        Self {
            timeout,
            stdout_cap,
            stderr_cap: 64 * 1024,
            stderr_tail: true,
            allow_unconsumed_stdin: false,
            retain_descendants: false,
            detached: false,
            fail_on_output_limit: false,
            may_mutate: true,
        }
    }
    pub fn ssh(timeout: Duration, stdout_cap: usize) -> Self {
        Self {
            retain_descendants: true,
            detached: true,
            ..Self::bounded(timeout, stdout_cap)
        }
    }
}

#[derive(Debug)]
pub struct Finished {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stdout_total: usize,
    pub stderr: Vec<u8>,
    pub stderr_total: usize,
    pub timed_out: bool,
    pub cancelled: bool,
    pub started: bool,
    pub output_limited: bool,
    pub output_incomplete: bool,
    pub io_error: Option<String>,
}
impl Finished {
    pub fn success(&self) -> bool {
        !self.timed_out
            && !self.cancelled
            && !self.output_limited
            && self.io_error.is_none()
            && self.status.is_some_and(|status| status.success())
    }
    pub fn code(&self) -> Option<i32> {
        self.status.and_then(|status| status.code())
    }
    pub fn tool_status(&self) -> super::execution::ToolStatus {
        use super::execution::ToolStatus;
        if self.cancelled {
            ToolStatus::Aborted
        } else if self.timed_out {
            ToolStatus::Timeout
        } else if self.success() {
            ToolStatus::Ok
        } else {
            ToolStatus::Error
        }
    }
    pub fn execution(&self, may_mutate: bool) -> super::execution::Execution {
        // A timeout whose leader was reaped after the group kill is an ordinary failure: nothing
        // still runs and any partial effects are on disk to inspect, like a non-zero exit.
        let reaped_timeout = self.timed_out && self.status.is_some();
        super::execution::Execution {
            status: Some(self.tool_status()),
            effects: if !self.started || !may_mutate {
                super::execution::Effects::None
            } else if self.cancelled
                || (self.timed_out && !reaped_timeout)
                || self.output_limited
                || self.io_error.is_some()
            {
                super::execution::Effects::Unknown
            } else {
                super::execution::Effects::Known
            },
            changed_paths: Vec::new(),
        }
    }
    pub fn stdout_text(&self) -> String {
        self.text(Stream::Stdout)
    }
    pub fn text(&self, stream: Stream) -> String {
        let (bytes, total) = match stream {
            Stream::Stdout => (&self.stdout, self.stdout_total),
            Stream::Stderr => (&self.stderr, self.stderr_total),
        };
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        if total > bytes.len() {
            text.push_str(&format!(
                "\n[{} bytes omitted by the capture limit]",
                total - bytes.len()
            ));
        }
        if self.output_incomplete {
            text.push_str("\n[output may be incomplete: a background process kept the pipe open]");
        }
        text
    }
    pub fn stderr_text(&self, max: usize) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        let text = text.trim();
        if text.len() <= max {
            return text.to_owned();
        }
        let mut start = text.len() - max;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        format!("…{}", &text[start..])
    }
    pub fn outcome(&self) -> super::execution::ProcessOutcome {
        super::execution::ProcessOutcome {
            started: self.started,
            exit_code: self.code(),
            signal: self
                .status
                .as_ref()
                .and_then(crate::platform::process::exit_signal),
            timed_out: self.timed_out,
            cancelled: self.cancelled,
            output_complete: !self.output_incomplete
                && !self.output_limited
                && self.stdout_total == self.stdout.len()
                && self.stderr_total == self.stderr.len()
                && self.io_error.is_none(),
        }
    }
    pub fn failure(&self, what: &str, timeout: Duration) -> String {
        if self.cancelled {
            return format!("{what}: cancelled");
        }
        if self.timed_out {
            return format!("{what}: timed out after {}s", timeout.as_secs());
        }
        if self.output_limited {
            return format!("{what}: output exceeds the capture limit");
        }
        if let Some(error) = &self.io_error {
            return format!("{what}: {error}");
        }
        let detail = self.stderr_text(2000);
        let code = self.code().map_or_else(
            || "terminated by a signal".into(),
            |n| format!("exit code {n}"),
        );
        if detail.is_empty() {
            format!("{what}: {code}")
        } else {
            format!("{what}: {detail} ({code})")
        }
    }
}

struct OwnedChild {
    child: Child,
    reaped: bool,
}
impl OwnedChild {
    fn kill(&mut self) {
        let _ = crate::platform::process::kill_group(self.child.id());
        let _ = self.child.kill();
    }
    fn reap(&mut self) -> Option<ExitStatus> {
        let status = self.child.wait().ok();
        self.reaped = status.is_some();
        if self.reaped {
            crate::platform::process::forget_group(self.child.id());
        }
        status
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill();
            let _ = self.reap();
        }
    }
}

#[cfg(unix)]
type Input = ChildStdin;
#[cfg(windows)]
type Input = crate::platform::io::QueuedWriter<ChildStdin>;
fn input(pipe: ChildStdin) -> io::Result<Input> {
    #[cfg(unix)]
    {
        crate::platform::io::set_nonblocking(&pipe)?;
        Ok(pipe)
    }
    #[cfg(windows)]
    {
        Ok(crate::platform::io::QueuedWriter::new(pipe, 64 * 1024))
    }
}

pub fn run(
    command: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    stdout_cap: usize,
    cancel: &AtomicBool,
    on_chunk: &mut dyn FnMut(Stream, &[u8]),
) -> Result<Finished, String> {
    run_with_policy(
        command,
        stdin,
        Policy::bounded(timeout, stdout_cap),
        cancel,
        on_chunk,
    )
}

pub fn run_with_policy(
    command: Command,
    stdin: Option<Vec<u8>>,
    policy: Policy,
    cancel: &AtomicBool,
    on_chunk: &mut dyn FnMut(Stream, &[u8]),
) -> Result<Finished, String> {
    run_with_control(
        command,
        stdin,
        policy,
        &|| cancel.load(Ordering::Acquire),
        on_chunk,
    )
}

pub fn run_with_control(
    mut command: Command,
    stdin: Option<Vec<u8>>,
    policy: Policy,
    cancelled: &dyn Fn() -> bool,
    on_chunk: &mut dyn FnMut(Stream, &[u8]),
) -> Result<Finished, String> {
    let mut result = Finished {
        status: None,
        stdout: Vec::new(),
        stdout_total: 0,
        stderr: Vec::new(),
        stderr_total: 0,
        timed_out: false,
        cancelled: false,
        started: false,
        output_limited: false,
        output_incomplete: false,
        io_error: None,
    };
    if cancelled() {
        result.cancelled = true;
        super::execution::process_status(super::execution::ToolStatus::Aborted);
        return Ok(result);
    }
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    if policy.detached {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; no parent memory is touched.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(unix)]
    let child = if policy.detached {
        command.spawn()
    } else {
        crate::platform::process::spawn_group(&mut command)
    };
    #[cfg(not(unix))]
    let child = crate::platform::process::spawn_group(&mut command);
    let mut owner = OwnedChild {
        child: child.map_err(|error| format!("starting {program}: {error}"))?,
        reaped: false,
    };
    result.started = true;
    let mut out = owner.child.stdout.take();
    let mut err = owner.child.stderr.take();
    let setup = (|| -> io::Result<Option<Input>> {
        if let Some(pipe) = &out {
            crate::platform::io::set_nonblocking(pipe)?;
        }
        if let Some(pipe) = &err {
            crate::platform::io::set_nonblocking(pipe)?;
        }
        owner.child.stdin.take().map(input).transpose()
    })();
    let mut writer = match setup {
        Ok(writer) => writer,
        Err(error) => {
            result.io_error = Some(format!("preparing pipes: {error}"));
            owner.kill();
            result.status = owner.reap();
            if policy.may_mutate {
                super::execution::process_failure(result.tool_status());
            }
            return Ok(result);
        }
    };
    let bytes = stdin.unwrap_or_default();
    let mut written = 0;
    let started = Instant::now();
    let mut exited_at = None;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let mut progress = false;
        for stream in [Stream::Stdout, Stream::Stderr] {
            let read = match stream {
                Stream::Stdout => out
                    .as_mut()
                    .map(|pipe| crate::platform::io::read_nonblocking(pipe, &mut buffer)),
                Stream::Stderr => err
                    .as_mut()
                    .map(|pipe| crate::platform::io::read_nonblocking(pipe, &mut buffer)),
            };
            match read {
                Some(Ok(0)) => match stream {
                    Stream::Stdout => out = None,
                    Stream::Stderr => err = None,
                },
                Some(Ok(n)) => {
                    progress = true;
                    match stream {
                        Stream::Stdout => {
                            result.stdout_total = result.stdout_total.saturating_add(n);
                            let keep = n.min(policy.stdout_cap.saturating_sub(result.stdout.len()));
                            result.stdout.extend_from_slice(&buffer[..keep]);
                        }
                        Stream::Stderr => {
                            result.stderr_total = result.stderr_total.saturating_add(n);
                            if policy.stderr_tail {
                                result.stderr.extend_from_slice(&buffer[..n]);
                                let excess = result.stderr.len().saturating_sub(policy.stderr_cap);
                                if excess != 0 {
                                    result.stderr.drain(..excess);
                                }
                            } else {
                                let keep =
                                    n.min(policy.stderr_cap.saturating_sub(result.stderr.len()));
                                result.stderr.extend_from_slice(&buffer[..keep]);
                            }
                        }
                    }
                    on_chunk(stream, &buffer[..n]);
                }
                Some(Err(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Some(Err(error)) => result.io_error = Some(format!("reading {stream:?}: {error}")),
                None => {}
            }
        }
        let mut ignore_input = false;
        if let Some(pipe) = &mut writer {
            if written < bytes.len() {
                match pipe.write(&bytes[written..]) {
                    Ok(0) if policy.allow_unconsumed_stdin => ignore_input = true,
                    Ok(0) => result.io_error = Some("stdin closed before delivery".into()),
                    Ok(n) => {
                        written += n;
                        progress = true;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error)
                        if policy.allow_unconsumed_stdin
                            && error.kind() == io::ErrorKind::BrokenPipe =>
                    {
                        ignore_input = true
                    }
                    Err(error) => result.io_error = Some(format!("writing stdin: {error}")),
                }
            }
            if !ignore_input && written == bytes.len() {
                match pipe.flush() {
                    Ok(()) => writer = None,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error)
                        if policy.allow_unconsumed_stdin
                            && error.kind() == io::ErrorKind::BrokenPipe =>
                    {
                        ignore_input = true;
                    }
                    Err(error) => result.io_error = Some(format!("flushing stdin: {error}")),
                }
            }
        }
        if ignore_input {
            writer = None;
        }
        if exited_at.is_none() {
            match crate::platform::process::exited(&owner.child) {
                Ok(true) => {
                    // Reserve the leader PID until descendants are cleaned up.
                    if !policy.retain_descendants {
                        owner.kill();
                    }
                    if writer.is_some() && !policy.allow_unconsumed_stdin {
                        result.io_error = Some("process exited before stdin delivery".into());
                    }
                    result.status = owner.reap();
                    exited_at = Some(Instant::now());
                    writer = None;
                }
                Ok(false) => {}
                Err(error) => result.io_error = Some(format!("observing {program}: {error}")),
            }
        }
        result.cancelled |= cancelled();
        result.timed_out |=
            exited_at.is_none() && !policy.timeout.is_zero() && started.elapsed() >= policy.timeout;
        result.output_limited |= policy.fail_on_output_limit
            && (result.stdout_total > policy.stdout_cap || result.stderr_total > policy.stderr_cap);
        if result.cancelled
            || result.timed_out
            || result.output_limited
            || result.io_error.is_some()
        {
            if !owner.reaped {
                owner.kill();
                result.status = owner.reap();
            }
            break;
        }
        if let Some(at) = exited_at {
            if out.is_none() && err.is_none() {
                break;
            }
            if at.elapsed() >= Duration::from_millis(300) {
                result.output_incomplete = true;
                break;
            }
        }
        if !progress {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    if result.cancelled || result.timed_out {
        super::execution::process_status(result.tool_status());
    }
    if result.execution(policy.may_mutate).effects == super::execution::Effects::Unknown {
        super::execution::process_failure(result.tool_status());
    }
    Ok(result)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn captures_streams_with_caps_and_stdin() {
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        let done = run(
            sh("cat; echo err >&2; exit 3"),
            Some(b"hello world".to_vec()),
            Duration::from_secs(10),
            5,
            &cancel,
            &mut |stream, chunk| seen.push((stream, chunk.to_vec())),
        )
        .unwrap();
        assert_eq!(done.stdout, b"hello");
        assert_eq!(done.stdout_total, 11);
        assert!(!done.outcome().output_complete);
        assert_eq!(done.stderr_text(100), "err");
        assert_eq!(done.code(), Some(3));
        assert!(!done.success());
        assert!(seen.iter().any(|(s, _)| *s == Stream::Stderr));
        assert!(
            done.failure("x", Duration::from_secs(1))
                .contains("err (exit code 3)")
        );
    }

    #[test]
    fn deadline_kills_the_whole_group() {
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let done = run(
            sh("sleep 30 & sleep 30"),
            None,
            Duration::from_millis(200),
            10,
            &cancel,
            &mut |_, _| {},
        )
        .unwrap();
        assert!(done.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_daemon_holding_the_pipe_does_not_hang_the_caller() {
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        // The background job keeps stdout open after the shell exits, like `ssh -f`.
        let done = run(
            sh("(sleep 5) & echo ready"),
            None,
            Duration::from_secs(10),
            100,
            &cancel,
            &mut |_, _| {},
        )
        .unwrap();
        assert!(done.success());
        assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "ready");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn cancellation_before_start_runs_nothing() {
        let cancel = AtomicBool::new(true);
        let done = run(
            sh("exit 0"),
            None,
            Duration::from_secs(1),
            1,
            &cancel,
            &mut |_, _| {},
        )
        .unwrap();
        assert!(done.cancelled && done.status.is_none());
    }
}
