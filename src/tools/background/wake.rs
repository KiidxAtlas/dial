//! Kernel-driven wake-ups for background waits.
//!
//! Every state change that a blocked background operation waits on (ring output, reader EOF, child
//! exit, cancellation, a freed input slot) bumps one process-wide generation counter and notifies a
//! condvar. Waiters read the generation *before* checking their condition, then sleep only while the
//! generation is unchanged, so a change that lands between the check and the sleep is never missed.
//! Unix fd waits additionally register a self-pipe so cancellation can interrupt `poll(2)`.

use parking_lot::{Condvar, Mutex};
use std::time::Instant;

static GENERATION: Mutex<u64> = Mutex::new(0);
static CHANGED: Condvar = Condvar::new();

/// The current generation. Read it before checking a condition, then pass it to [`wait_past`].
pub(super) fn generation() -> u64 {
    *GENERATION.lock()
}

/// Records a state change and wakes every waiter.
pub(super) fn notify() {
    let mut generation = GENERATION.lock();
    *generation = generation.wrapping_add(1);
    CHANGED.notify_all();
}

/// Blocks while the generation still equals `seen`, until it moves or `deadline` passes.
pub(super) fn wait_past(seen: u64, deadline: Option<Instant>) {
    let mut generation = GENERATION.lock();
    while *generation == seen {
        match deadline {
            None => CHANGED.wait(&mut generation),
            Some(deadline) => {
                if CHANGED.wait_until(&mut generation, deadline).timed_out() {
                    return;
                }
            }
        }
    }
}

/// Wakes every background wait so it re-checks its cancellation flags.
///
/// Callers that set a per-operation cancel flag or the background interrupt flag must call this
/// right after setting it.
pub fn notify_cancel() {
    notify();
    #[cfg(unix)]
    unix::wake_pollers();
    crate::platform::wake::signal();
}

#[cfg(unix)]
mod unix {
    use super::notify;
    use parking_lot::Mutex;
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    /// Write ends of every self-pipe currently blocked in [`Waker::wait`], keyed by waker id.
    static POLLERS: Mutex<Vec<(u64, RawFd)>> = Mutex::new(Vec::new());
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    pub(super) fn wake_pollers() {
        let pollers = POLLERS.lock();
        for &(_, fd) in pollers.iter() {
            // SAFETY: `fd` is the write end of a live self-pipe; it is unregistered before the
            // pipe is closed (see `Drop for Waker`), and the pipe is non-blocking, so this never
            // blocks. Failure (full pipe) means a wake is already pending.
            unsafe {
                libc::write(fd, b"!".as_ptr().cast(), 1);
            }
        }
    }

    /// A self-pipe that lets one blocked `poll(2)` call be interrupted by [`super::notify_cancel`].
    pub(in crate::tools::background) struct Waker {
        id: u64,
        read: io::PipeReader,
        _write: io::PipeWriter,
    }

    impl Waker {
        pub(in crate::tools::background) fn new() -> io::Result<Self> {
            let (read, write) = io::pipe()?;
            set_nonblocking(read.as_raw_fd())?;
            set_nonblocking(write.as_raw_fd())?;
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            POLLERS.lock().push((id, write.as_raw_fd()));
            Ok(Waker {
                id,
                read,
                _write: write,
            })
        }

        /// Blocks until `fd` has one of `events` ready, a wake arrives, `end` passes, or a signal
        /// interrupts the call. Callers re-check their condition and loop.
        pub(in crate::tools::background) fn wait(
            &self,
            fd: RawFd,
            events: libc::c_short,
            end: Option<Instant>,
        ) {
            let timeout = match end {
                None => -1,
                Some(end) => {
                    let remaining = end.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return;
                    }
                    // Round up so a sub-millisecond remainder still sleeps instead of spinning.
                    let millis = remaining.as_nanos().div_ceil(1_000_000);
                    i32::try_from(millis).unwrap_or(i32::MAX)
                }
            };
            let mut fds = [
                libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: `fds` is a valid array of two `pollfd` entries for the duration of the call.
            unsafe {
                libc::poll(fds.as_mut_ptr(), 2, timeout);
            }
            if fds[1].revents != 0 {
                self.drain();
            }
        }

        fn drain(&self) {
            let mut buf = [0u8; 64];
            loop {
                // SAFETY: `buf` is valid for `buf.len()` bytes and the read end is non-blocking.
                let n = unsafe {
                    libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
                };
                if n <= 0 {
                    return;
                }
            }
        }
    }

    impl Drop for Waker {
        fn drop(&mut self) {
            POLLERS.lock().retain(|&(id, _)| id != self.id);
        }
    }

    fn set_nonblocking(fd: RawFd) -> io::Result<()> {
        // SAFETY: plain fcntl calls on a descriptor owned by the caller.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Notifies waiters when `pid` exits. It waits with `WNOWAIT`, so the zombie is left for the
    /// owner to reap and its PID stays reserved. Must be called while the child is still unreaped.
    pub(in crate::tools::background) fn watch_exit(pid: u32) {
        // A failed spawn drops the closure; output and refresh paths still notify on exit.
        let _ = std::thread::Builder::new()
            .name("dial-bg-exit".into())
            .spawn(move || {
                loop {
                    // SAFETY: `siginfo_t` is plain data; zeroed is a valid initial value.
                    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                    // SAFETY: `info` is a valid out-pointer for the duration of the call.
                    let rc = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            pid as libc::id_t,
                            &mut info,
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    };
                    if rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                        break;
                    }
                }
                notify();
            });
    }
}

#[cfg(windows)]
mod windows {
    use super::notify;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::{INFINITE, OpenProcess, WaitForSingleObject};

    const SYNCHRONIZE: u32 = 0x0010_0000;

    /// Notifies waiters when `pid` exits. The handle is opened here, while the child is still
    /// unreaped, so the PID cannot have been reused; the thread only waits on that handle.
    pub(in crate::tools::background) fn watch_exit(pid: u32) {
        // SAFETY: plain OpenProcess call; the result is checked before use.
        let raw = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            return;
        }
        // SAFETY: `raw` is a freshly opened handle owned by nothing else.
        let owned = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        let _ = std::thread::Builder::new()
            .name("dial-bg-exit".into())
            .spawn(move || {
                // SAFETY: `owned` stays alive for the whole call.
                unsafe { WaitForSingleObject(owned.as_raw_handle() as HANDLE, INFINITE) };
                notify();
            });
    }
}

#[cfg(unix)]
pub(super) use unix::{Waker, watch_exit};
#[cfg(windows)]
pub(super) use windows::watch_exit;
