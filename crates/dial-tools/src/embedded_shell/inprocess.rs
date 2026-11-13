//! In-process execution of the built-in shell (`shell.backend = "inprocess"`).
//!
//! Unlike the `embedded` backend, which hosts brush in a re-executed child `dial` process, this
//! runs the script on a dedicated thread of the Dial process itself. Nothing separates the shell
//! from Dial, so the caller must start it only when the OS sandbox is off; the backend selection
//! in `tools::exec::shell` enforces that and refuses every other sandbox mode.
//!
//! Stopping a run is cooperative for the shell and forceful for what it launched:
//!
//! * the script task is aborted, so no further builtin or command runs once the shell reaches an
//!   `await` (every external command, pipe and sleep is one);
//! * every external command carries a per-run token in its environment; on a stop, Unix kills
//!   the process groups led by a direct child of Dial that carry it (found with `ps`; each
//!   command starts in its own group), and Windows terminates every process started during the
//!   run whose environment block holds it (Toolhelp snapshot plus a PEB read, one handle per check
//!   and kill), which also reaches descendants that inherited the environment.
//!
//! Limits: a builtin loop that never awaits (for example `while :; do :; done`) cannot be
//! interrupted from outside its thread, so the run is abandoned: Dial returns, and the thread
//! keeps spinning until the process exits. A command that leaves its process group (`setsid`) or
//! scrubs the token from its environment survives a stop. Use the `system` or `embedded` backend
//! when a hard kill matters.

use std::io::PipeWriter;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

/// Environment variable that marks everything one run starts.
pub const TOKEN_ENV: &str = "DIAL_INPROCESS_RUN";
/// Exit code reported when the shell thread died without producing a result.
const PANICKED: i32 = 125;
/// Exit code reported when the script task was aborted (an interrupted shell).
#[cfg(feature = "embedded-shell")]
const ABORTED: i32 = 130;
/// How long a stopped run may take to wind down before it is abandoned.
const STOP_GRACE: Duration = Duration::from_millis(500);
const CLEANUP_GRACE: Duration = Duration::from_millis(500);
#[cfg(unix)]
const CLEANUP_OUTPUT_CAP: usize = 1024 * 1024;

/// Shell completion and external-command cleanup are independent facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Termination {
    pub shell_stopped: bool,
    pub cleanup_error: Option<String>,
}

impl Termination {
    pub fn uncertain(&self) -> bool {
        !self.shell_stopped || self.cleanup_error.is_some()
    }
}

/// What to run, and the environment and output the shell gets.
pub struct Request {
    pub script: String,
    pub cwd: PathBuf,
    /// The complete environment; nothing is inherited from the Dial process.
    pub env: Vec<(String, String)>,
    /// Receives both stdout and stderr. Stdin is empty.
    pub output: PipeWriter,
}

/// A running (or finished) in-process script.
pub struct Run {
    done: Receiver<i32>,
    code: Option<i32>,
    token: String,
    /// When the run started, as a FILETIME: Windows cleanup only considers newer processes.
    #[cfg(windows)]
    started: u64,
    termination: Option<Termination>,
    #[cfg(feature = "embedded-shell")]
    abort: tokio::task::AbortHandle,
}

impl Run {
    /// Starts `request` on a new thread.
    #[cfg(feature = "embedded-shell")]
    pub fn start(mut request: Request) -> Result<Self, String> {
        let token = format!("{:032x}", rand::random::<u128>());
        #[cfg(windows)]
        let started = crate::platform::processes::filetime_now();
        request.env.push((TOKEN_ENV.to_owned(), token.clone()));
        let (done_tx, done) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("dial-inprocess-shell".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!(
                            "cannot start the in-process shell runtime: {error}"
                        )));
                        return;
                    }
                };
                let task = runtime.spawn(execute(request));
                let _ = ready_tx.send(Ok(task.abort_handle()));
                let code = match runtime.block_on(task) {
                    Ok(code) => code,
                    Err(error) if error.is_cancelled() => ABORTED,
                    Err(_) => PANICKED,
                };
                // Drop whatever the shell spawned without waiting for stuck tasks.
                runtime.shutdown_background();
                let _ = done_tx.send(code);
            })
            .map_err(|error| format!("cannot start the in-process shell thread: {error}"))?;
        let abort = ready_rx
            .recv()
            .map_err(|_| "the in-process shell thread ended before it started".to_string())??;
        Ok(Self {
            done,
            code: None,
            token,
            #[cfg(windows)]
            started,
            termination: None,
            abort,
        })
    }

    #[cfg(not(feature = "embedded-shell"))]
    pub fn start(_request: Request) -> Result<Self, String> {
        Err(
            "the in-process shell is not in this build (the embedded-shell feature is disabled)"
                .into(),
        )
    }

    /// Whether the script has finished.
    pub fn exited(&mut self) -> bool {
        if self.code.is_some() {
            return true;
        }
        match self.done.try_recv() {
            Ok(code) => self.code = Some(code),
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => self.code = Some(PANICKED),
        }
        true
    }

    /// The script's exit code once it has finished.
    pub fn code(&self) -> Option<i32> {
        self.code
    }

    /// Stops the script and attempts bounded external cleanup. Cached, including on Drop.
    pub fn terminate(&mut self) -> Termination {
        #[cfg(windows)]
        {
            let started = self.started;
            self.terminate_with(|token, deadline| kill_marked_processes(token, started, deadline))
        }
        #[cfg(not(windows))]
        self.terminate_with(kill_marked_groups)
    }

    fn terminate_with(
        &mut self,
        cleanup: impl FnOnce(&str, Instant) -> Result<(), String>,
    ) -> Termination {
        if let Some(result) = &self.termination {
            return result.clone();
        }
        #[cfg(feature = "embedded-shell")]
        self.abort.abort();
        if self.code.is_none() {
            match self.done.recv_timeout(STOP_GRACE) {
                Ok(code) => self.code = Some(code),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.code = Some(PANICKED),
            }
        }
        let result = Termination {
            shell_stopped: self.code.is_some(),
            cleanup_error: cleanup(&self.token, Instant::now() + CLEANUP_GRACE).err(),
        };
        self.termination = Some(result.clone());
        result
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
fn kill_marked_groups(token: &str, deadline: Instant) -> Result<(), String> {
    let mut budget = CLEANUP_OUTPUT_CAP;
    cleanup_groups(
        token,
        deadline,
        &mut budget,
        group_leaders,
        marked_leaders,
        crate::platform::process::kill_group,
    )
}

#[cfg(unix)]
fn cleanup_groups(
    token: &str,
    deadline: Instant,
    budget: &mut usize,
    mut discover: impl FnMut(Instant, &mut usize) -> Result<Vec<u32>, String>,
    mut marked: impl FnMut(&[u32], &str, Instant, &mut usize) -> Result<Vec<u32>, String>,
    mut kill: impl FnMut(u32) -> std::io::Result<()>,
) -> Result<(), String> {
    for _ in 0..3 {
        remaining(deadline)?;
        let leaders = discover(deadline, budget)?;
        if leaders.is_empty() {
            return Ok(());
        }
        remaining(deadline)?;
        // One environment check for the whole batch: Dial may lead many unrelated groups
        // (background jobs, concurrent runs), and a per-leader `ps` would exhaust the deadline.
        let found = marked(&leaders, token, deadline, budget)?;
        for leader in &found {
            kill(*leader).map_err(|e| format!("killing marked process group: {e}"))?;
        }
        if found.is_empty() {
            return Ok(());
        }
    }
    Err("marked process groups remained after cleanup attempts".into())
}

/// Terminate every process started since `started` whose environment holds this run's token,
/// repeating until a pass finds none (a dying parent may still start a child).
#[cfg(windows)]
fn kill_marked_processes(token: &str, started: u64, deadline: Instant) -> Result<(), String> {
    use crate::platform::processes;
    let entry = format!("{TOKEN_ENV}={token}");
    let own = std::process::id();
    for _ in 0..3 {
        remaining(deadline)?;
        let mut found = false;
        let list = processes::snapshot().map_err(|e| format!("listing processes: {e}"))?;
        for process in list {
            if process.pid == own || process.pid <= 4 {
                continue;
            }
            remaining(deadline)?;
            found |= processes::kill_marked(process.pid, &entry, started)
                .map_err(|e| format!("terminating marked process {}: {e}", process.pid))?;
        }
        if !found {
            return Ok(());
        }
    }
    Err("marked processes remained after cleanup attempts".into())
}

#[cfg(not(any(unix, windows)))]
fn kill_marked_groups(_token: &str, _deadline: Instant) -> Result<(), String> {
    Err("external-command cleanup is unavailable on this platform".into())
}

#[cfg(any(unix, windows))]
fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "external-command cleanup deadline exceeded".into())
}

/// All discovery commands share a deadline and an aggregate output budget.
#[cfg(unix)]
fn discovery_output(
    mut command: std::process::Command,
    deadline: Instant,
    budget: &mut usize,
) -> Result<Vec<u8>, String> {
    use crate::runtime::process::{Policy, run_with_policy};
    if *budget == 0 {
        return Err("external-command discovery output budget exhausted".into());
    }
    command.stdin(std::process::Stdio::null());
    let mut policy = Policy::bounded(remaining(deadline)?, *budget);
    policy.stderr_cap = 0;
    policy.fail_on_output_limit = true;
    policy.may_mutate = false;
    let result = run_with_policy(
        command,
        None,
        policy,
        &std::sync::atomic::AtomicBool::new(false),
        &mut |_, _| {},
    )?;
    *budget = budget.saturating_sub(result.stdout_total.saturating_add(result.stderr_total));
    if !result.success() || result.output_incomplete {
        return Err(result.failure("external-command discovery", policy.timeout));
    }
    remaining(deadline)?;
    Ok(result.stdout)
}

/// Direct children of Dial that lead their own process group.
#[cfg(unix)]
fn group_leaders(deadline: Instant, budget: &mut usize) -> Result<Vec<u32>, String> {
    let mut command = std::process::Command::new("ps");
    command.args(["-A", "-o", "pid=,ppid=,pgid="]);
    let output = discovery_output(command, deadline, budget)?;
    Ok(parse_leaders(
        &String::from_utf8_lossy(&output),
        std::process::id(),
    ))
}

/// Leaders among `ps -o pid=,ppid=,pgid=` rows: processes whose parent is `parent` and that lead
/// their own process group.
#[cfg(unix)]
pub(crate) fn parse_leaders(ps: &str, parent: u32) -> Vec<u32> {
    ps.lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace().map(str::parse::<u32>);
            let (pid, ppid, pgid) = (
                columns.next()?.ok()?,
                columns.next()?.ok()?,
                columns.next()?.ok()?,
            );
            (ppid == parent && pid == pgid).then_some(pid)
        })
        .collect()
}

/// The `leaders` whose environment holds the run token; failures are not evidence of absence.
#[cfg(unix)]
fn marked_leaders(
    leaders: &[u32],
    token: &str,
    deadline: Instant,
    budget: &mut usize,
) -> Result<Vec<u32>, String> {
    let needle = format!("{TOKEN_ENV}={token}");
    if cfg!(target_os = "linux") {
        use std::io::Read;
        let mut marked = Vec::new();
        for &pid in leaders {
            remaining(deadline)?;
            let file = match std::fs::File::open(format!("/proc/{pid}/environ")) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("reading process environment: {e}")),
            };
            let mut bytes = Vec::new();
            file.take(budget.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("reading process environment: {e}"))?;
            if bytes.len() > *budget {
                return Err("external-command discovery output budget exhausted".into());
            }
            *budget -= bytes.len();
            if bytes.split(|b| *b == 0).any(|var| var == needle.as_bytes()) {
                marked.push(pid);
            }
        }
        remaining(deadline)?;
        return Ok(marked);
    }
    let mut pids: Vec<String> = leaders.iter().map(u32::to_string).collect();
    // Include Dial so `ps` succeeds even when every candidate disappeared since discovery.
    // A missing row is evidence of exit, not a ps failure.
    pids.push(std::process::id().to_string());
    let mut command = std::process::Command::new("ps");
    command.args(["-E", "-ww", "-o", "pid=,command=", "-p", &pids.join(",")]);
    let output = discovery_output(command, deadline, budget)?;
    let output = String::from_utf8_lossy(&output);
    Ok(leaders
        .iter()
        .copied()
        .filter(|&pid| token_in_ps(&output, pid, &needle))
        .collect())
}

#[cfg(unix)]
fn token_in_ps(output: &str, pid: u32, needle: &str) -> bool {
    output.lines().any(|line| {
        let mut columns = line.split_whitespace();
        columns.next().and_then(|s| s.parse::<u32>().ok()) == Some(pid)
            && columns.any(|var| var == needle)
    })
}

#[cfg(feature = "embedded-shell")]
async fn execute(request: Request) -> i32 {
    use brush_builtins::{BuiltinSet, ShellBuilderExt};
    use brush_core::openfiles::{self, OpenFile, OpenFiles};
    use brush_core::{ProcessGroupPolicy, SourceInfo};
    use std::io::Write;

    let Request {
        script,
        cwd,
        env,
        output,
    } = request;
    let mut report = match output.try_clone() {
        Ok(report) => report,
        Err(_) => return 2,
    };
    let mut fail = |message: String| {
        let _ = writeln!(report, "dial: {message}");
        2
    };
    let stderr = match output.try_clone() {
        Ok(stderr) => stderr,
        Err(error) => return fail(format!("cannot start the in-process shell: {error}")),
    };
    let stdin = match openfiles::null() {
        Ok(stdin) => stdin,
        Err(error) => return fail(format!("cannot start the in-process shell: {error}")),
    };
    let fds = [
        (OpenFiles::STDIN_FD, stdin),
        (OpenFiles::STDOUT_FD, OpenFile::from(output)),
        (OpenFiles::STDERR_FD, OpenFile::from(stderr)),
    ]
    .into_iter()
    .collect();
    let mut shell = match brush_core::Shell::builder()
        .default_builtins(BuiltinSet::BashMode)
        .builtins(super::builtins::registrations())
        .profile(brush_core::ProfileLoadBehavior::Skip)
        .rc(brush_core::RcLoadBehavior::Skip)
        .interactive(false)
        .shell_name("bash".to_owned())
        .do_not_inherit_env(true)
        .working_dir(cwd)
        .fds(fds)
        .build()
        .await
    {
        Ok(shell) => shell,
        Err(error) => return fail(format!("cannot start the in-process shell: {error}")),
    };
    for (name, value) in env {
        let mut variable = brush_core::variables::ShellVariable::new(value);
        variable.export();
        if let Err(error) = shell.env_mut().set_global(&name, variable) {
            return fail(format!("setting {name}: {error}"));
        }
    }
    let mut params = shell.default_exec_params();
    // Each external command leads its own group so a stop can kill it without touching Dial.
    params.process_group_policy = ProcessGroupPolicy::NewProcessGroup;
    match shell
        .run_string(script, &SourceInfo::default(), &params)
        .await
    {
        Ok(result) => i32::from(u8::from(result.exit_code)),
        Err(error) => fail(error.to_string()),
    }
}

#[cfg(test)]
mod tests;
