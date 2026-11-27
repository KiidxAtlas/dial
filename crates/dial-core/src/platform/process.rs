use std::io;
use std::process::{Child, Command, ExitStatus};

/// Spawn an ordinary child without changing its process group. Shares Unix pipe/fork
/// serialization with [`spawn_group`], including commands configured to detach with `setsid`.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        super::imp::process::spawn(command)
    }
    #[cfg(windows)]
    {
        command.spawn()
    }
}

/// Stop Windows from showing modal error dialogs, such as "The application was unable to start
/// correctly" when a DLL fails to load or the crash dialog, for this process and the children it
/// starts afterwards; children inherit the error mode. The failure becomes the child's exit
/// status, which the caller can report, instead of a popup that blocks the program. This is
/// process-wide, only adds flags and is idempotent. Does nothing on other platforms.
pub fn quiet_error_dialogs() {
    #[cfg(windows)]
    super::imp::process::quiet_error_dialogs();
}
/// Optional OS-enforced resource limits applied to a spawned process group.
/// Any `None` field leaves that resource unchanged from the parent environment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Maximum CPU time in seconds (`RLIMIT_CPU` on Unix; Job Object time limit on Windows).
    pub max_cpu_secs: Option<u64>,
    /// Maximum memory in bytes (`RLIMIT_AS` on Unix; Job Object commit memory limit on Windows).
    pub max_memory_bytes: Option<u64>,
    /// Maximum size of any single written file in bytes (`RLIMIT_FSIZE` on Unix).
    pub max_file_size_bytes: Option<u64>,
    /// Maximum open file descriptors (`RLIMIT_NOFILE` on Unix).
    pub max_open_files: Option<u64>,
    /// Maximum simultaneous processes (`RLIMIT_NPROC` on Unix; Job Object active processes on Windows).
    pub max_processes: Option<u32>,
}

impl ResourceLimits {
    pub const fn is_empty(&self) -> bool {
        self.max_cpu_secs.is_none()
            && self.max_memory_bytes.is_none()
            && self.max_file_size_bytes.is_none()
            && self.max_open_files.is_none()
            && self.max_processes.is_none()
    }
}


/// Spawn `command` as the leader of a new process group (Unix: `process_group(0)`; Windows: a
/// new console process group inside a Job Object assigned before the child runs any code).
/// On Windows this sets the command's creation flags; do not set them separately.
pub fn spawn_group(command: &mut Command) -> io::Result<Child> {
    super::imp::process::spawn_group(command)
}

/// Like [`spawn_group`], also installing any configured OS resource limits before child execution.
pub fn spawn_group_limited(command: &mut Command, limits: ResourceLimits) -> io::Result<Child> {
    if limits.is_empty() {
        return spawn_group(command);
    }
    super::imp::process::spawn_group_limited(command, limits)
}

/// Apply any configured POSIX `setrlimit(2)` bounds inside a Unix `pre_exec` closure.
/// Async-signal-safe: invokes only `getrlimit`/`setrlimit` on stack values.
#[cfg(unix)]
pub fn apply_resource_limits(limits: ResourceLimits) -> io::Result<()> {
    if limits.is_empty() {
        return Ok(());
    }
    super::imp::process::apply_resource_limits(limits)
}

/// Spawn a console-independent Windows job shared by cooperating Dial processes. The final
/// handle close (including a crash) stops the server and all its descendants.
#[cfg(windows)]
pub fn spawn_shared_group(
    command: &mut Command,
) -> io::Result<(Child, std::os::windows::io::OwnedHandle)> {
    super::imp::process::spawn_shared_group(command)
}

/// Acquire a non-inheritable lease on the named job containing this exact server process.
#[cfg(windows)]
pub fn hold_shared_group(
    server: super::procinfo::ProcId,
) -> io::Result<std::os::windows::io::OwnedHandle> {
    super::imp::process::hold_shared_group(server)
}

/// Kill the group led by `leader` immediately (Unix: `killpg(SIGKILL)`; Windows:
/// `TerminateJobObject` for a group owned by this runtime).
pub fn kill_group(leader: u32) -> io::Result<()> {
    super::imp::process::kill_group(leader)
}

/// Ask the group led by `leader` to stop (Unix: `killpg(SIGTERM)`; Windows: `CTRL_BREAK` to
/// the console process group, falling back to [`kill_group`] when no console is shared).
pub fn terminate_group(leader: u32) -> io::Result<()> {
    super::imp::process::terminate_group(leader)
}

/// Terminate an identity-validated Windows handle without reopening its PID. An owned job
/// is terminated only if this exact process belongs to it.
#[cfg(windows)]
pub fn kill_checked(process: std::os::windows::io::BorrowedHandle<'_>) -> io::Result<()> {
    super::imp::process::kill_checked(process)
}

/// Whether a process with `pid` is running and this user may open it (`kill -0`). A PID that names
/// no process, or one that has exited, is `Ok(false)`. Windows only.
#[cfg(windows)]
pub fn process_exists(pid: u32) -> io::Result<bool> {
    super::imp::process::process_exists(pid)
}

/// Terminates the process `pid` with `exit_code` (`TerminateProcess`). Only that process is
/// terminated; its children are not. Fails with `NotFound` when no such process is running and
/// `PermissionDenied` when this user may not terminate it. Windows only.
#[cfg(windows)]
pub fn terminate_pid(pid: u32, exit_code: u32) -> io::Result<()> {
    super::imp::process::terminate_pid(pid, exit_code)
}

/// Sends `signal` to the process `pid` (`kill(2)`); signal 0 only checks that it exists. Fails
/// with `NotFound` when no such process exists and `PermissionDenied` when this user may not
/// signal it. Unix only.
#[cfg(unix)]
pub fn signal_pid(pid: u32, signal: i32) -> io::Result<()> {
    super::imp::process::signal_pid(pid, signal)
}

#[cfg(any(test, feature = "test-support"))]
/// Whether `pid` names a live process this user may signal (Unix: `kill(pid, 0) == 0`).
pub fn alive(pid: u32) -> bool {
    super::imp::process::alive(pid)
}

/// Whether `pid` leads its own process group (Unix: `getpgid(pid) == pid`; Windows: `pid` is
/// inside the Job Object Dial created for it).
pub fn is_group_leader(pid: u32) -> bool {
    super::imp::process::is_group_leader(pid)
}

/// Whether `pid` belongs to the process group led by `leader`: inside the Job Object Dial created
/// for that group, descendants included. Used to prove a listener's ownership.
#[cfg(windows)]
pub fn in_group(leader: u32, pid: u32) -> bool {
    super::imp::process::in_group(leader, pid)
}

/// True once `child` has exited, without reaping it, so its PID stays reserved (Unix:
/// `waitid(WNOWAIT)`; Windows: the process handle, held by `Child`, is signalled).
pub fn exited(child: &Child) -> io::Result<bool> {
    super::imp::process::exited(child)
}

/// Blocks until `child` exits or `timeout` elapses; the child is not reaped. Returns whether
/// it exited. This is the event-driven wait used in place of a polling loop.
pub fn wait_exited(child: &Child, timeout: std::time::Duration) -> io::Result<bool> {
    super::imp::process::wait_exited(child, timeout)
}

/// The signal that terminated the process, if any (always `None` on Windows).
pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
    super::imp::process::exit_signal(status)
}

/// Release bookkeeping for a reaped group leader (Windows: the Job Object handle).
pub fn forget_group(leader: u32) {
    super::imp::process::forget_group(leader)
}

/// Sends `signal` to every member of the process group `leader` (`killpg(2)`); signal 0 only checks
/// that the group exists. Fails with `NotFound` when no such group exists. Unix only.
#[cfg(unix)]
pub fn signal_group(leader: u32, signal: i32) -> io::Result<()> {
    super::imp::process::signal_group(leader, signal)
}

/// Terminates the process `pid` and every descendant of it with `exit_code`, the Windows meaning
/// of a signal to a process group (`cmd.exe` launching `node.exe` leaves no orphan). Fails with
/// `NotFound` when `pid` names no running process. Windows only.
#[cfg(windows)]
pub fn terminate_tree(pid: u32, exit_code: u32) -> io::Result<()> {
    super::imp::process::terminate_tree(pid, exit_code)
}

#[cfg(all(test, unix))]
mod tests;
