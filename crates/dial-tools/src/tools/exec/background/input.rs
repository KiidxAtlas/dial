//! Owned, bounded nonblocking input. PTYs have no half-close: canonical EOF uses the terminal's VEOF.
use super::wake;
#[cfg(unix)]
use super::wake::Waker;
use parking_lot::{Mutex, MutexGuard};
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) enum Handle {
    #[cfg(unix)]
    Pipe(io::PipeWriter),
    #[cfg(windows)]
    Pipe(crate::platform::io::QueuedWriter<io::PipeWriter>),
    #[cfg(unix)]
    Terminal(File),
    #[cfg(windows)]
    Terminal(crate::platform::io::QueuedWriter<File>),
}
impl Handle {
    pub(super) fn pipe(writer: io::PipeWriter) -> Self {
        #[cfg(unix)]
        {
            Self::Pipe(writer)
        }
        #[cfg(windows)]
        {
            Self::Pipe(crate::platform::io::QueuedWriter::new(writer, 64 * 1024))
        }
    }
    pub(super) fn terminal_input(writer: File) -> Self {
        #[cfg(unix)]
        {
            Self::Terminal(writer)
        }
        #[cfg(windows)]
        {
            Self::Terminal(crate::platform::io::QueuedWriter::new(writer, 64 * 1024))
        }
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Pipe(file) => file.write(bytes),
            #[cfg(windows)]
            Self::Pipe(file) => file.try_write(bytes),
            #[cfg(unix)]
            Self::Terminal(file) => file.write(bytes),
            #[cfg(windows)]
            Self::Terminal(file) => file.try_write(bytes),
        }
    }
    fn prepare(&self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Pipe(file) => crate::platform::io::set_nonblocking(file),
            #[cfg(windows)]
            Self::Pipe(_) | Self::Terminal(_) => Ok(()),
            #[cfg(unix)]
            Self::Terminal(file) => crate::platform::io::set_nonblocking(file),
        }
    }
    fn drain(&self, timeout: Duration) -> io::Result<bool> {
        match self {
            #[cfg(unix)]
            Self::Pipe(_) | Self::Terminal(_) => {
                let _ = timeout; // Unix writes are synchronous; nothing is queued.
                Ok(true)
            }
            #[cfg(windows)]
            Self::Pipe(writer) => writer.wait_until_drained(timeout),
            #[cfg(windows)]
            Self::Terminal(writer) => writer.wait_until_drained(timeout),
        }
    }
}
#[cfg(unix)]
pub(super) fn nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(unix)]
impl AsRawFd for Handle {
    fn as_raw_fd(&self) -> i32 {
        match self {
            Self::Pipe(file) => file.as_raw_fd(),
            Self::Terminal(file) => file.as_raw_fd(),
        }
    }
}

#[cfg(unix)]
pub(super) fn terminal(rows: u16, cols: u16) -> Result<(File, File), String> {
    crate::platform::terminal::open(rows, cols).map_err(|e| format!("opening PTY: {e}"))
}

pub(super) struct Input {
    handle: Mutex<Option<Handle>>,
    stopped: std::sync::Arc<AtomicBool>,
}
impl Input {
    pub(super) fn new(handle: Handle, stopped: std::sync::Arc<AtomicBool>) -> Result<Self, String> {
        handle
            .prepare()
            .map_err(|e| format!("configuring background stdin: {e}"))?;
        Ok(Self {
            handle: Mutex::new(Some(handle)),
            stopped,
        })
    }

    fn interrupted(
        &self,
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
        written: usize,
    ) -> Result<(), String> {
        let reason = if cancel.load(Ordering::Acquire) || interrupt.load(Ordering::Acquire) {
            Some("cancelled")
        } else if self.stopped.load(Ordering::Acquire) {
            Some("process stopped")
        } else if end.is_some_and(|end| Instant::now() >= end) {
            Some("deadline exceeded")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(format!(
                "background input {reason}; {written} bytes written (not rolled back)"
            )),
            None => Ok(()),
        }
    }

    /// Takes the slot once its current holder releases it. Every holder notifies on release, and
    /// cancellation notifies too, so the wait ends on the kernel-side condvar rather than polling.
    fn lock_slot(
        &self,
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
    ) -> Result<MutexGuard<'_, Option<Handle>>, String> {
        loop {
            let seen = wake::generation();
            if let Some(slot) = self.handle.try_lock() {
                return Ok(slot);
            }
            self.interrupted(end, cancel, interrupt, 0)?;
            wake::wait_past(seen, end);
        }
    }

    /// Blocks until `handle` can accept more bytes, cancellation, or the deadline. Unix waits on
    /// `poll(POLLOUT)` with the wake pipe registered before the flags are checked.
    fn wait_writable(
        &self,
        handle: &Handle,
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
        written: usize,
    ) -> Result<(), String> {
        #[cfg(unix)]
        {
            let waker = Waker::new()
                .map_err(|e| format!("writing child stdin: {e}; {written} bytes written"))?;
            self.interrupted(end, cancel, interrupt, written)?;
            waker.wait(handle.as_raw_fd(), libc::POLLOUT, end);
        }
        #[cfg(windows)]
        {
            let _ = handle;
            let seen = wake::generation();
            self.interrupted(end, cancel, interrupt, written)?;
            wake::wait_past(seen, end);
        }
        Ok(())
    }

    pub(super) fn write(
        &self,
        bytes: &[u8],
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
    ) -> Result<usize, String> {
        let result = self.write_locked(bytes, end, cancel, interrupt);
        wake::notify();
        result
    }

    fn write_locked(
        &self,
        bytes: &[u8],
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
    ) -> Result<usize, String> {
        let mut slot = self.lock_slot(end, cancel, interrupt)?;
        let handle = slot.as_mut().ok_or("background stdin is closed")?;
        let mut written = 0;
        while written < bytes.len() {
            self.interrupted(end, cancel, interrupt, written)?;
            match handle.write(&bytes[written..]) {
                Ok(0) => return Err(format!("child closed stdin; {written} bytes written")),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.wait_writable(handle, end, cancel, interrupt, written)?
                }
                Err(e) => return Err(format!("writing child stdin: {e}; {written} bytes written")),
            }
        }
        loop {
            self.interrupted(end, cancel, interrupt, written)?;
            if handle
                .drain(Duration::from_millis(10))
                .map_err(|e| format!("writing child stdin: {e}"))?
            {
                break;
            }
        }
        Ok(written)
    }

    pub(super) fn close(
        &self,
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
    ) -> Result<&'static str, String> {
        let result = self.close_locked(end, cancel, interrupt);
        wake::notify();
        result
    }

    fn close_locked(
        &self,
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
    ) -> Result<&'static str, String> {
        let mut slot = self.lock_slot(end, cancel, interrupt)?;
        let handle = slot.as_mut().ok_or("background stdin is already closed")?;
        #[cfg(unix)]
        if let Handle::Terminal(file) = handle {
            let mut term: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(file.as_raw_fd(), &mut term) } == -1 {
                return Err(format!(
                    "reading PTY EOF settings: {}",
                    io::Error::last_os_error()
                ));
            }
            let disabled = unsafe { libc::fpathconf(file.as_raw_fd(), libc::_PC_VDISABLE) };
            if term.c_lflag & libc::ICANON == 0
                || disabled < 0
                || term.c_cc[libc::VEOF] as libc::c_long == disabled
            {
                return Err("PTY has no half-close; close_stdin requires canonical mode with VEOF enabled (raw-mode programs must use their own exit protocol or kill)".into());
            }
            let bytes = [term.c_cc[libc::VEOF]; 2];
            let mut written = 0;
            while written < bytes.len() {
                self.interrupted(end, cancel, interrupt, written)?;
                match handle.write(&bytes[written..]) {
                    Ok(0) => return Err("child closed PTY input".into()),
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.wait_writable(handle, end, cancel, interrupt, written)?
                    }
                    Err(e) => return Err(format!("sending PTY EOF: {e}")),
                }
            }
            slot.take();
            return Ok(
                "canonical PTY EOF sent; further managed input disabled (output remains open)",
            );
        }
        #[cfg(windows)]
        if matches!(handle, Handle::Terminal(_)) {
            return Err("Windows ConPTY has no supported half-close operation".into());
        }
        loop {
            self.interrupted(end, cancel, interrupt, 0)?;
            if handle
                .drain(Duration::from_millis(10))
                .map_err(|e| format!("closing child stdin: {e}"))?
            {
                break;
            }
        }
        slot.take();
        Ok("stdin pipe closed; child receives EOF after buffered input")
    }

    pub(super) fn resize(
        &self,
        rows: u16,
        cols: u16,
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
    ) -> Result<(), String> {
        let result = self.resize_locked(rows, cols, end, cancel, interrupt);
        wake::notify();
        result
    }

    fn resize_locked(
        &self,
        rows: u16,
        cols: u16,
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
    ) -> Result<(), String> {
        #[cfg(windows)]
        {
            let _ = (rows, cols, end, cancel, interrupt);
            Err("ConPTY resize must be performed by the owning native PTY process".into())
        }
        #[cfg(unix)]
        {
            let slot = self.lock_slot(end, cancel, interrupt)?;
            let Some(Handle::Terminal(file)) = slot.as_ref() else {
                return Err("resize requires an open PTY input".into());
            };
            let size = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            if unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
                return Err(format!("resizing PTY: {}", io::Error::last_os_error()));
            }
            Ok(())
        }
    }
}
