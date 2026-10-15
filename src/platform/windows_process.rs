//! Suspended creation prevents descendants escaping the job before containment.
use std::{
    collections::HashMap,
    io,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    process::{Child, Command, ExitStatus},
    sync::{LazyLock, Mutex},
};
use windows_sys::Win32::{
    Foundation::{HANDLE, STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent},
        JobObjects::*,
        Threading::*,
    },
};
static JOBS: LazyLock<Mutex<HashMap<u32, OwnedHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtResumeProcess(process: HANDLE) -> i32;
}
fn jobs() -> std::sync::MutexGuard<'static, HashMap<u32, OwnedHandle>> {
    JOBS.lock().unwrap_or_else(|poison| poison.into_inner())
}
pub(in crate::platform) fn spawn_group(command: &mut Command) -> io::Result<Child> {
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut child = command
        .creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP)
        .spawn()?;
    let handle = child.as_raw_handle() as HANDLE;
    if unsafe { AssignProcessToJobObject(raw, handle) } == 0 {
        let error = io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let status = unsafe { NtResumeProcess(handle) };
    if status < 0 {
        let error = io::Error::from_raw_os_error(unsafe {
            windows_sys::Win32::Foundation::RtlNtStatusToDosError(status)
        } as i32);
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    jobs().insert(child.id(), job);
    Ok(child)
}
pub(in crate::platform) fn register_group(
    pid: u32,
    job: std::os::windows::io::BorrowedHandle<'_>,
) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    let mut duplicate = std::ptr::null_mut();
    let current = unsafe { GetCurrentProcess() };
    if unsafe {
        DuplicateHandle(
            current,
            job.as_raw_handle() as HANDLE,
            current,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    jobs().insert(pid, unsafe {
        OwnedHandle::from_raw_handle(duplicate.cast())
    });
    Ok(())
}
pub(in crate::platform) fn kill_group(pid: u32) -> io::Result<()> {
    let jobs = jobs();
    if let Some(job) = jobs.get(&pid) {
        if unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    // Unknown PIDs are not our groups: never kill an unrelated PID-reused process tree.
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "process group is not owned by this Dial runtime",
    ))
}
pub(in crate::platform) fn terminate_group(pid: u32) -> io::Result<()> {
    if !jobs().contains_key(&pid) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "unknown process group",
        ));
    }
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } != 0 {
        return Ok(());
    }
    kill_group(pid)
}
/// Kill the validated process, or its owned job after checking membership on that exact handle.
pub(in crate::platform) fn kill_checked(
    process: std::os::windows::io::BorrowedHandle<'_>,
) -> io::Result<()> {
    let raw = process.as_raw_handle() as HANDLE;
    let pid = unsafe { GetProcessId(raw) };
    if pid == 0 {
        return Err(io::Error::last_os_error());
    }
    let groups = jobs();
    if let Some(job) = groups.get(&pid) {
        let mut member = 0;
        if unsafe { IsProcessInJob(raw, job.as_raw_handle() as HANDLE, &mut member) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if member == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process group identity changed",
            ));
        }
        if unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
    } else if unsafe { TerminateProcess(raw, 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(test)]
pub(in crate::platform) fn alive(pid: u32) -> bool {
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | 0x0010_0000, 0, pid) };
    if raw.is_null() {
        return false;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    unsafe { WaitForSingleObject(handle.as_raw_handle() as HANDLE, 0) == WAIT_TIMEOUT }
}
pub(in crate::platform) fn is_group_leader(pid: u32) -> bool {
    let jobs = jobs();
    let Some(job) = jobs.get(&pid) else {
        return false;
    };
    // An exited leader is not running, and a reused PID is not a member of this job.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return false;
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.as_raw_handle() as HANDLE, &mut code) } == 0
        || code != STILL_ACTIVE as u32
    {
        return false;
    }
    let mut in_job = 0;
    let checked = unsafe {
        IsProcessInJob(
            process.as_raw_handle() as HANDLE,
            job.as_raw_handle() as HANDLE,
            &mut in_job,
        )
    };
    checked != 0 && in_job != 0
}
pub(in crate::platform) fn exited(child: &Child) -> io::Result<bool> {
    match unsafe { WaitForSingleObject(child.as_raw_handle() as HANDLE, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}
/// Blocks until `child` exits or `timeout` elapses, without reaping it (the handle is signalled).
pub(in crate::platform) fn wait_exited(
    child: &Child,
    timeout: std::time::Duration,
) -> io::Result<bool> {
    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    match unsafe { WaitForSingleObject(child.as_raw_handle() as HANDLE, millis) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}
pub(in crate::platform) fn exit_signal(_: &ExitStatus) -> Option<i32> {
    None
}
pub(in crate::platform) fn forget_group(pid: u32) {
    jobs().remove(&pid);
}
