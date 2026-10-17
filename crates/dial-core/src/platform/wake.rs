//! Event-driven waits: block until an output pipe is readable, a child exits, or a cancellation
//! is signalled. Every waiter owns a wake channel, and [`signal`] reaches all live waiters, so a
//! cancel set before a wait begins is never lost (the channel stays readable until drained).

use std::io;
use std::process::Child;
use std::time::Duration;

use super::io::Pipe;

/// Wake every live [`Waiter`]. Call immediately after setting any cancel or interrupt flag.
pub fn signal() {
    imp::signal();
}

/// A wake channel for one blocking loop. Create one per loop; drop it when the loop ends.
pub struct Waiter(imp::Waiter);

impl Waiter {
    pub fn new() -> io::Result<Self> {
        imp::Waiter::new().map(Self)
    }

    /// Block until `pipe` is readable, `child` has exited, [`signal`] ran, or `timeout` elapsed
    /// (`None` waits without a deadline). Returns on spurious wakeups too, so callers re-check
    /// their state after every return.
    pub fn wait<P: Pipe + ?Sized>(
        &self,
        pipe: Option<&P>,
        child: Option<&Child>,
        timeout: Option<Duration>,
    ) -> io::Result<()> {
        self.0.wait(pipe, child, timeout)
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::process::Child;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::Duration;

    use parking_lot::Mutex;

    use crate::platform::io::Pipe;

    /// Waiters the registry can serve at once; a wait beyond this fails instead of blocking.
    const SLOTS: usize = 256;

    /// Write end of each live waiter's wake pipe, or -1. Read without locking so [`signal`] can
    /// run from a signal handler.
    static LIVE: [AtomicI32; SLOTS] = [const { AtomicI32::new(-1) }; SLOTS];

    /// Idle wake pipes as `(read, write)`. Their descriptors are never closed, so a late write
    /// from [`signal`] can only reach one of these pipes, never a reused descriptor.
    static POOL: Mutex<Vec<(RawFd, RawFd)>> = Mutex::new(Vec::new());

    /// Wake every live waiter. Lock-free and async-signal-safe.
    pub(super) fn signal() {
        for cell in &LIVE {
            let fd = cell.load(Ordering::Acquire);
            if fd >= 0 {
                // SAFETY: `fd` belongs to a pipe that is never closed; `write` is
                // async-signal-safe. A full pipe (EAGAIN) already holds a pending wakeup.
                unsafe {
                    libc::write(fd, [1u8].as_ptr() as *const c_void, 1);
                }
            }
        }
    }

    /// A nonblocking, close-on-exec pipe as `(read, write)`.
    fn create_pipe() -> io::Result<(RawFd, RawFd)> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` has room for both descriptors.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let configured = fds.iter().try_for_each(|&fd| {
            set_flag(fd, libc::F_SETFD, libc::FD_CLOEXEC)?;
            set_flag(fd, libc::F_SETFL, libc::O_NONBLOCK)
        });
        if let Err(error) = configured {
            // SAFETY: both descriptors were created above and are not shared yet.
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(error);
        }
        Ok((fds[0], fds[1]))
    }

    pub(super) struct Waiter {
        read: RawFd,
        write: RawFd,
        slot: usize,
    }

    impl Waiter {
        pub(super) fn new() -> io::Result<Self> {
            let (read, write) = match POOL.lock().pop() {
                Some(pair) => pair,
                None => create_pipe()?,
            };
            for (slot, cell) in LIVE.iter().enumerate() {
                if cell
                    .compare_exchange(-1, write, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(Self { read, write, slot });
                }
            }
            POOL.lock().push((read, write));
            Err(io::Error::other("too many concurrent command waits"))
        }

        pub(super) fn wait<P: Pipe + ?Sized>(
            &self,
            pipe: Option<&P>,
            child: Option<&Child>,
            timeout: Option<Duration>,
        ) -> io::Result<()> {
            let exit = match child {
                Some(child) => match exit_fd(child.id())? {
                    Some(fd) => Some(fd),
                    // Already exited and reaped: nothing to wait for; the caller re-checks.
                    None => return Ok(()),
                },
                None => None,
            };
            let mut fds = vec![poll_entry(self.read)];
            if let Some(pipe) = pipe {
                fds.push(poll_entry(pipe.as_fd().as_raw_fd()));
            }
            if let Some(exit) = &exit {
                fds.push(poll_entry(exit.as_raw_fd()));
            }
            let millis = match timeout {
                None => -1,
                Some(limit) => limit.as_millis().min(i32::MAX as u128) as libc::c_int,
            };
            // SAFETY: `fds` is a valid array of `fds.len()` entries for the duration of the call.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                // An interrupted wait reports "not yet"; callers re-check and wait again.
                return if error.kind() == io::ErrorKind::Interrupted {
                    Ok(())
                } else {
                    Err(error)
                };
            }
            if fds[0].revents != 0 {
                self.drain();
            }
            Ok(())
        }

        /// Consume pending wake bytes so the next wait blocks again.
        fn drain(&self) {
            let mut buffer = [0u8; 64];
            loop {
                // SAFETY: `buffer` is valid for `buffer.len()` bytes; the read end is nonblocking.
                let count = unsafe {
                    libc::read(self.read, buffer.as_mut_ptr() as *mut c_void, buffer.len())
                };
                if count <= 0 {
                    break;
                }
            }
        }
    }

    impl Drop for Waiter {
        fn drop(&mut self) {
            // Unpublish before pooling so `signal` stops writing to this pipe. The descriptors
            // stay open for reuse.
            LIVE[self.slot].store(-1, Ordering::Release);
            POOL.lock().push((self.read, self.write));
        }
    }

    fn poll_entry(fd: RawFd) -> libc::pollfd {
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }
    }

    fn set_flag(fd: RawFd, command: libc::c_int, flag: libc::c_int) -> io::Result<()> {
        // SAFETY: F_GETFD/F_GETFL take no argument; the set variants take an int flag word.
        let current = unsafe {
            libc::fcntl(
                fd,
                if command == libc::F_SETFD {
                    libc::F_GETFD
                } else {
                    libc::F_GETFL
                },
            )
        };
        if current < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `current | flag` is a valid flag word for `command`.
        if unsafe { libc::fcntl(fd, command, current | flag) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// A descriptor that becomes readable when `pid` exits. `None` means the pid is already gone.
    #[cfg(target_os = "linux")]
    fn exit_fd(pid: u32) -> io::Result<Option<OwnedFd>> {
        // SAFETY: pidfd_open takes a pid and flags only; the returned descriptor is owned below.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: `raw` is a fresh pidfd owned only here.
        Ok(Some(unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) }))
    }

    /// A kqueue whose `EVFILT_PROC`/`NOTE_EXIT` event makes it readable when `pid` exits.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    fn exit_fd(pid: u32) -> io::Result<Option<OwnedFd>> {
        // SAFETY: kqueue takes no pointer arguments; the returned descriptor is owned below.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh kqueue owned only here.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT | libc::EV_RECEIPT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: zeroed kevent is a valid out-parameter for kevent(2).
        let mut receipt: libc::kevent = unsafe { std::mem::zeroed() };
        // SAFETY: one change and one receipt slot, both valid for the call.
        let count = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &change,
                1,
                &mut receipt,
                1,
                std::ptr::null(),
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 1 && receipt.flags & libc::EV_ERROR != 0 && receipt.data != 0 {
            return if receipt.data as libc::c_int == libc::ESRCH {
                Ok(None)
            } else {
                Err(io::Error::from_raw_os_error(receipt.data as i32))
            };
        }
        Ok(Some(queue))
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::time::Duration;

    use parking_lot::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
    };

    use crate::platform::io::Pipe;

    /// Pipe readiness has no waitable handle on Windows anonymous pipes, so a pipe wait re-checks
    /// at this bound while still returning at once for signals and child exit.
    const PIPE_RECHECK: u32 = 5;

    /// Event handles of every live waiter, stored as addresses so the registry is `Send`.
    static WAKERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    pub(super) fn signal() {
        for event in WAKERS.lock().iter() {
            // SAFETY: each registered handle is open: it is removed before `Waiter` closes it.
            unsafe {
                SetEvent(*event as HANDLE);
            }
        }
    }

    pub(super) struct Waiter {
        event: HANDLE,
    }

    impl Waiter {
        pub(super) fn new() -> io::Result<Self> {
            // SAFETY: default security, manual-reset, unsignalled, unnamed event.
            let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            WAKERS.lock().push(event as usize);
            Ok(Self { event })
        }

        pub(super) fn wait<P: Pipe + ?Sized>(
            &self,
            pipe: Option<&P>,
            child: Option<&Child>,
            timeout: Option<Duration>,
        ) -> io::Result<()> {
            let mut handles = vec![self.event];
            if let Some(child) = child {
                handles.push(child.as_raw_handle() as HANDLE);
            }
            let mut millis = match timeout {
                None => INFINITE,
                Some(limit) => limit.as_millis().min(u32::MAX as u128 - 1) as u32,
            };
            if pipe.is_some() {
                millis = millis.min(PIPE_RECHECK);
            }
            // SAFETY: `handles` holds valid handles for the duration of the call.
            let result = unsafe {
                WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, millis)
            };
            if result == WAIT_FAILED {
                // A failed wait returned immediately; surface it so callers do not spin.
                return Err(io::Error::last_os_error());
            }
            if result == WAIT_OBJECT_0 {
                // SAFETY: the event is owned by this waiter.
                unsafe {
                    ResetEvent(self.event);
                }
            }
            Ok(())
        }
    }

    impl Drop for Waiter {
        fn drop(&mut self) {
            let event = self.event as usize;
            WAKERS.lock().retain(|registered| *registered != event);
            // SAFETY: the event was created by `new` and is closed exactly once here.
            unsafe {
                CloseHandle(self.event);
            }
        }
    }
}
