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
    Foundation::{ERROR_INVALID_PARAMETER, HANDLE, STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT},
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
pub(in crate::platform) fn quiet_error_dialogs() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        GetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
    };
    // SAFETY: both calls take and return plain flag words for this process only.
    unsafe {
        SetErrorMode(GetErrorMode() | SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    }
}
pub(in crate::platform) fn spawn_group(command: &mut Command) -> io::Result<Child> {
    // SAFETY: null security attributes and name request an unnamed job with default security.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateJobObjectW returned a fresh non-null handle owned only by this scope.
    let job = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: raw stays owned by job; limits is a live initialized structure whose exact
    // size matches the JobObjectExtendedLimitInformation class supplied here.
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
    // SAFETY: job and child own both live handles; the child is still suspended, so
    // containment is established before it can start descendants.
    if unsafe { AssignProcessToJobObject(raw, handle) } == 0 {
        let error = io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    // SAFETY: handle is our suspended child's live process handle; NtResumeProcess
    // changes its suspend count without borrowing any Rust memory.
    let status = unsafe { NtResumeProcess(handle) };
    if status < 0 {
        // SAFETY: RtlNtStatusToDosError accepts the scalar NTSTATUS returned above.
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
    // SAFETY: GetCurrentProcess has no arguments and returns a non-owning pseudo handle.
    let current = unsafe { GetCurrentProcess() };
    // SAFETY: job is borrowed for this call; duplicate is writable and both source and
    // target process pseudo handles refer to this process, so no remote ownership is involved.
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
    // SAFETY: DuplicateHandle transferred one fresh handle, now uniquely owned by the map.
    jobs().insert(pid, unsafe {
        OwnedHandle::from_raw_handle(duplicate.cast())
    });
    Ok(())
}
pub(in crate::platform) fn kill_group(pid: u32) -> io::Result<()> {
    let jobs = jobs();
    if let Some(job) = jobs.get(&pid) {
        // SAFETY: the jobs guard keeps this owned job handle live during termination.
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
    // SAFETY: this API takes only a scalar event and group ID, with no memory arguments;
    // the map check above limits the best-effort event to a runtime-owned group ID.
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
    // SAFETY: process lends a live process handle for this query.
    let pid = unsafe { GetProcessId(raw) };
    if pid == 0 {
        return Err(io::Error::last_os_error());
    }
    let groups = jobs();
    if let Some(job) = groups.get(&pid) {
        let mut member = 0;
        // SAFETY: process and the locked map keep both handles live; member is writable.
        if unsafe { IsProcessInJob(raw, job.as_raw_handle() as HANDLE, &mut member) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if member == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process group identity changed",
            ));
        }
        // SAFETY: the map guard keeps this job handle live and membership was checked
        // on the same pinned process object immediately above.
        if unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
    } else {
        // SAFETY: raw remains borrowed from process; TerminateProcess takes no memory pointers.
        if unsafe { TerminateProcess(raw, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
#[cfg(any(test, feature = "test-support"))]
pub(in crate::platform) fn alive(pid: u32) -> bool {
    // SAFETY: OpenProcess takes scalar access/ID arguments; null is handled below.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | 0x0010_0000, 0, pid) };
    if raw.is_null() {
        return false;
    }
    // SAFETY: OpenProcess succeeded and its fresh handle is uniquely owned here.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    // SAFETY: handle stays owned for this zero-time, non-owning wait.
    unsafe { WaitForSingleObject(handle.as_raw_handle() as HANDLE, 0) == WAIT_TIMEOUT }
}
pub(in crate::platform) fn is_group_leader(pid: u32) -> bool {
    let jobs = jobs();
    let Some(job) = jobs.get(&pid) else {
        return false;
    };
    // An exited leader is not running, and a reused PID is not a member of this job.
    // SAFETY: OpenProcess takes scalar access/ID arguments and null is handled before use.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return false;
    }
    // SAFETY: raw is a fresh non-null OpenProcess handle, adopted exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let mut code = 0u32;
    // SAFETY: process keeps the handle live and code is writable exit-code storage.
    if unsafe { GetExitCodeProcess(process.as_raw_handle() as HANDLE, &mut code) } == 0
        || code != STILL_ACTIVE as u32
    {
        return false;
    }
    let mut in_job = 0;
    // SAFETY: process and the jobs lock retain both handles; in_job is writable output storage.
    let checked = unsafe {
        IsProcessInJob(
            process.as_raw_handle() as HANDLE,
            job.as_raw_handle() as HANDLE,
            &mut in_job,
        )
    };
    checked != 0 && in_job != 0
}
/// Whether `pid` runs inside the Job Object Dial created for the group led by `leader`. Jobs
/// are inherited, so this covers every descendant; an unknown leader is never a match.
pub(in crate::platform) fn in_group(leader: u32, pid: u32) -> bool {
    let jobs = jobs();
    let Some(job) = jobs.get(&leader) else {
        return false;
    };
    // SAFETY: OpenProcess takes scalar access/ID arguments and null is handled before use.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return false;
    }
    // SAFETY: raw is a fresh non-null OpenProcess handle, adopted exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let mut member = 0;
    // SAFETY: process and the jobs lock retain both handles; member is writable output storage.
    let checked = unsafe {
        IsProcessInJob(
            process.as_raw_handle() as HANDLE,
            job.as_raw_handle() as HANDLE,
            &mut member,
        )
    };
    checked != 0 && member != 0
}
/// Opens `pid` with `access`. A PID that names no process is `NotFound`.
fn open_pid(pid: u32, access: u32) -> io::Result<OwnedHandle> {
    // SAFETY: OpenProcess takes only scalar access, inheritance and ID arguments.
    let raw = unsafe { OpenProcess(access, 0, pid) };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        return Err(
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                io::Error::new(io::ErrorKind::NotFound, "no such process")
            } else {
                error
            },
        );
    }
    // SAFETY: raw is a fresh non-null process handle, adopted exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw.cast()) })
}
pub(in crate::platform) fn process_exists(pid: u32) -> io::Result<bool> {
    let process = match open_pid(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
        Ok(process) => process,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    // SAFETY: process keeps the handle live for this zero-time wait; a signalled process exited.
    Ok(unsafe { WaitForSingleObject(process.as_raw_handle() as HANDLE, 0) } == WAIT_TIMEOUT)
}
pub(in crate::platform) fn terminate_pid(pid: u32, exit_code: u32) -> io::Result<()> {
    let process = open_pid(pid, PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION)?;
    let raw = process.as_raw_handle() as HANDLE;
    // SAFETY: process keeps the handle live for this zero-time wait.
    if unsafe { WaitForSingleObject(raw, 0) } == WAIT_OBJECT_0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "process has already exited",
        ));
    }
    // SAFETY: process pins the exact process this call names; TerminateProcess takes scalars only.
    if unsafe { TerminateProcess(raw, exit_code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Terminates `pid` and every descendant with `exit_code`, so a `.cmd` shim's child (`cmd.exe` ->
/// `node.exe`) does not outlive it. Descendants come from one Toolhelp snapshot; a process counts
/// as a child only when its parent PID names a process that started no later than it, so a reused
/// PID is never taken for one. Each process is opened and identity-checked before the first
/// termination, and the named root must still be running (`NotFound` otherwise).
pub(in crate::platform) fn terminate_tree(pid: u32, exit_code: u32) -> io::Result<()> {
    let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION;
    let root = open_pid(pid, access)?;
    let root_created = match crate::platform::procinfo::win::times(&root) {
        Some((created, false)) => created,
        Some((_, true)) => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "process has already exited",
            ));
        }
        None => return Err(io::Error::last_os_error()),
    };
    let entries = crate::platform::processes::snapshot()?;
    // (pid, creation time, handle) for the root and each descendant found so far, breadth first.
    let mut members: Vec<(u32, u64, OwnedHandle)> = vec![(pid, root_created, root)];
    let mut next = 0;
    while next < members.len() {
        let (parent, parent_created) = (members[next].0, members[next].1);
        next += 1;
        for entry in entries
            .iter()
            .filter(|entry| entry.parent == parent && entry.pid != parent)
        {
            if members.iter().any(|(known, _, _)| *known == entry.pid) {
                continue;
            }
            let Ok(handle) = open_pid(entry.pid, access) else {
                continue;
            };
            if let Some((created, false)) = crate::platform::procinfo::win::times(&handle)
                && created >= parent_created
            {
                members.push((entry.pid, created, handle));
            }
        }
    }
    // Deepest first, so a parent cannot spawn a replacement child while the sweep runs.
    let mut status = Ok(());
    for (index, (_, _, handle)) in members.iter().enumerate().rev() {
        // SAFETY: handle pins the exact process whose identity was checked when it was opened.
        let terminated = unsafe { TerminateProcess(handle.as_raw_handle() as HANDLE, exit_code) };
        if index == 0 && terminated == 0 {
            status = Err(io::Error::last_os_error());
        }
    }
    status
}

pub(in crate::platform) fn exited(child: &Child) -> io::Result<bool> {
    // SAFETY: child owns the live process handle for the entire zero-time query.
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
    // SAFETY: child owns the process handle throughout the bounded, non-owning wait.
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
