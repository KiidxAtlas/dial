//! Unix implementations: exactly the syscalls Dial issued before the platform layer existed.

pub(super) mod fs {
    use crate::platform::FileIdentity;
    use std::ffi::CString;
    use std::fs::{DirBuilder, File, Metadata, OpenOptions};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Component, Path};

    pub(in crate::platform) fn open_nofollow(
        options: &OpenOptions,
        path: &Path,
        nonblocking: bool,
    ) -> io::Result<File> {
        let mut options = options.clone();
        let extra = if nonblocking { libc::O_NONBLOCK } else { 0 };
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | extra)
            .open(path)
    }

    pub(in crate::platform) fn open_dir_nofollow(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }

    pub(in crate::platform) fn open_beneath(
        root: &Path,
        relative: &Path,
        directory: bool,
    ) -> io::Result<File> {
        let mut file = open_dir_nofollow(root)?;
        let parts: Vec<_> = relative.components().collect();
        for (index, part) in parts.iter().enumerate() {
            let Component::Normal(name) = part else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path beneath a root must contain plain names only",
                ));
            };
            let name = CString::new(name.as_bytes()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte")
            })?;
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if index + 1 < parts.len() || directory {
                    libc::O_DIRECTORY
                } else {
                    0
                };
            // SAFETY: name is NUL-terminated and `file` is an open directory descriptor.
            let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: openat returned a new descriptor owned exactly once.
            file = unsafe { File::from_raw_fd(fd) };
        }
        Ok(file)
    }

    pub(in crate::platform) fn open_beneath_handle(
        root: &File,
        relative: &Path,
    ) -> io::Result<File> {
        let mut parts = relative.components();
        let (Some(Component::Normal(name)), None) = (parts.next(), parts.next()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "handle-relative open requires one plain name",
            ));
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        // SAFETY: name is NUL-terminated and `root` is an open directory descriptor.
        let fd = unsafe {
            libc::openat(
                root.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new descriptor owned exactly once.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    pub(in crate::platform) fn owned_by_current_user(file: &File) -> io::Result<bool> {
        // SAFETY: geteuid has no preconditions.
        Ok(file.metadata()?.uid() == unsafe { libc::geteuid() })
    }

    pub(in crate::platform) fn path_owned_by_current_user(path: &Path) -> io::Result<bool> {
        // SAFETY: geteuid has no preconditions.
        Ok(std::fs::symlink_metadata(path)?.uid() == unsafe { libc::geteuid() })
    }

    pub(in crate::platform) fn path_owned_by_system(path: &Path) -> io::Result<bool> {
        Ok(std::fs::symlink_metadata(path)?.uid() == 0)
    }

    pub(in crate::platform) fn link_count(file: &File) -> io::Result<u64> {
        Ok(file.metadata()?.nlink())
    }

    pub(in crate::platform) fn identity(file: &File) -> io::Result<FileIdentity> {
        let meta = file.metadata()?;
        Ok(FileIdentity {
            device: meta.dev(),
            inode: meta.ino(),
        })
    }

    pub(in crate::platform) fn path_identity(path: &Path) -> io::Result<FileIdentity> {
        let meta = std::fs::symlink_metadata(path)?;
        Ok(FileIdentity {
            device: meta.dev(),
            inode: meta.ino(),
        })
    }

    pub(in crate::platform) fn is_owner_only(file: &File) -> io::Result<bool> {
        Ok(file.metadata()?.mode() & 0o077 == 0)
    }

    pub(in crate::platform) fn set_owner_only_file(file: &File) -> io::Result<()> {
        let mode = if file.metadata()?.is_dir() {
            0o700
        } else {
            0o600
        };
        if unsafe { libc::fchmod(file.as_raw_fd(), mode) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(in crate::platform) fn set_owner_only(path: &Path) -> io::Result<()> {
        let mode = if std::fs::metadata(path)?.is_dir() {
            0o700
        } else {
            0o600
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    pub(in crate::platform) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    pub(in crate::platform) fn mode(meta: &Metadata) -> u32 {
        meta.permissions().mode()
    }

    pub(in crate::platform) fn is_executable(path: &Path) -> bool {
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }

    pub(in crate::platform) fn open_owner_only(
        options: &OpenOptions,
        path: &Path,
    ) -> io::Result<File> {
        let mut options = options.clone();
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private file must be a regular file",
            ));
        }
        Ok(file)
    }

    pub(in crate::platform) fn create_dir_owner_only(
        builder: &DirBuilder,
        path: &Path,
    ) -> io::Result<()> {
        let mut builder = clone_builder(builder);
        builder.mode(0o700).create(path)
    }

    /// `DirBuilder` is not `Clone`; its only portable state is `recursive`, which `create`
    /// observably needs. Recover it from the builder's `Debug` output.
    fn clone_builder(builder: &DirBuilder) -> DirBuilder {
        let mut copy = DirBuilder::new();
        copy.recursive(format!("{builder:?}").contains("recursive: true"));
        copy
    }
}

pub(super) mod process {
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
    pub fn alive(target: u32) -> bool {
        // SAFETY: signal 0 only checks existence and permission.
        pid(target).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }

    pub(in crate::platform) fn is_group_leader(target: u32) -> bool {
        // SAFETY: getpgid only reads process state.
        pid(target).is_ok_and(|pid| unsafe { libc::getpgid(pid) } == pid)
    }

    pub fn exited(child: &Child) -> io::Result<bool> {
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
}

pub(super) mod io {
    use crate::platform::io::Pipe;
    use std::io::{self, Read};
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    pub(in crate::platform) fn set_nonblocking(pipe: &impl Pipe) -> io::Result<()> {
        let fd = pipe.as_fd().as_raw_fd();
        // SAFETY: fcntl only reads/updates status flags of a live descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub(in crate::platform) fn set_blocking(pipe: &impl Pipe) -> io::Result<()> {
        let fd = pipe.as_fd().as_raw_fd();
        // SAFETY: fcntl only reads/updates status flags of a live descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(in crate::platform) fn read_nonblocking<R: Read + Pipe>(
        reader: &mut R,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        reader.read(buffer)
    }

    fn wait(pipe: &impl Pipe, events: libc::c_short, timeout: Duration) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: pipe.as_fd().as_raw_fd(),
            events,
            revents: 0,
        };
        let millis = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: one initialized pollfd and a finite timeout.
        let ready = unsafe { libc::poll(&mut poll, 1, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            };
        }
        Ok(ready > 0)
    }

    pub(in crate::platform) fn wait_readable(
        pipe: &impl Pipe,
        timeout: Duration,
    ) -> io::Result<bool> {
        wait(pipe, libc::POLLIN, timeout)
    }

    pub(in crate::platform) fn wait_writable(
        pipe: &impl Pipe,
        timeout: Duration,
    ) -> io::Result<bool> {
        wait(pipe, libc::POLLOUT, timeout)
    }
}

pub(super) mod sys {
    use std::io;
    use std::sync::OnceLock;

    pub(in crate::platform) fn hostname() -> io::Result<String> {
        let mut name = [0u8; 256];
        // SAFETY: the buffer is writable for its full length.
        if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let end = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        Ok(String::from_utf8_lossy(&name[..end]).into_owned())
    }

    static HANDLER: OnceLock<fn()> = OnceLock::new();

    extern "C" fn dispatch(_: libc::c_int) {
        if let Some(handler) = HANDLER.get() {
            handler();
        }
    }

    pub(in crate::platform) fn on_interrupt(handler: fn()) {
        if HANDLER.set(handler).is_err() {
            return;
        }
        // SAFETY: a zeroed sigaction with an empty mask and a plain handler is valid.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = dispatch as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0;
            libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        }
    }

    pub(in crate::platform) fn exit_now(code: i32) -> ! {
        // SAFETY: _exit never returns and has no preconditions.
        unsafe { libc::_exit(code) }
    }
}
