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
//! * every external command starts in its own process group and carries a per-run token in its
//!   environment; on a stop the groups led by a direct child of Dial that carry the token are
//!   killed (found with `ps`, Unix only; on Windows only the task abort applies).
//!
//! Limits: a builtin loop that never awaits (for example `while :; do :; done`) cannot be
//! interrupted from outside its thread, so the run is abandoned: Dial returns, and the thread
//! keeps spinning until the process exits. A command that leaves its process group (`setsid`) or
//! scrubs the token from its environment survives a stop. Use the `system` or `embedded` backend
//! when a hard kill matters.

use std::io::PipeWriter;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

/// Environment variable that marks everything one run starts.
pub const TOKEN_ENV: &str = "DIAL_INPROCESS_RUN";
/// Exit code reported when the shell thread died without producing a result.
const PANICKED: i32 = 125;
/// Exit code reported when the script task was aborted (an interrupted shell).
#[cfg(feature = "embedded-shell")]
const ABORTED: i32 = 130;
/// How long a stopped run may take to wind down before it is abandoned.
const STOP_GRACE: Duration = Duration::from_millis(500);

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
    #[cfg(feature = "embedded-shell")]
    abort: tokio::task::AbortHandle,
}

impl Run {
    /// Starts `request` on a new thread.
    #[cfg(feature = "embedded-shell")]
    pub fn start(mut request: Request) -> Result<Self, String> {
        let token = format!("{:032x}", rand::random::<u128>());
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

    /// Stops the script and kills what it started. Safe to repeat and after normal completion.
    /// Returns `false` when the shell thread did not stop and was abandoned.
    pub fn terminate(&mut self) -> bool {
        #[cfg(feature = "embedded-shell")]
        self.abort.abort();
        if self.code.is_none() {
            match self.done.recv_timeout(STOP_GRACE) {
                Ok(code) => self.code = Some(code),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.code = Some(PANICKED),
            }
        }
        kill_marked_groups(&self.token);
        self.code.is_some()
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
fn kill_marked_groups(token: &str) {
    for _ in 0..3 {
        let marked: Vec<u32> = group_leaders(std::process::id())
            .into_iter()
            .filter(|pid| carries_token(*pid, token))
            .collect();
        if marked.is_empty() {
            return;
        }
        for leader in marked {
            let _ = crate::platform::process::kill_group(leader);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(not(unix))]
fn kill_marked_groups(_token: &str) {}

/// Direct children of `parent` that lead their own process group.
#[cfg(unix)]
fn group_leaders(parent: u32) -> Vec<u32> {
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            parse_leaders(&String::from_utf8_lossy(&output.stdout), parent)
        }
        _ => Vec::new(),
    }
}

/// Leaders among `ps -o pid=,ppid=,pgid=` rows: processes whose parent is `parent` and that lead
/// their own process group.
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

/// Whether the process's environment holds this run's token (Linux: `/proc`; macOS: `ps -E`).
#[cfg(unix)]
fn carries_token(pid: u32, token: &str) -> bool {
    let needle = format!("{TOKEN_ENV}={token}");
    if cfg!(target_os = "linux") {
        return std::fs::read(format!("/proc/{pid}/environ"))
            .map(|bytes| bytes.split(|b| *b == 0).any(|var| var == needle.as_bytes()))
            .unwrap_or(false);
    }
    std::process::Command::new("ps")
        .args(["-E", "-ww", "-o", "command=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains(&needle))
        .unwrap_or(false)
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
