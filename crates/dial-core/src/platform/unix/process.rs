use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};

fn pid(value: u32) -> io::Result<libc::pid_t> {
    libc::pid_t::try_from(value)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process id"))
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(in crate::platform) fn spawn_group(command: &mut Command) -> io::Result<Child> {
    command.process_group(0).spawn()
}

pub(in crate::platform) fn kill_group(leader: u32) -> io::Result<()> {
    // SAFETY: killpg only signals; the caller owns the group it names.
    check(unsafe { libc::killpg(pid(leader)?, libc::SIGKILL) })
}

pub(in crate::platform) fn terminate_group(leader: u32) -> io::Result<()> {
    // SAFETY: killpg only signals; the caller owns the group it names.
    check(unsafe { libc::killpg(pid(leader)?, libc::SIGTERM) })
}

#[cfg(any(test, feature = "test-support"))]
pub(in crate::platform) fn alive(target: u32) -> bool {
    // SAFETY: signal 0 only checks existence and permission.
    pid(target).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
}

pub(in crate::platform) fn is_group_leader(target: u32) -> bool {
    // SAFETY: getpgid only reads process state.
    pid(target).is_ok_and(|pid| unsafe { libc::getpgid(pid) } == pid)
}

pub(in crate::platform) fn exited(child: &Child) -> io::Result<bool> {
    // SAFETY: zeroed siginfo_t is a valid out-parameter; WNOWAIT leaves the child unreaped.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
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
    // SAFETY: waitid filled `info` (si_pid stays 0 when no child changed state).
    Ok(unsafe { info.si_pid() } != 0)
}

/// Blocks until `child` exits or `timeout` elapses, without reaping it. The kernel reports the
/// exit on a descriptor that is waited on directly, so no thread or polling loop is involved.
pub(in crate::platform) fn wait_exited(
    child: &Child,
    timeout: std::time::Duration,
) -> io::Result<bool> {
    wait_exit_event(child.id() as libc::pid_t, timeout)
}

/// Linux: a pidfd becomes readable when its process exits.
#[cfg(target_os = "linux")]
fn wait_exit_event(pid: libc::pid_t, timeout: std::time::Duration) -> io::Result<bool> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: pidfd_open takes a pid and flags only; the returned descriptor is owned below.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        let error = io::Error::last_os_error();
        // ESRCH: the process was already exited and reaped, so there is nothing to wait for.
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(true)
        } else {
            Err(error)
        };
    }
    // SAFETY: `raw` is a fresh descriptor from pidfd_open and is owned only here.
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
    let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    let mut fds = [libc::pollfd {
        fd: std::os::fd::AsRawFd::as_raw_fd(&pidfd),
        events: libc::POLLIN,
        revents: 0,
    }];
    // SAFETY: `fds` is a valid one-element array for the duration of the call.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, millis) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        // An interrupted wait reports "not yet"; callers re-check their state and wait again.
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(error)
        };
    }
    Ok(ready > 0)
}

/// macOS and BSD: `EVFILT_PROC` with `NOTE_EXIT` delivers the exit event for one pid.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn wait_exit_event(pid: libc::pid_t, timeout: std::time::Duration) -> io::Result<bool> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SAFETY: kqueue takes no pointer arguments; the returned descriptor is owned below.
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor from kqueue and is owned only here.
    let queue = unsafe { OwnedFd::from_raw_fd(raw) };
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: zeroed kevent is a valid out-parameter for kevent(2).
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    let limit = libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: timeout.subsec_nanos() as libc::c_long,
    };
    // SAFETY: one change and one event slot, both valid for the call; `limit` outlives it.
    let count = unsafe { libc::kevent(queue.as_raw_fd(), &change, 1, &mut event, 1, &limit) };
    if count < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    if count == 0 {
        return Ok(false);
    }
    if event.flags & libc::EV_ERROR != 0 {
        // Registration failed: ESRCH means the pid is already gone (exited and reaped).
        return if event.data as libc::c_int == libc::ESRCH {
            Ok(true)
        } else {
            Err(io::Error::from_raw_os_error(event.data as i32))
        };
    }
    Ok(true)
}

pub(in crate::platform) fn exit_signal(status: &ExitStatus) -> Option<i32> {
    status.signal()
}

pub(in crate::platform) fn forget_group(_leader: u32) {}
