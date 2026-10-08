use std::io;
use std::process::{Command, ExitStatus};

#[cfg(unix)]
pub use std::os::unix::process::{CommandExt, ExitStatusExt};
#[cfg(unix)]
pub use std::process::Child;

#[cfg(windows)]
mod windows {
    use super::*;
    use process_wrap::std::{ChildWrapper, CommandWrap, JobObject};
    use std::os::windows::io::AsRawHandle;
    use std::process::{ChildStderr, ChildStdin, ChildStdout};
    use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    /// A child starts suspended and joins its Job Object before running. Dropping
    /// it or cancelling it terminates all descendants, including detached shells.
    pub struct Child {
        inner: Box<dyn ChildWrapper>,
        pub stdin: Option<ChildStdin>,
        pub stdout: Option<ChildStdout>,
        pub stderr: Option<ChildStderr>,
    }
    impl Child {
        pub fn id(&self) -> u32 {
            self.inner.id()
        }
        pub fn kill(&mut self) -> io::Result<()> {
            self.inner.start_kill()
        }
        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            self.inner.wait()
        }
        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            self.inner.try_wait()
        }
        pub fn exited(&self) -> io::Result<bool> {
            let handle = self
                .inner
                .process_handle()
                .ok_or_else(|| io::Error::other("missing process handle"))?;
            match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_FAILED => Err(io::Error::last_os_error()),
                _ => Ok(false),
            }
        }
    }
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.inner.start_kill();
            let _ = self.inner.wait();
        }
    }
    pub fn spawn(command: Command) -> io::Result<Child> {
        let mut command = CommandWrap::from(command);
        command.wrap(JobObject);
        let mut inner = command.spawn()?;
        let stdin = inner.stdin().take();
        let stdout = inner.stdout().take();
        let stderr = inner.stderr().take();
        Ok(Child {
            inner,
            stdin,
            stdout,
            stderr,
        })
    }
    pub trait CommandExt {
        fn process_group(&mut self, group: i32) -> &mut Self;
    }
    impl CommandExt for Command {
        fn process_group(&mut self, group: i32) -> &mut Self {
            assert_eq!(group, 0, "only a new process group is supported on Windows");
            self
        }
    }
    pub trait ExitStatusExt {
        fn signal(&self) -> Option<i32>;
    }
    impl ExitStatusExt for ExitStatus {
        fn signal(&self) -> Option<i32> {
            None
        }
    }
}
#[cfg(windows)]
pub use windows::{Child, CommandExt, ExitStatusExt};

/// Spawn a supervised child. Unix callers configure their process group first.
pub fn spawn(mut command: Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        command.spawn()
    }
    #[cfg(windows)]
    {
        windows::spawn(command)
    }
}

pub fn exited(child: &Child) -> io::Result<bool> {
    #[cfg(windows)]
    {
        child.exited()
    }
    #[cfg(unix)]
    {
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
        Ok(unsafe { info.si_pid() } != 0)
    }
}
pub fn terminate(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = child.kill();
    }
}

/// Used only with the current owned process ID, before its handle is reaped.
pub fn terminate_pid(pid: i32) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
        };
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_TIMEOUT},
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid as u32);
        if handle.is_null() {
            return false;
        }
        let alive = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
        CloseHandle(handle);
        alive
    }
}
