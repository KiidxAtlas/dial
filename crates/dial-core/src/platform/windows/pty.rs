//! Native ConPTY process creation and lifetime management.
use std::{
    ffi::{OsStr, OsString},
    io,
    mem::{size_of, zeroed},
    os::{
        raw::c_void,
        windows::{
            ffi::OsStrExt,
            io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
            process::ExitStatusExt,
        },
    },
    path::Path,
    process::ExitStatus,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole},
        JobObjects::*,
        Pipes::CreatePipe,
        Threading::*,
    },
};

pub struct NativePtyChild {
    process: OwnedHandle,
    pid: u32,
    input: Option<std::fs::File>,
    output: Option<std::fs::File>,
    pseudo: Option<HPCON>,
    _job: OwnedHandle,
}
impl NativePtyChild {
    pub fn id(&self) -> u32 {
        self.pid
    }
    pub fn input(&mut self) -> &mut std::fs::File {
        self.input.as_mut().expect("ConPTY input already closed")
    }
    pub fn output(&mut self) -> &mut std::fs::File {
        self.output.as_mut().expect("ConPTY output already closed")
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        // SAFETY: self.process owns a live process handle throughout this zero-time wait.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, 0) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: self.process remains live and code is writable exit-code storage.
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle() as HANDLE, &mut code) }
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(ExitStatus::from_raw(code)))
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        // SAFETY: self.process owns the process handle throughout the infinite wait.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, u32::MAX) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: self.process remains live and code is writable exit-code storage.
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle() as HANDLE, &mut code) }
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(ExitStatus::from_raw(code))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub fn kill(&mut self) -> io::Result<()> {
        // SAFETY: self._job owns the live job handle; termination takes only a scalar code.
        if unsafe { TerminateJobObject(self._job.as_raw_handle() as HANDLE, 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        if cols == 0 || rows == 0 || cols > i16::MAX as u16 || rows > i16::MAX as u16 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal dimensions must fit a nonzero Windows COORD",
            ));
        }
        let c = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        let pseudo = self
            .pseudo
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "pseudoconsole is closed"))?;
        // SAFETY: pseudo is still owned by self and dimensions were validated to fit COORD.
        let hr = unsafe { ResizePseudoConsole(pseudo, c) };
        if hr < 0 {
            Err(io::Error::from_raw_os_error(hr))
        } else {
            Ok(())
        }
    }
    /// Close the ConPTY output stream so readers can drain buffered output and observe EOF.
    pub fn close_output(&mut self) {
        self.input.take();
        self.output.take();
        if let Some(pseudo) = self.pseudo.take() {
            // SAFETY: take removes the uniquely owned HPCON, ensuring it is closed only once.
            unsafe {
                ClosePseudoConsole(pseudo);
            }
        }
    }
}
impl Drop for NativePtyChild {
    fn drop(&mut self) {
        let _ = self.kill();
        super::process::forget_group(self.pid);
        self.input.take();
        // SAFETY: self.process remains owned until after Drop; waiting does not close it.
        unsafe {
            WaitForSingleObject(self.process.as_raw_handle() as HANDLE, u32::MAX);
        }
        if let Some(pseudo) = self.pseudo.take() {
            let drain = self.output.take().map(|mut output| {
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut buffer = [0u8; 4096];
                    while output.read(&mut buffer).unwrap_or(0) != 0 {}
                })
            });
            // SAFETY: take removed our uniquely owned HPCON; the drain thread keeps output
            // consumable while the synchronous close flushes the pseudoconsole stream.
            unsafe {
                ClosePseudoConsole(pseudo);
            }
            if let Some(drain) = drain {
                let _ = drain.join();
            }
        }
    }
}
fn wide(s: &OsStr) -> io::Result<Vec<u16>> {
    let v: Vec<u16> = s.encode_wide().collect();
    if v.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in Windows string",
        ));
    }
    Ok(v)
}
fn quote(arg: &[u16]) -> Vec<u16> {
    if !arg.is_empty() && !arg.iter().any(|&c| c == 32 || c == 9 || c == 34) {
        return arg.to_vec();
    }
    let mut out = vec![34];
    let mut slash = 0usize;
    for &c in arg {
        if c == 92 {
            slash += 1
        } else if c == 34 {
            out.extend(std::iter::repeat_n(92, slash * 2 + 1));
            out.push(c);
            slash = 0
        } else {
            out.extend(std::iter::repeat_n(92, slash));
            slash = 0;
            out.push(c)
        }
    }
    out.extend(std::iter::repeat_n(92, slash * 2));
    out.push(34);
    out
}
fn env_block(env: &[(OsString, OsString)]) -> io::Result<Vec<u16>> {
    use std::collections::BTreeMap;
    let mut values = BTreeMap::<String, (OsString, OsString)>::new();
    for (k, v) in env {
        if k.is_empty() || k.to_string_lossy().contains('=') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid environment key",
            ));
        }
        wide(k)?;
        wide(v)?;
        values.insert(k.to_string_lossy().to_lowercase(), (k.clone(), v.clone()));
    }
    let mut block = Vec::new();
    for (_, (k, v)) in values {
        block.extend(wide(&k)?);
        block.push(b'=' as u16);
        block.extend(wide(&v)?);
        block.push(0)
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0)
    }
    Ok(block)
}
fn hr(result: i32) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::from_raw_os_error(result))
    } else {
        Ok(())
    }
}
pub fn spawn_conpty(
    program: &OsStr,
    args: &[OsString],
    cwd: Option<&Path>,
    env: &[(OsString, OsString)],
    cols: u16,
    rows: u16,
) -> io::Result<NativePtyChild> {
    if program.is_empty()
        || cols == 0
        || rows == 0
        || cols > i16::MAX as u16
        || rows > i16::MAX as u16
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "program must be nonempty and terminal dimensions must fit a nonzero Windows COORD",
        ));
    }
    let mut in_read: HANDLE = ptr::null_mut();
    let mut in_write: HANDLE = ptr::null_mut();
    let mut out_read: HANDLE = ptr::null_mut();
    let mut out_write: HANDLE = ptr::null_mut();
    // SAFETY: both output pointers name separate writable HANDLE slots; null security
    // attributes request non-inheritable anonymous pipe handles.
    if unsafe { CreatePipe(&mut in_read, &mut in_write, ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreatePipe returned this fresh write handle, adopted exactly once.
    let parent_in = unsafe { OwnedHandle::from_raw_handle(in_write.cast()) };
    // SAFETY: successful CreatePipe returned this distinct fresh read handle, adopted once.
    let child_in = unsafe { OwnedHandle::from_raw_handle(in_read.cast()) };
    // SAFETY: both output pointers name separate writable HANDLE slots; default pipe security.
    if unsafe { CreatePipe(&mut out_read, &mut out_write, ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreatePipe returned this fresh read handle, adopted exactly once.
    let parent_out = unsafe { OwnedHandle::from_raw_handle(out_read.cast()) };
    // SAFETY: successful CreatePipe returned this distinct fresh write handle, adopted once.
    let child_out = unsafe { OwnedHandle::from_raw_handle(out_write.cast()) };
    let mut pseudo: HPCON = 0;
    // SAFETY: both pipe handles remain owned through creation, pseudo is writable, and
    // validated dimensions fit COORD. ConPTY retains its own references to these endpoints.
    hr(unsafe {
        CreatePseudoConsole(
            COORD {
                X: cols as i16,
                Y: rows as i16,
            },
            child_in.as_raw_handle() as HANDLE,
            child_out.as_raw_handle() as HANDLE,
            0,
            &mut pseudo,
        )
    })?;
    let mut attr_bytes = 0usize;
    // SAFETY: null with zero initial size is the documented sizing query; attr_bytes is writable.
    unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut attr_bytes) };
    let layout = match std::alloc::Layout::from_size_align(attr_bytes, 16) {
        Ok(layout) => layout,
        Err(_) => {
            // SAFETY: creation succeeded and no owner has yet adopted or closed pseudo.
            unsafe { ClosePseudoConsole(pseudo) };
            return Err(io::ErrorKind::OutOfMemory.into());
        }
    };
    // SAFETY: layout is a validated allocation layout with the requested size/alignment;
    // null is checked before the allocation is passed to Windows.
    let attrs = unsafe { std::alloc::alloc_zeroed(layout) } as LPPROC_THREAD_ATTRIBUTE_LIST;
    if attrs.is_null() {
        // SAFETY: pseudo is our still-unadopted successful CreatePseudoConsole result.
        unsafe { ClosePseudoConsole(pseudo) };
        return Err(io::ErrorKind::OutOfMemory.into());
    }
    let mut attr_initialized = false;
    let result = (|| {
        // SAFETY: attrs points to attr_bytes writable bytes with 16-byte alignment, as
        // reported by the sizing query; the list is uninitialized and initialized once here.
        if unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut attr_bytes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        attr_initialized = true;
        // SAFETY: attrs is initialized; PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE uniquely takes
        // the live HPCON value itself as lpValue (not a pointer to the HPCON variable).
        if unsafe {
            UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                pseudo as *const c_void,
                size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: STARTUPINFOEXW has only scalar/pointer fields whose zero values are valid;
        // cb and lpAttributeList are filled before passing it to CreateProcessW.
        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attrs;
        let prog = wide(program)?;
        let mut cmd = quote(&prog);
        for a in args {
            cmd.push(32);
            cmd.extend(quote(&wide(a)?));
        }
        cmd.push(0);
        let mut app = prog.clone();
        app.push(0);
        let dir = match cwd {
            Some(p) => {
                let mut d = wide(p.as_os_str())?;
                d.push(0);
                Some(d)
            }
            None => None,
        };
        let mut block = env_block(env)?;
        // SAFETY: null security attributes and name request an unnamed default-security job.
        let rawjob = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if rawjob.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a fresh non-null handle adopted exactly once.
        let job = unsafe { OwnedHandle::from_raw_handle(rawjob.cast()) };
        // SAFETY: all-zero fields form valid job limits; the desired flag is set before use.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: rawjob remains owned, limits is initialized and the size matches the
        // JobObjectExtendedLimitInformation class used for this synchronous call.
        if unsafe {
            SetInformationJobObject(
                rawjob,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: PROCESS_INFORMATION consists of scalar/pointer output fields with valid zeros.
        let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
        // SAFETY: app/cmd/optional dir are live NUL-terminated UTF-16 buffers, block is the
        // double-NUL environment block, startup retains the live attribute list and HPCON,
        // and pi is writable. cmd is mutable as required; no inheritable handles are requested.
        if unsafe {
            CreateProcessW(
                app.as_ptr(),
                cmd.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                block.as_mut_ptr() as *const c_void,
                dir.as_ref().map_or(ptr::null(), |d| d.as_ptr()),
                &startup.StartupInfo,
                &mut pi,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        drop(child_in);
        drop(child_out);
        // SAFETY: successful CreateProcessW returned this fresh process handle, adopted once.
        let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess.cast()) };
        // SAFETY: successful CreateProcessW returned this distinct fresh thread handle, adopted once.
        let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread.cast()) };
        // SAFETY: job and process own both live handles; the process is still suspended.
        if unsafe { AssignProcessToJobObject(rawjob, pi.hProcess) } == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: process still owns pi.hProcess; failure cleanup kills only this new child.
            unsafe {
                TerminateProcess(pi.hProcess, 1);
            }
            return Err(error);
        }
        super::process::register_group(pi.dwProcessId, job.as_handle())?;
        // SAFETY: thread owns the initial suspended thread handle returned by CreateProcessW.
        if unsafe { ResumeThread(pi.hThread) } == u32::MAX {
            let error = io::Error::last_os_error();
            // SAFETY: job still owns rawjob; the new child was assigned to it above.
            unsafe {
                TerminateJobObject(rawjob, 1);
            }
            super::process::forget_group(pi.dwProcessId);
            return Err(error);
        }
        let pid = pi.dwProcessId;
        drop(thread);
        let input = std::fs::File::from(parent_in);
        let output = std::fs::File::from(parent_out);
        Ok((process, pid, input, output, job))
    })();
    // SAFETY: attrs is still our allocation with its original layout. Delete is called only
    // after successful initialization, and no process-creation call can still refer to the list.
    unsafe {
        if attr_initialized {
            DeleteProcThreadAttributeList(attrs);
        }
        std::alloc::dealloc(attrs.cast::<u8>(), layout)
    };
    match result {
        Ok((process, pid, input, output, job)) => Ok(NativePtyChild {
            process,
            pid,
            input: Some(input),
            output: Some(output),
            pseudo: Some(pseudo),
            _job: job,
        }),
        Err(e) => {
            // SAFETY: the error path has not transferred pseudo into NativePtyChild, so this
            // closes the one remaining ownership acquired by CreatePseudoConsole.
            unsafe { ClosePseudoConsole(pseudo) };
            Err(e)
        }
    }
}
#[cfg(test)]
mod tests;
