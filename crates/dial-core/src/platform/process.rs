use std::io;
use std::process::{Child, Command, ExitStatus};

/// Spawn `command` as the leader of a new process group (Unix: `process_group(0)`; Windows: a
/// new console process group inside a Job Object assigned before the child runs any code).
/// On Windows this sets the command's creation flags; do not set them separately.
pub fn spawn_group(command: &mut Command) -> io::Result<Child> {
    super::imp::process::spawn_group(command)
}

/// Kill the group led by `leader` immediately (Unix: `killpg(SIGKILL)`; Windows:
/// `TerminateJobObject`, or a creation-time-checked process-tree kill for a group spawned by
/// another Dial process that has since exited).
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

#[cfg(all(test, unix))]
mod tests;
