//! Process identity that survives PID reuse: a PID plus the kernel's record of its start time.
//!
//! Every cross-process decision in `bg` (re-attaching a service after a Dial restart, reaping
//! an orphan, signalling a supervisor) compares a stored identity with the live one first, so a
//! recycled PID is never signalled. When the start time cannot be read the identity is treated
//! as unverifiable and nothing is signalled.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcId {
    pub pid: i32,
    /// Start time in kernel-specific units; only ever compared for equality.
    pub start: u64,
}

/// Kill signal for `ProcId::signal_group`. Windows has no signals; its group kill is always forceful.
#[cfg(unix)]
pub const SIGKILL: i32 = libc::SIGKILL;
#[cfg(windows)]
pub const SIGKILL: i32 = 9;

impl ProcId {
    pub fn of(pid: i32) -> Option<ProcId> {
        if pid <= 1 {
            return None;
        }
        let (start, zombie) = probe(pid)?;
        (!zombie && start != 0).then_some(ProcId { pid, start })
    }

    pub fn current() -> Option<ProcId> {
        Self::of(std::process::id() as i32)
    }

    /// The same process still exists, zombie or not. A zombie group leader can still have live
    /// group members, so group signalling uses this rather than `alive`.
    pub fn same_process(&self) -> bool {
        self.start != 0 && probe(self.pid).is_some_and(|(start, _)| start == self.start)
    }

    /// The process is running and not a zombie (exited, zombie or reused PIDs are not).
    pub fn alive(&self) -> bool {
        Self::of(self.pid).is_some_and(|live| live == *self)
    }

    /// Signal this process's group (`pgid == pid`) only while the identity still matches.
    #[cfg(unix)]
    pub fn signal_group(&self, signal: i32) -> bool {
        self.same_process() && unsafe { libc::killpg(self.pid, signal) } == 0
    }

    /// Kill this process (and its job, when this runtime owns one) only while the identity still
    /// matches. The identity is read and the kill issued on one handle, which pins the process
    /// object, so a recycled PID can never be the target. Windows has no signals.
    #[cfg(windows)]
    pub fn signal_group(&self, _signal: i32) -> bool {
        use std::os::windows::io::AsHandle;
        self.start != 0
            && win::open(self.pid, win::KILL_ACCESS).is_some_and(|handle| {
                win::times(&handle) == Some((self.start, false))
                    && crate::platform::process::kill_checked(handle.as_handle()).is_ok()
            })
    }

    /// Signal just this process only while the identity still matches.
    #[cfg(unix)]
    pub fn signal(&self, signal: i32) -> bool {
        self.alive() && unsafe { libc::kill(self.pid, signal) } == 0
    }
}

/// (start time, zombie) for a live PID.
#[cfg(target_os = "macos")]
fn probe(pid: i32) -> Option<(u64, bool)> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    // SZOMB from <sys/proc.h>.
    const SZOMB: u32 = 5;
    Some((
        info.pbi_start_tvsec
            .wrapping_mul(1_000_000)
            .wrapping_add(info.pbi_start_tvusec),
        info.pbi_status == SZOMB,
    ))
}

#[cfg(target_os = "linux")]
fn probe(pid: i32) -> Option<(u64, bool)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses; fields resume after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?;
    // Field 22 (starttime) is the 20th after the state (field 3).
    let start = fields.nth(18)?.parse().ok()?;
    Some((start, state == "Z" || state == "X"))
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn probe(pid: i32) -> Option<(u64, bool)> {
    let output = super::process::spawn(
        std::process::Command::new("ps")
            .env("LC_ALL", "C")
            .args(["-o", "lstart=,stat=", "-p", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
    )
    .ok()?
    .wait_with_output()
    .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.trim();
    if !output.status.success() || line.is_empty() {
        return None;
    }
    let (start, stat) = line.rsplit_once(char::is_whitespace)?;
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    start.trim().hash(&mut hasher);
    Some((hasher.finish(), stat.starts_with('Z')))
}

#[cfg(windows)]
fn probe(pid: i32) -> Option<(u64, bool)> {
    win::times(&win::open(pid, win::QUERY_ACCESS)?)
}

#[cfg(windows)]
mod win {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{FILETIME, HANDLE},
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_TERMINATE,
        },
    };
    // STILL_ACTIVE from <minwinbase.h>: the code a running process reports.
    const STILL_ACTIVE: u32 = 259;
    pub(super) const QUERY_ACCESS: u32 = PROCESS_QUERY_LIMITED_INFORMATION;
    pub(super) const KILL_ACCESS: u32 = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE;

    /// Opens `pid` with `access`. The handle pins the process object for its whole lifetime.
    pub(super) fn open(pid: i32, access: u32) -> Option<OwnedHandle> {
        let raw = unsafe { OpenProcess(access, 0, pid as u32) };
        (!raw.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(raw.cast()) })
    }

    /// (start time, exited) read from an already-open handle, so identity and kill share one object.
    pub(super) fn times(handle: &OwnedHandle) -> Option<(u64, bool)> {
        let raw = handle.as_raw_handle() as HANDLE;
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        if unsafe { GetProcessTimes(raw, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
            return None;
        }
        let mut code = 0u32;
        if unsafe { GetExitCodeProcess(raw, &mut code) } == 0 {
            return None;
        }
        let start = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        Some((start, code != STILL_ACTIVE))
    }
}

#[cfg(test)]
mod tests;
